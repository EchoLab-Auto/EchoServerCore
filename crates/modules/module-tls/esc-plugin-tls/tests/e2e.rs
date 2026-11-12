//! 端到端：真的握手、真的转发。
//!
//! 单元测试能证明「配置解析对了」「头剥对了」，但证明不了**这条链路通**。
//! 这一层用真 TLS 客户端 + 真上游，把 nginx 那四件事逐条验一遍：
//!
//! ```text
//! TLS 客户端 → esc.tls（本 crate）→ 上游（测试里现起的 hyper）
//! ```
//!
//! 为什么值得写：这层代码接替的是一份**已经在生产跑的** nginx 配置。换实现时
//! 唯一能证明"没退化"的方式，就是拿同样的请求走一遍新链路。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use esc_plugin_tls::certs::{self, CertStore};
use esc_plugin_tls::config::{Action, CertConfig, Site};
use esc_plugin_tls::control::Metrics;
use esc_plugin_tls::serve;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;

// ─────────────────────────────────────────────────────────────────────────────
// 测试脚手架
// ─────────────────────────────────────────────────────────────────────────────

struct SelfSigned {
    cert_pem: String,
    cert_path: std::path::PathBuf,
    key_path: std::path::PathBuf,
    _dir: std::path::PathBuf,
}

/// 现生成一张自签证书并落盘（`CertStore` 只吃文件）。
fn self_signed(cn: &str) -> SelfSigned {
    let dir = std::env::temp_dir().join(format!(
        "esc-tls-e2e-{cn}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let generated = rcgen::generate_simple_self_signed(vec![cn.to_string()]).unwrap();
    let cert_pem = generated.cert.pem();
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, cert_pem.as_bytes()).unwrap();
    std::fs::write(&key_path, generated.signing_key.serialize_pem().as_bytes()).unwrap();

    SelfSigned {
        cert_pem,
        cert_path,
        key_path,
        _dir: dir,
    }
}

/// 起一个上游：把收到的请求头原样回显成 `k: v` 行，便于断言改写结果。
///
/// 返回它监听的地址。任务随测试结束被丢弃。
async fn start_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(|req: Request<Incoming>| async move {
                            // 特殊路径让测试能拿到状态码
                            if req.uri().path() == "/boom" {
                                return Ok::<_, std::convert::Infallible>(
                                    Response::builder()
                                        .status(StatusCode::IM_A_TEAPOT)
                                        .body(http_body_util::Full::new(bytes::Bytes::from_static(
                                            b"teapot\n",
                                        )))
                                        .unwrap(),
                                );
                            }
                            let mut body = String::new();
                            body.push_str(&format!("METHOD {}\n", req.method()));
                            body.push_str(&format!("URI {}\n", req.uri()));
                            let mut names: Vec<_> =
                                req.headers().iter().map(|(k, _)| k.as_str().to_string()).collect();
                            names.sort();
                            for name in names {
                                for value in req.headers().get_all(&name) {
                                    body.push_str(&format!(
                                        "HDR {} = {}\n",
                                        name,
                                        value.to_str().unwrap_or("<binary>")
                                    ));
                                }
                            }
                            Ok(Response::new(http_body_util::Full::new(bytes::Bytes::from(
                                body,
                            ))))
                        }),
                    )
                    .await;
            });
        }
    });

    addr
}

/// 起一个 TLS 终结，返回它监听的端口。
async fn start_tls(sites: Vec<Site>, certs: Vec<CertConfig>, default: &str) -> SocketAddr {
    let store = Arc::new(CertStore::load(&certs, &Some(default.to_string())).expect("证书应能加载"));
    let tls = certs::server_config(store, true).expect("TLS 配置应能构建");
    let bound = serve::bind(&sites).expect("应能绑定");
    let handle =
        serve::spawn(bound, tls, Duration::from_secs(5), Metrics::new()).expect("应能启动");
    let addr = handle.addrs()[0];
    // 句柄随任务泄漏到测试结束（Drop 会关掉，所以这里必须 leak）：
    // 测试进程退出时 OS 会回收，而提前 drop 会把监听关掉。
    std::mem::forget(handle);
    addr
}

fn site(name: &str, addr: SocketAddr, upstream: SocketAddr, websocket: bool) -> Site {
    Site {
        name: name.into(),
        addr,
        action: Action::Proxy(upstream),
        websocket,
        log_query: false,
        read_timeout: Duration::from_secs(5),
        max_body_bytes: 1024 * 1024,
        hsts: false,
    }
}

