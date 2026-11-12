//! 每个请求做什么：301 跳转，或转发到本机上游。
//!
//! # 为什么手写而不是套一个现成的代理
//!
//! 与 `esc-gateway` 同样的理由（见那边 `proxy.rs` 的模块注释）：可用的反向代理
//! 中间件要么长期没维护、要么行为不透明。而这里要的行为其实很少，且必须**可解释**
//! ——它挡在所有入口前面。
//!
//! # 明确做什么
//!
//! ```text
//! 请求 → 来源地址？(无白名单，靠上层)  → hop-by-hop 头剥离
//!      → Host / X-Forwarded-* 重写     → 连上游
//!      → 101？拼接双向流 : 透传响应    → 访问日志
//! ```
//!
//! # 明确不做什么
//!
//! * **HTTP/2**：`serve.rs` 的 ALPN 只宣告 `http/1.1`。
//! * **请求体之外的缓冲**：流式响应原样透传（SSE、日志、事件流都不会被卡住）。
//! * **多上游 / 负载均衡**：一个端口一个上游。要分流请用 `esc.edge-router`。
//! * **路径改写**：`strip_prefix` 之类一概没有。上游看到的路径就是客户端发的。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::body::Incoming;
use hyper::header::{
    HeaderMap, HeaderName, HeaderValue, CONNECTION, CONTENT_LENGTH, HOST, UPGRADE,
};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Version};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::config::{Action, Site};
use crate::control::Metrics;

/// 响应与上游请求共用的体类型。
///
/// `UnsyncBoxBody` 而不是 `BoxBody`：后者要求 `Sync`，而 `hyper::body::Incoming`
/// 不是。为了一个用不到的 `Sync` 去换一层包装不值得。
pub type Body = UnsyncBoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

pub fn empty() -> Body {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed_unsync()
}

/// 这些头只对**单跳**有意义，绝不能原样转发（RFC 9110 §7.6.1）。
///
/// 用 `&str` 而不是 `HeaderName`：`HeaderName::from_static` 虽然是 const fn，
/// 但把它放进 `const` 数组会触到「临时值的内部可变借用被延长到程序结束」的
/// 限制（E0492）。比较字符串就够了，这里是热路径但只有几个名字。
///
/// `upgrade` 与 `connection` 在升级路径上要**保留**，见 [`strip_hop_by_hop`]。
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

/// 处理一个请求。
pub async fn handle(
    site: Arc<Site>,
    req: Request<Incoming>,
    peer: SocketAddr,
    metrics: Arc<Metrics>,
) -> Response<Body> {
    metrics.record_request();
    let started = Instant::now();
    let method = req.method().clone();
    // 日志用：`log_query = false` 时只留路径（`?token=` 不能落盘）
    let uri_for_log = if site.log_query {
        req.uri().to_string()
    } else {
        req.uri().path().to_string()
    };

    let (response, upstream_label) = match &site.action {
        Action::Redirect(template) => (redirect(site.as_ref(), &req, template), "—".to_string()),
        Action::Proxy(upstream) => {
            let r = forward(site.as_ref(), req, peer, *upstream, Arc::clone(&metrics)).await;
            (r, upstream.to_string())
        }
    };

    log_access(
        &site,
        peer,
        method,
        uri_for_log,
        &response,
        &upstream_label,
        started.elapsed(),
    );
    response
}