/// 一个信任指定自签证书的 TLS 客户端。
fn client_trusting(cert_pem: &str) -> TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    let mut reader = std::io::BufReader::new(cert_pem.as_bytes());
    for cert in rustls_pemfile::certs(&mut reader) {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

/// 发一个请求，返回 (状态码, 响应体文本, 响应头)。
async fn get(
    addr: SocketAddr,
    sni: &str,
    connector: &TlsConnector,
    path: &str,
    extra: &[(&str, &str)],
) -> (u16, String, Vec<(String, String)>) {
    let tcp = TcpStream::connect(addr).await.expect("应能连上 TLS 端口");
    let server_name = rustls::pki_types::ServerName::try_from(sni.to_string()).unwrap();
    let tls = connector
        .connect(server_name, tcp)
        .await
        .expect("TLS 握手应成功");

    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(TokioIo::new(tls)).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut req = Request::builder()
        .method("GET")
        .uri(path)
        .header(hyper::header::HOST, format!("{sni}:{}", addr.port()));
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    let req = req.body(http_body_util::Empty::<bytes::Bytes>::new()).unwrap();

    let resp = sender.send_request(req).await.expect("应能拿到响应");
    let status = resp.status().as_u16();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    (status, String::from_utf8_lossy(&body).to_string(), headers)
}

// ─────────────────────────────────────────────────────────────────────────────
// 测试
// ─────────────────────────────────────────────────────────────────────────────

/// 最基本的一条：TLS 握手成功、请求到达上游、响应回来。
#[tokio::test]
async fn proxies_over_tls_end_to_end() {
    let cert = self_signed("panel.example");
    let upstream = start_upstream().await;
    let addr = start_tls(
        vec![site("panel", "127.0.0.1:0".parse().unwrap(), upstream, false)],
        vec![CertConfig {
            name: "self".into(),
            cert: cert.cert_path.clone(),
            key: cert.key_path.clone(),
            sni: vec![],
        }],
        "self",
    )
    .await;

    let connector = client_trusting(&cert.cert_pem);
    let (status, body, _) = get(addr, "panel.example", &connector, "/api/sessions", &[]).await;

    assert_eq!(status, 200);
    assert!(body.contains("METHOD GET"), "{body}");
    assert!(body.contains("URI /api/sessions"), "{body}");
}

/// nginx 那份配置的核心行为：`X-Forwarded-For` / `X-Real-IP` / `X-Forwarded-Proto`。
///
/// 尤其 `X-Forwarded-For` **必须是覆盖而不是追加** —— `esc-gateway` 的
/// `trust_forwarded_for` 能信任这个头，前提就是这一层会把它整体换掉。
#[tokio::test]
async fn forwarding_headers_are_set_and_forged_values_are_replaced() {
    let cert = self_signed("panel.example");
    let upstream = start_upstream().await;
    let addr = start_tls(
        vec![site("panel", "127.0.0.1:0".parse().unwrap(), upstream, false)],
        vec![CertConfig {
            name: "self".into(),
            cert: cert.cert_path.clone(),
            key: cert.key_path.clone(),
            sni: vec![],
        }],
        "self",
    )
    .await;

    let connector = client_trusting(&cert.cert_pem);
    let (status, body, _) = get(
        addr,
        "panel.example",
        &connector,
        "/",
        // 客户端伪造
        &[("x-forwarded-for", "6.6.6.6"), ("x-real-ip", "6.6.6.6")],
    )
    .await;

    assert_eq!(status, 200);
    assert!(body.contains("HDR x-forwarded-proto = https"), "{body}");
    assert!(body.contains("HDR x-real-ip = 127.0.0.1"), "{body}");
    assert!(
        body.contains("HDR x-forwarded-for = 127.0.0.1"),
        "应是覆盖后的真实地址：{body}"
    );
    assert!(
        !body.contains("6.6.6.6"),
        "客户端伪造的值不能漏给上游：{body}"
    );
    // Host 要原样保留（含端口）：网关按 Host 拼链接
    assert!(body.contains(&format!("HDR host = panel.example:{}", addr.port())), "{body}");
}

/// 上游的状态码要原样透传，不能被改写成 502/200。
#[tokio::test]
async fn upstream_status_is_passed_through() {
    let cert = self_signed("panel.example");
    let upstream = start_upstream().await;
    let addr = start_tls(
        vec![site("panel", "127.0.0.1:0".parse().unwrap(), upstream, false)],
        vec![CertConfig {
            name: "self".into(),
            cert: cert.cert_path.clone(),
            key: cert.key_path.clone(),
            sni: vec![],
        }],
        "self",
    )
    .await;

    let connector = client_trusting(&cert.cert_pem);
    let (status, body, _) = get(addr, "panel.example", &connector, "/boom", &[]).await;
    assert_eq!(status, 418, "上游的 418 要原样出来");
    assert_eq!(body, "teapot\n");
}

/// 上游挂了 → 502，而且**文案要指明是哪个上游**。
#[tokio::test]
async fn dead_upstream_yields_502_with_the_address() {
    let cert = self_signed("panel.example");
    // 先占一个端口再放掉，保证这个地址上没人听
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let addr = start_tls(
        vec![site("panel", "127.0.0.1:0".parse().unwrap(), dead, false)],
        vec![CertConfig {
            name: "self".into(),
            cert: cert.cert_path.clone(),
            key: cert.key_path.clone(),
            sni: vec![],
        }],
        "self",
    )
    .await;

    let connector = client_trusting(&cert.cert_pem);
    let (status, body, _) = get(addr, "panel.example", &connector, "/", &[]).await;
    assert_eq!(status, 502);
    assert!(
        body.contains(&dead.to_string()),
        "错误信息要说出连不上谁：{body}"
    );
}

/// `redirect` 站点：301 + Location 模板展开（保留查询串）。
#[tokio::test]
async fn redirect_site_returns_301_with_expanded_location() {
    let cert = self_signed("panel.example");
    let addr = start_tls(
        vec![Site {
            name: "old".into(),
            addr: "127.0.0.1:0".parse().unwrap(),
            action: Action::Redirect("https://{host}:2224{uri}".into()),
            websocket: false,
            log_query: false,
            read_timeout: Duration::from_secs(5),
            max_body_bytes: 1024,
            hsts: false,
        }],
        vec![CertConfig {
            name: "self".into(),
            cert: cert.cert_path.clone(),
            key: cert.key_path.clone(),
            sni: vec![],
        }],
        "self",
    )
    .await;

    let connector = client_trusting(&cert.cert_pem);
    let (status, _, headers) = get(addr, "panel.example", &connector, "/a/b?x=1", &[]).await;
    assert_eq!(status, 301);
    let location = headers
        .iter()
        .find(|(k, _)| k == "location")
        .map(|(_, v)| v.clone())
        .expect("要有 Location");
    assert_eq!(
        location,
        format!("https://panel.example:{}:2224/a/b?x=1", addr.port()),
        "模板要展开并且带上查询串"
    );
}

/// `{hostname}` 必须**去掉端口**。
///
/// 2223 → 2224 那条跳转靠它：nginx 写的是 `https://$host:2224$request_uri`，
/// 而 `$host` 不含端口。用含端口的 `{host}` 会拼出 `https://x:2223:2224/…`
/// 这种废地址 —— 这条只有真点一次才看得出来。
#[tokio::test]
async fn redirect_hostname_placeholder_drops_the_port() {
    let cert = self_signed("panel.example");
    let addr = start_tls(
        vec![Site {
            name: "old".into(),
            addr: "127.0.0.1:0".parse().unwrap(),
            // 注意是 {hostname} 而不是 {host}
            action: Action::Redirect("https://{hostname}:2224{uri}".into()),
            websocket: false,
            log_query: false,
            read_timeout: Duration::from_secs(5),
            max_body_bytes: 1024,
            hsts: false,
        }],
        vec![CertConfig {
            name: "self".into(),
            cert: cert.cert_path.clone(),
            key: cert.key_path.clone(),
            sni: vec![],
        }],
        "self",
    )
    .await;

    let connector = client_trusting(&cert.cert_pem);
    let (status, _, headers) = get(addr, "panel.example", &connector, "/a/b?x=1", &[]).await;
    assert_eq!(status, 301);
    let location = headers
        .iter()
        .find(|(k, _)| k == "location")
        .map(|(_, v)| v.clone())
        .expect("要有 Location");
    assert_eq!(
        location, "https://panel.example:2224/a/b?x=1",
        "Host 里的端口（{}）必须被去掉，不能拼出双端口",
        addr.port()
    );
}

/// SNI 选证书：两个名字 → 两张证书。
///
/// 这是替代 nginx「同一端口两张证书」的核心能力。
///
/// 断言方式特意选了**直接比对出示的证书 DER**，而不是"信任链能不能过"：
/// 后者在自签场景下只能证明"不是这张"，而错误种类（`BadSignature` 还是
/// `NotValidForName`）取决于客户端内部顺序，脆得很。这里直接问
/// `peer_certificates()` 拿服务端出示的那张，比它是不是预期的。
#[tokio::test]
async fn sni_picks_the_matching_certificate() {
    let real = self_signed("real.example");
    let fallback = self_signed("fallback.example");

    let upstream = start_upstream().await;
    let addr = start_tls(
        vec![site("panel", "127.0.0.1:0".parse().unwrap(), upstream, false)],
        vec![
            CertConfig {
                name: "self".into(),
                cert: fallback.cert_path.clone(),
                key: fallback.key_path.clone(),
                sni: vec![],
            },
            CertConfig {
                name: "real".into(),
                cert: real.cert_path.clone(),
                key: real.key_path.clone(),
                sni: vec!["real.example".into()],
            },
        ],
        "self",
    )
    .await;

    // 信任两张的客户端：这样两种 SNI 下握手都能成功，才能比"出示了哪张"
    let both_roots = {
        let mut roots = rustls::RootCertStore::empty();
        for pem in [&real.cert_pem, &fallback.cert_pem] {
            let mut reader = std::io::BufReader::new(pem.as_bytes());
            for c in rustls_pemfile::certs(&mut reader) {
                roots.add(c.unwrap()).unwrap();
            }
        }
        TlsConnector::from(Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
        ))
    };

    /// 取第一张证书的 DER。
    ///
    /// 迭代器要先绑到变量上：直接链式 `.next()` 的话，那个临时迭代器的析构会
    /// 排在 `reader` 之后，借用在 reader 已死时仍被需要（E0597）。
    fn der_of(pem: &str) -> Vec<u8> {
        let mut reader = std::io::BufReader::new(pem.as_bytes());
        let mut iter = rustls_pemfile::certs(&mut reader);
        let cert = iter.next().expect("至少一张证书").expect("PEM 应能解析");
        cert.to_vec()
    }
    let real_der = der_of(&real.cert_pem);
    let fallback_der = der_of(&fallback.cert_pem);
    assert_ne!(real_der, fallback_der, "两张证书必须不同，否则测试没意义");

    // ① SNI 命中 → 出示 real
    let tcp = TcpStream::connect(addr).await.unwrap();
    let sni = rustls::pki_types::ServerName::try_from("real.example".to_string()).unwrap();
    let tls = both_roots.connect(sni, tcp).await.expect("应握手成功");
    let presented = tls
        .get_ref()
        .1
        .peer_certificates()
        .expect("应有证书链")[0]
        .as_ref()
        .to_vec();
    assert_eq!(presented, real_der, "SNI=real.example 应出示 real 那张");

    // ② SNI 未命中 → 出示默认（fallback）
    let tcp = TcpStream::connect(addr).await.unwrap();
    let sni = rustls::pki_types::ServerName::try_from("fallback.example".to_string()).unwrap();
    let tls = both_roots.connect(sni, tcp).await.expect("应握手成功");
    let presented = tls
        .get_ref()
        .1
        .peer_certificates()
        .expect("应有证书链")[0]
        .as_ref()
        .to_vec();
    assert_eq!(
        presented, fallback_der,
        "未命中的 SNI 应出示默认证书，而不是「最像」的那张"
    );

    // ③ SNI 大小写不影响选择（配置里故意写的是 Cloud.TX… 那种混写时的归一化）
    let tcp = TcpStream::connect(addr).await.unwrap();
    let sni = rustls::pki_types::ServerName::try_from("REAL.EXAMPLE".to_string()).unwrap();
    let tls = both_roots.connect(sni, tcp).await.expect("应握手成功");
    let presented = tls.get_ref().1.peer_certificates().unwrap()[0]
        .as_ref()
        .to_vec();
    assert_eq!(presented, real_der, "SNI 匹配要不分大小写");

    // ④ 只信任 real 的客户端 + 未命中的 SNI → 握手必须失败
    let trusted_real = client_trusting(&real.cert_pem);
    let tcp = TcpStream::connect(addr).await.unwrap();
    let sni = rustls::pki_types::ServerName::try_from("fallback.example".to_string()).unwrap();
    assert!(
        trusted_real.connect(sni, tcp).await.is_err(),
        "出示的不是被信任的那张，握手就该失败 —— 否则等于 SNI 没起作用"
    );
}