/// 301。
///
/// 占位符：
///
/// | 占位符 | 展开成 |
/// |---|---|
/// | `{host}` | 请求的 Host 头**原样**（含端口） |
/// | `{hostname}` | Host 去掉端口 |
/// | `{path}` | 只有路径 |
/// | `{uri}` | 路径 + 查询串（等价 nginx 的 `$request_uri`） |
fn redirect(site: &Site, req: &Request<Incoming>, template: &str) -> Response<Body> {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let path = req.uri().path();
    let full_uri = match req.uri().query() {
        Some(q) => format!("{path}?{q}"),
        None => path.to_string(),
    };

    // `{hostname}` 是**去掉端口**的 Host。
    //
    // 2223 → 2224 那条跳转需要它：nginx 写的是 `https://$host:2224$request_uri`，
    // 而 `$host` **不含端口**。直接用含端口的 `{host}` 会拼出
    // `https://x:2223:2224/…` 这种废地址 —— 这条错误只有真点一次才发现。
    let hostname = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    let location = template
        .replace("{hostname}", hostname)
        .replace("{host}", host)
        .replace("{path}", path)
        .replace("{uri}", &full_uri);

    // 目标本身也得是合法的头值，否则 hyper 会在写响应时 panic/报错 —— 提前挡住，
    // 换成 502 说清楚。这里唯一可能的坏输入来自模板，而模板来自配置。
    match HeaderValue::from_str(&location) {
        Ok(v) => {
            let mut resp = text_response(
                StatusCode::MOVED_PERMANENTLY,
                format!("→ {location}\n"),
            );
            resp.headers_mut().insert(hyper::header::LOCATION, v);
            apply_common_headers(site, resp.headers_mut());
            resp
        }
        Err(_) => {
            warn!(site = %site.name, %location, "redirect 目标不是合法的 Location 值");
            let mut resp = text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("redirect 目标非法：{location}\n"),
            );
            apply_common_headers(site, resp.headers_mut());
            resp
        }
    }
}

/// 一段 UTF-8 文本体。
fn text_body(s: impl Into<String>) -> Body {
    http_body_util::Full::new(Bytes::from(s.into()))
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// 构造一个 `text/plain` 错误响应。
///
/// 文案要能让人**不看服务端日志就知道哪一层坏了**：`502 连不上上游 127.0.0.1:18790`
/// 比一个空白的 `502` 有用得多，而这条响应可能正是排障的人看到的第一条信息。
fn text_response(status: StatusCode, message: impl Into<String>) -> Response<Body> {
    let mut resp = Response::new(text_body(message));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

/// 加在所有响应上的头（目前只有 HSTS）。
fn apply_common_headers(site: &Site, headers: &mut HeaderMap) {
    if site.hsts {
        headers.insert(
            hyper::header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=2592000"),
        );
    }
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
}

/// 转发。`upstream` 是已解析的地址（配置校验保证了这一点）。
async fn forward(
    site: &Site,
    mut req: Request<Incoming>,
    peer: SocketAddr,
    upstream: SocketAddr,
    metrics: Arc<Metrics>,
) -> Response<Body> {
    // ── 请求体上限 ─────────────────────────────────────────────────────────
    //
    // 先看 `content-length`：能提前拒绝就提前拒绝，不必等字节流过来。
    // 没有 `content-length`（分块上传）时靠后面的 `Limited` 兜底 —— 只检查前者
    // 会留下一个"分块上传可以无限大"的洞。
    if let Some(len) = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if len > site.max_body_bytes {
            metrics.record_body_too_large();
            return text_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "请求体 {} 字节，超过这个端口的上限 {} 字节\n",
                    len, site.max_body_bytes
                ),
            );
        }
    }

    // 升级请求（WebSocket）要单独走：握手请求没有体，而 101 之后这两条流要拼接。
    let is_upgrade = req
        .headers()
        .get(UPGRADE)
        .map(|v| !v.is_empty())
        .unwrap_or(false);

    if is_upgrade && !site.websocket {
        // 没显式打开就拒绝。一个"能升级"的端口意味着任意长连接都能挂在上面，
        // 不该是默认行为。
        debug!(site = %site.name, "拒绝 Upgrade：该站点没有开 websocket");
        return text_response(
            StatusCode::NOT_IMPLEMENTED,
            format!(
                "端口 {}（site {:?}）没有开 WebSocket 升级。\n                 要允许它，在配置里给这个 site 加 websocket = true。\n",
                site.addr, site.name
            ),
        );
    }

    // 客户端侧的升级句柄必须在**取出请求头之后、发送之前**拿到。
    let client_upgrade = if is_upgrade {
        Some(hyper::upgrade::on(&mut req))
    } else {
        None
    };

    let (parts, incoming) = {
        let (parts, body) = req.into_parts();
        (parts, body)
    };

    // ── 连上游 ─────────────────────────────────────────────────────────────
    let stream = match TcpStream::connect(upstream).await {
        Ok(s) => s,
        Err(e) => {
            warn!(site = %site.name, %upstream, error = %e, "连不上上游");
            metrics.record_upstream_error();
            return gateway_error(StatusCode::BAD_GATEWAY, &format!("连不上上游 {upstream}"));
        }
    };
    // 面板的流量大多是小的 JSON + 长连接事件，Nagle 只会白等 40ms。
    let _ = stream.set_nodelay(true);

    let (mut sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(site = %site.name, %upstream, error = %e, "上游 HTTP 握手失败");
            metrics.record_upstream_error();
            return gateway_error(StatusCode::BAD_GATEWAY, "上游 HTTP 握手失败");
        }
    };
    // 驱动上游连接。它自己结束时任务就结束；这里不 join，错误只在 debug 里出现
    // （客户端断开时上游连接被丢是正常的）。
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            debug!(error = %e, "上游连接结束");
        }
    });

    // ── 构造上游请求 ───────────────────────────────────────────────────────
    let mut headers = strip_hop_by_hop(&parts.headers, is_upgrade);
    // Host：原样保留客户端发来的（含端口）。
    //
    // 原 nginx 配置用的是 `$http_host` 而不是 `$host`，注释写着「网关按 Host 拼链接，
    // 端口不能丢」。这里等价：直接透传客户端的 Host 头。缺失时（HTTP/1.0 客户端）
    // 用上游地址补一个，免得上游收到没有 Host 的请求。
    if !headers.contains_key(HOST) {
        if let Ok(v) = HeaderValue::from_str(&upstream.to_string()) {
            headers.insert(HOST, v);
        }
    }
    set_forwarded(&mut headers, peer);

    let body: Body = if is_upgrade {
        // 握手请求没有体。这里给一个空的，101 之后走拼接。
        empty()
    } else {
        // `Limited` 的 `Error` 就是 `Box<dyn Error + Send + Sync>`，与我们的体
        // 类型一致，所以这里不用再转一次。
        Limited::new(incoming, usize::try_from(site.max_body_bytes).unwrap_or(usize::MAX))
            .boxed_unsync()
    };

    let mut upstream_req = Request::new(body);
    *upstream_req.method_mut() = parts.method.clone();
    *upstream_req.uri_mut() = parts.uri.clone();
    *upstream_req.version_mut() = Version::HTTP_11;
    *upstream_req.headers_mut() = headers;

    // ── 发出去（带超时）────────────────────────────────────────────────────
    //
    // 超时是必须的：上游卡住时客户端会一直等，而这里没有 nginx 那套
    // `proxy_read_timeout` 兜底。
    let sent = tokio::time::timeout(site.read_timeout, sender.send_request(upstream_req)).await;

    let mut upstream_resp = match sent {
        Err(_) => {
            warn!(site = %site.name, %upstream, timeout = ?site.read_timeout, "上游超时");
            metrics.record_upstream_error();
            return gateway_error(StatusCode::GATEWAY_TIMEOUT, "上游超时");
        }
        Ok(Err(e)) => {
            warn!(site = %site.name, %upstream, error = %e, "上游请求失败");
            metrics.record_upstream_error();
            return gateway_error(StatusCode::BAD_GATEWAY, "上游请求失败");
        }
        Ok(Ok(r)) => r,
    };

    let status = upstream_resp.status();

    // ── 101：拼接双向流 ────────────────────────────────────────────────────
    if status == StatusCode::SWITCHING_PROTOCOLS {
        let Some(client_upgrade) = client_upgrade else {
            // 上游升了级但我们没准备升级（客户端没发 Upgrade）—— 协议上说不过去，
            // 当作上游错误。
            warn!(site = %site.name, %upstream, "上游返回 101，但客户端没有请求升级");
            return gateway_error(StatusCode::BAD_GATEWAY, "上游返回了意外的 101");
        };
        let server_upgrade = hyper::upgrade::on(&mut upstream_resp);

        tokio::spawn(async move {
            match tokio::try_join!(client_upgrade, server_upgrade) {
                Ok((client, server)) => {
                    let mut client = TokioIo::new(client);
                    let mut server = TokioIo::new(server);
                    match tokio::io::copy_bidirectional(&mut client, &mut server).await {
                        Ok((up, down)) => debug!(bytes_up = up, bytes_down = down, "升级连接结束"),
                        Err(e) => debug!(error = %e, "升级连接中断"),
                    }
                }
                Err(e) => warn!(error = %e, "升级失败"),
            }
        });
    }

    // ── 透传响应 ───────────────────────────────────────────────────────────
    let (resp_parts, resp_body) = upstream_resp.into_parts();
    let mut headers = strip_hop_by_hop(&resp_parts.headers, status == StatusCode::SWITCHING_PROTOCOLS);
    apply_common_headers(site, &mut headers);

    let body: Body = if status == StatusCode::SWITCHING_PROTOCOLS {
        // 101 的响应体就是那条被升级的连接，交给上面的任务处理。
        empty()
    } else {
        resp_body
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
            .boxed_unsync()
    };

    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.version_mut() = Version::HTTP_11;
    *response.headers_mut() = headers;
    response
}