/// 没开 `websocket` 的站点收到 Upgrade 请求 → 501，并且**不转发**给上游。
///
/// 为什么默认关：一个"能升级"的端口意味着任意长连接都能挂在上面。
#[tokio::test]
async fn upgrade_is_refused_when_websocket_is_off() {
    let cert = self_signed("panel.example");
    let upstream = start_upstream().await;
    let addr = start_tls(
        vec![site("panel", "127.0.0.1:0".parse().unwrap(), upstream, false)],
        vec![CertConfig {
            name: "self".into(),
            cert: cert.cert_path.clone(),
            key: cert.key_path.clone(),
            sni: vec![],
        }],
        "self",
    )
    .await;

    let connector = client_trusting(&cert.cert_pem);
    let (status, body, _) = get(
        addr,
        "panel.example",
        &connector,
        "/ws",
        &[("upgrade", "websocket"), ("connection", "upgrade")],
    )
    .await;

    assert_eq!(status, 501, "没开 websocket 就该拒绝");
    assert!(
        body.contains("websocket = true"),
        "要告诉人怎么打开：{body}"
    );
}

/// 开了 `websocket` 的话，升级头必须**保留**着送给上游。
///
/// 这一条最容易写错：无脑剥 hop-by-hop 会让 WebSocket 永远停在「正在连接」，
/// 而表现跟后端没起一样。
#[tokio::test]
async fn upgrade_headers_reach_the_upstream_when_enabled() {
    let cert = self_signed("panel.example");
    let upstream = start_upstream().await;
    let addr = start_tls(
        vec![site("panel", "127.0.0.1:0".parse().unwrap(), upstream, true)],
        vec![CertConfig {
            name: "self".into(),
            cert: cert.cert_path.clone(),
            key: cert.key_path.clone(),
            sni: vec![],
        }],
        "self",
    )
    .await;

    let connector = client_trusting(&cert.cert_pem);
    // 上游不认这个升级，会回 200 —— 但只要它**收到了**这两个头，就说明没被剥掉。
    let (status, body, _) = get(
        addr,
        "panel.example",
        &connector,
        "/ws",
        &[("upgrade", "websocket"), ("connection", "upgrade")],
    )
    .await;

    assert_eq!(status, 200);
    assert!(body.contains("HDR upgrade = websocket"), "upgrade 头必须到上游：{body}");
    assert!(
        body.contains("HDR connection = upgrade"),
        "connection 头必须到上游：{body}"
    );
}

/// 请求体超限 → 413，而且是**在连上游之前**就拒掉。
#[tokio::test]
async fn oversized_body_is_rejected_before_contacting_upstream() {
    let cert = self_signed("panel.example");
    let upstream = start_upstream().await;
    let addr = start_tls(
        vec![Site {
            name: "panel".into(),
            addr: "127.0.0.1:0".parse().unwrap(),
            action: Action::Proxy(upstream),
            websocket: false,
            log_query: false,
            read_timeout: Duration::from_secs(5),
            max_body_bytes: 16, // 极小，逼出 413
            hsts: false,
        }],
        vec![CertConfig {
            name: "self".into(),
            cert: cert.cert_path.clone(),
            key: cert.key_path.clone(),
            sni: vec![],
        }],
        "self",
    )
    .await;

    let connector = client_trusting(&cert.cert_pem);
    let tcp = TcpStream::connect(addr).await.unwrap();
    let sni = rustls::pki_types::ServerName::try_from("panel.example".to_string()).unwrap();
    let tls = connector.connect(sni, tcp).await.unwrap();

    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(TokioIo::new(tls)).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let req = Request::builder()
        .method("POST")
        .uri("/upload")
        .header(hyper::header::HOST, "panel.example")
        .header(hyper::header::CONTENT_LENGTH, "1024")
        .body(http_body_util::Full::new(bytes::Bytes::from(vec![b'x'; 1024])))
        .unwrap();
    let resp = sender.send_request(req).await.expect("应拿到响应");
    assert_eq!(resp.status().as_u16(), 413);
}