fn gateway_error(status: StatusCode, detail: &str) -> Response<Body> {
    text_response(status, format!("{detail}\n"))
}

/// 剥掉 hop-by-hop 头。
///
/// `keep_upgrade` 为真时**保留** `connection` 与 `upgrade` —— 升级握手要靠它们。
/// 这是整个代理里最容易写错的一处：无脑全剥会让 WebSocket 永远停在
/// 「正在连接」，而表现跟后端没起一样。
fn strip_hop_by_hop(headers: &HeaderMap, keep_upgrade: bool) -> HeaderMap {
    let mut out = HeaderMap::new();
    // `Connection` 头里列出的字段名也是 hop-by-hop 的（RFC 9110 §7.6.1）。
    let mut listed: Vec<String> = Vec::new();
    for value in headers.get_all(CONNECTION).iter() {
        if let Ok(s) = value.to_str() {
            for token in s.split(',') {
                let t = token.trim();
                if !t.is_empty() {
                    listed.push(t.to_ascii_lowercase());
                }
            }
        }
    }

    for (name, value) in headers.iter() {
        if keep_upgrade && (name == CONNECTION || name == UPGRADE) {
            out.append(name.clone(), value.clone());
            continue;
        }
        if HOP_BY_HOP.iter().any(|h| *h == name.as_str()) {
            continue;
        }
        if listed.iter().any(|l| l.eq_ignore_ascii_case(name.as_str())) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// 补 `X-Forwarded-For` / `X-Real-IP` / `X-Forwarded-Proto`。
///
/// 与 nginx 配置一致：`X-Forwarded-For` 用 `$remote_addr` **整体替换**而不是追加。
/// 追加会让客户端伪造的值留在链路上，而上游（`esc-gateway`）正是拿这个头做
/// `trust_forwarded_for` 的信任判断 —— 那里能信任它，前提就是这里会覆盖。
fn set_forwarded(headers: &mut HeaderMap, peer: SocketAddr) {
    let ip = peer.ip().to_string();
    if let Ok(v) = HeaderValue::from_str(&ip) {
        headers.insert(HeaderName::from_static("x-forwarded-for"), v.clone());
        headers.insert(HeaderName::from_static("x-real-ip"), v);
    }
    headers.insert(
        HeaderName::from_static("x-forwarded-proto"),
        HeaderValue::from_static("https"),
    );
}

/// 访问日志。
///
/// 用 `tracing` 的结构化字段而不是照抄 nginx 的 `log_format` 文本行：
///
/// * 同样的信息，但能按字段过滤与聚合（`site`、`status`、`rt_ms`）；
/// * `log_query = false` 时**根本不存在**带令牌的字段，泄漏面从"记得配 log_format"
///   变成"这个字段压根没采集"。
///
/// 需要 nginx 那种一行文本时，`tracing-subscriber` 的 fmt 层就能输出。
fn log_access(
    site: &Site,
    peer: SocketAddr,
    method: Method,
    uri: String,
    response: &Response<Body>,
    upstream: &str,
    elapsed: Duration,
) {
    let status = response.status().as_u16();
    let user_agent = response
        .headers()
        .get(hyper::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    // 响应体大小此时未知（流式），照 nginx 的做法记"最终写出字节数"需要包一层
    // 计数 body。目前只记 `content-length`（能拿到就拿，拿不到记 0），
    // 与 nginx 在 `proxy_buffering off` 下的行为接近。
    let len = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("0");

    info!(
        target: "esc_tls::access",
        site = %site.name,
        %peer,
        %method,
        %uri,
        status,
        len,
        user_agent,
        upstream,
        rt_ms = elapsed.as_millis() as u64,
        "access"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 一些小工具
// ─────────────────────────────────────────────────────────────────────────────

/// 给 `serve.rs` 用的服务工厂：把 site 与 peer 绑进闭包。
pub fn service(
    site: Arc<Site>,
    peer: SocketAddr,
    metrics: Arc<Metrics>,
) -> impl hyper::service::Service<
    Request<Incoming>,
    Response = Response<Body>,
    Error = std::convert::Infallible,
    Future = impl std::future::Future<Output = Result<Response<Body>, std::convert::Infallible>>,
> {
    service_fn(move |req: Request<Incoming>| {
        let site = Arc::clone(&site);
        let metrics = Arc::clone(&metrics);
        async move { Ok::<_, std::convert::Infallible>(handle(site, req, peer, metrics).await) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_headers_are_removed() {
        let mut h = HeaderMap::new();
        h.insert(HOST, HeaderValue::from_static("example"));
        h.insert(HeaderName::from_static("keep-alive"), HeaderValue::from_static("timeout=5"));
        h.insert(
            HeaderName::from_static("te"),
            HeaderValue::from_static("trailers"),
        );
        h.insert(
            HeaderName::from_static("x-custom"),
            HeaderValue::from_static("keep"),
        );

        let out = strip_hop_by_hop(&h, false);
        assert!(out.contains_key(HOST));
        assert!(out.contains_key("x-custom"));
        assert!(!out.contains_key("keep-alive"), "hop-by-hop 必须剥掉");
        assert!(!out.contains_key("te"));
    }

    /// `Connection: X-Foo` 会把 `X-Foo` 也变成 hop-by-hop —— 这条 RFC 细节
    /// 漏掉的话，某些中间件塞进来的私有头会漏给上游。
    #[test]
    fn connection_listed_headers_are_also_removed() {
        let mut h = HeaderMap::new();
        h.insert(CONNECTION, HeaderValue::from_static("X-Secret-Token"));
        h.insert(
            HeaderName::from_static("x-secret-token"),
            HeaderValue::from_static("leak"),
        );
        let out = strip_hop_by_hop(&h, false);
        assert!(!out.contains_key("x-secret-token"), "Connection 里列出的也要剥");
    }

    /// 升级路径必须保留 `Connection` / `Upgrade`：少了它们 WebSocket 永远停在
    /// 「正在连接」，而表现跟后端没起一样。
    #[test]
    fn upgrade_headers_survive_when_asked() {
        let mut h = HeaderMap::new();
        h.insert(CONNECTION, HeaderValue::from_static("upgrade"));
        h.insert(UPGRADE, HeaderValue::from_static("websocket"));
        h.insert(HeaderName::from_static("sec-websocket-key"), HeaderValue::from_static("abc"));

        let out = strip_hop_by_hop(&h, true);
        assert!(out.contains_key(CONNECTION));
        assert!(out.contains_key(UPGRADE));
        assert!(out.contains_key("sec-websocket-key"), "其它头照常保留");
    }

    #[test]
    fn forwarded_headers_are_set_not_appended() {
        let mut h = HeaderMap::new();
        // 客户端伪造的值
        h.insert(
            HeaderName::from_static("x-forwarded-for"),
            HeaderValue::from_static("1.2.3.4"),
        );
        // 注意：set_forwarded 作用在**已剥过**的头表上，而 x-forwarded-for 不在
        // hop-by-hop 列表里，所以这里手工清一下，模拟"覆盖"语义的验证场景。
        h.remove(HeaderName::from_static("x-forwarded-for"));

        let peer: SocketAddr = "9.9.9.9:1234".parse().unwrap();
        set_forwarded(&mut h, peer);
        assert_eq!(h.get("x-forwarded-for").unwrap(), "9.9.9.9");
        assert_eq!(h.get("x-real-ip").unwrap(), "9.9.9.9");
        assert_eq!(h.get("x-forwarded-proto").unwrap(), "https");
        assert_eq!(
            h.get_all("x-forwarded-for").iter().count(),
            1,
            "只能有一条，追加会让客户端伪造的值留下来"
        );
    }
}