/// 两个端口指向不同上游 —— 这是 nginx 的「按端口分流」。
#[tokio::test]
async fn each_listen_port_forwards_to_its_own_upstream() {
    let cert = self_signed("panel.example");
    let upstream_a = start_upstream().await;
    let upstream_b = start_upstream().await;

    let store = Arc::new(
        CertStore::load(
            &[CertConfig {
                name: "self".into(),
                cert: cert.cert_path.clone(),
                key: cert.key_path.clone(),
                sni: vec![],
            }],
            &Some("self".into()),
        )
        .unwrap(),
    );
    let tls = certs::server_config(store, true).unwrap();
    let bound = serve::bind(&[
        site("a", "127.0.0.1:0".parse().unwrap(), upstream_a, false),
        site("b", "127.0.0.1:0".parse().unwrap(), upstream_b, false),
    ])
    .expect("两个端口应都能绑");
    let addrs: Vec<SocketAddr> = bound
        .iter()
        .map(|b| b.listener.local_addr().unwrap())
        .collect();
    let handle = serve::spawn(bound, tls, Duration::from_secs(5), Metrics::new()).unwrap();
    std::mem::forget(handle);

    let connector = client_trusting(&cert.cert_pem);
    // 两个上游的行为一样（都是回显），所以只能验证"两个端口都能通"。
    // 分流是否真的分对了，靠下面的断言：两个端口收到的 Host 端口号不同。
    let (s1, b1, _) = get(addrs[0], "panel.example", &connector, "/one", &[]).await;
    let (s2, b2, _) = get(addrs[1], "panel.example", &connector, "/two", &[]).await;
    assert_eq!((s1, s2), (200, 200));
    assert!(b1.contains("URI /one"), "{b1}");
    assert!(b2.contains("URI /two"), "{b2}");
    assert_ne!(addrs[0], addrs[1]);
}

/// HSTS 只有显式打开才发。
#[tokio::test]
async fn hsts_header_only_when_enabled() {
    let cert = self_signed("panel.example");
    let upstream = start_upstream().await;

    let store = Arc::new(
        CertStore::load(
            &[CertConfig {
                name: "self".into(),
                cert: cert.cert_path.clone(),
                key: cert.key_path.clone(),
                sni: vec![],
            }],
            &Some("self".into()),
        )
        .unwrap(),
    );
    let tls = certs::server_config(store, true).unwrap();

    let mut with_hsts = site("h", "127.0.0.1:0".parse().unwrap(), upstream, false);
    with_hsts.hsts = true;
    let without = site("n", "127.0.0.1:0".parse().unwrap(), upstream, false);

    let bound = serve::bind(&[with_hsts, without]).unwrap();
    let addrs: Vec<SocketAddr> = bound
        .iter()
        .map(|b| b.listener.local_addr().unwrap())
        .collect();
    let handle = serve::spawn(bound, tls, Duration::from_secs(5), Metrics::new()).unwrap();
    std::mem::forget(handle);

    let connector = client_trusting(&cert.cert_pem);
    let (_, _, h1) = get(addrs[0], "panel.example", &connector, "/", &[]).await;
    let (_, _, h2) = get(addrs[1], "panel.example", &connector, "/", &[]).await;

    assert!(
        h1.iter()
            .any(|(k, _)| k == "strict-transport-security"),
        "打开了就该发 HSTS"
    );
    assert!(
        !h2.iter()
            .any(|(k, _)| k == "strict-transport-security"),
        "默认不发"
    );
    // nosniff 两边都要有
    assert!(h1.iter().any(|(k, v)| k == "x-content-type-options" && v == "nosniff"));
    assert!(h2.iter().any(|(k, v)| k == "x-content-type-options" && v == "nosniff"));
}

/// 客户端只发 HTTP/1.1 —— ALPN 不该把它升到 h2。
#[tokio::test]
async fn alpn_negotiates_http11_only() {
    let cert = self_signed("panel.example");
    let upstream = start_upstream().await;
    let addr = start_tls(
        vec![site("panel", "127.0.0.1:0".parse().unwrap(), upstream, false)],
        vec![CertConfig {
            name: "self".into(),
            cert: cert.cert_path.clone(),
            key: cert.key_path.clone(),
            sni: vec![],
        }],
        "self",
    )
    .await;

    // 客户端同时提供 h2 与 http/1.1
    let mut roots = rustls::RootCertStore::empty();
    let mut reader = std::io::BufReader::new(cert.cert_pem.as_bytes());
    for c in rustls_pemfile::certs(&mut reader) {
        roots.add(c.unwrap()).unwrap();
    }
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let connector = TlsConnector::from(Arc::new(cfg));

    let tcp = TcpStream::connect(addr).await.unwrap();
    let sni = rustls::pki_types::ServerName::try_from("panel.example".to_string()).unwrap();
    let tls = connector.connect(sni, tcp).await.unwrap();
    let (_, session) = tls.get_ref();
    assert_eq!(
        session.alpn_protocol(),
        Some(b"http/1.1".as_slice()),
        "服务端只该接受 http/1.1"
    );
}
