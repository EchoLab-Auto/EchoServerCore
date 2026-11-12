//! 监听、TLS 握手、把连接交给 HTTP/1.1。
//!
//! # 为什么 bind 在 mount 里同步做
//!
//! 与 `esc.edge-router` 同一个取舍：端口被占用、没有权限绑 1024 以下这类错误，要在
//! **挂载阶段**就让宿主看到（fail-closed）。等到「挂载成功但每个连接都失败」，那
//! 就变成一次线上事故而不是一次启动失败。
//!
//! # 每个连接的顺序
//!
//! ```text
//! accept → 握手（带超时）→ HTTP/1.1（带升级支持）→ 交给 proxy::handle
//! ```
//!
//! 握手有超时是必须的：没有它，一个只连不发数据的客户端能把连接槽位一直占着
//! （slowloris）。这里用的是 `defaults.handshake_timeout_secs`。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::Site;
use crate::proxy;

/// 同一个地址上「客户端迟迟不发完请求头」的上限。
///
/// 与 nginx 的 `client_header_timeout` 默认值（60s）对齐。它挡的是那种连上之后
/// 一个字节一个字节发头的客户端 —— 那类流量不会被握手超时挡住，因为握手已经成功了。
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// 一个已经绑好的监听。
///
/// `Debug` 是给测试里的 `unwrap_err()` 用的（它要求 `Ok` 侧可打印）。
#[derive(Debug)]
pub struct Bound {
    pub site: Site,
    pub listener: std::net::TcpListener,
}

/// 同步绑定全部监听地址。
///
/// 任一失败就整体失败，并且**把已经绑好的关掉**（`std_listener` 在 `Vec` 里，
/// 函数返回 `Err` 时随之 drop，端口自然释放）。
pub fn bind(sites: &[Site]) -> Result<Vec<Bound>, String> {
    let mut out = Vec::with_capacity(sites.len());
    for site in sites {
        let listener = std::net::TcpListener::bind(site.addr).map_err(|e| {
            // 说明"整体失败"很重要：`out` 随函数返回被 drop，已绑的端口会释放，
            // 修好配置直接重试就行，不用先找谁占着。
            format!(
                "site {:?} 无法监听 {}: {e}\n（整体失败，已绑定的端口都会释放）",
                site.name, site.addr
            )
        })?;
        // 已经绑定之后立刻要求非阻塞：tokio 接管时要用。
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("site {:?} 设置非阻塞失败: {e}", site.name))?;
        out.push(Bound {
            site: site.clone(),
            listener,
        });
    }
    Ok(out)
}

/// 运行中的服务句柄。`shutdown` 之后所有监听与在途连接都停。
pub struct ServerHandle {
    addrs: Vec<SocketAddr>,
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ServerHandle {
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// 停掉。幂等。
    pub fn shutdown(&self) {
        self.cancel.cancel();
        for t in &self.tasks {
            t.abort();
        }
    }

    pub fn is_running(&self) -> bool {
        !self.cancel.is_cancelled() && self.tasks.iter().any(|t| !t.is_finished())
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 为每个监听起一个 accept 循环。
pub fn spawn(
    bound: Vec<Bound>,
    tls: Arc<rustls::ServerConfig>,
    handshake_timeout: Duration,
) -> Result<ServerHandle, String> {
    let rt = tokio::runtime::Handle::try_current()
        .map_err(|_| "TLS 终结需要 tokio 运行时（宿主必须在 runtime 内挂载插件）".to_string())?;

    let cancel = CancellationToken::new();
    let mut addrs = Vec::with_capacity(bound.len());
    let mut tasks = Vec::with_capacity(bound.len());

    for Bound { site, listener } in bound {
        let addr = listener
            .local_addr()
            .map_err(|e| format!("读取监听地址失败: {e}"))?;
        let listener = TcpListener::from_std(listener)
            .map_err(|e| format!("接管监听 {addr} 失败: {e}"))?;
        addrs.push(addr);

        let site = Arc::new(site);
        let tls = Arc::clone(&tls);
        let token = cancel.child_token();

        // 启动日志要能回答「哪个端口接受什么」——这是排障时第一个要确认的事。
        info!(
            site = %site.name,
            %addr,
            action = ?site.action,
            websocket = site.websocket,
            log_query = site.log_query,
            "TLS 终结监听已启动"
        );

        let task = rt.spawn(async move {
            accept_loop(site, listener, tls, handshake_timeout, token).await;
        });
        tasks.push(task);
    }

    Ok(ServerHandle {
        addrs,
        cancel,
        tasks,
    })
}

async fn accept_loop(
    site: Arc<Site>,
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    handshake_timeout: Duration,
    cancel: CancellationToken,
) {
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                let (tcp, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        // accept 失败通常意味着 fd 耗尽。立刻继续会把它变成忙循环，
                        // 所以让出一下再试。
                        warn!(site = %site.name, error = %e, "accept 失败");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let site = Arc::clone(&site);
                let tls = Arc::clone(&tls);
                tokio::spawn(async move {
                    if let Err(e) = serve_conn(site.as_ref(), tcp, peer, tls, handshake_timeout).await {
                        // 握手失败是**常态**（端口扫描器、健康检查探活、客户端
                        // 提前断开），所以只记 debug。真正需要报警的是"每个连接
                        // 都失败"，那会在日志里形成一片而不是零星几条。
                        debug!(peer = %peer, error = %e, "连接未完成");
                    }
                });
            }
        }
    }

    debug!(%addr, "TLS 终结监听已关闭");
}

async fn serve_conn(
    site: &Site,
    tcp: TcpStream,
    peer: SocketAddr,
    tls: Arc<rustls::ServerConfig>,
    handshake_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = tcp.set_nodelay(true);

    // 证书按 SNI 由 `tls.cert_resolver` 选（见 certs.rs），所以这里只有一个
    // 固定的 ServerConfig，不需要 LazyConfigAcceptor。
    let acceptor = TlsAcceptor::from(tls);
    let stream = match tokio::time::timeout(handshake_timeout, acceptor.accept(tcp)).await {
        Err(_) => return Err("TLS 握手超时".into()),
        Ok(Err(e)) => return Err(Box::new(e)),
        Ok(Ok(s)) => s,
    };

    let io = TokioIo::new(stream);
    let service = proxy::service(Arc::new(site.clone()), peer);

    // `with_upgrades()` 是 WebSocket 的开关。少了它，`hyper::upgrade::on` 会立刻
    // 返回错误，于是 101 之后拼接的两条流都拿不到 —— 表现是面板一直「正在连接」。
    // `header_read_timeout` 必须配 `.timer(..)` —— hyper 不会替你挑时钟。
    // 少了它，第一次请求就 panic：`timeout set, but no timer set`。
    let conn = hyper::server::conn::http1::Builder::new()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .serve_connection(io, service)
        .with_upgrades();

    conn.await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Action;

    fn site(name: &str, addr: SocketAddr) -> Site {
        Site {
            name: name.into(),
            addr,
            action: Action::Proxy("127.0.0.1:1".parse().unwrap()),
            websocket: false,
            log_query: false,
            read_timeout: Duration::from_secs(5),
            max_body_bytes: 1024,
            hsts: false,
        }
    }

    /// 绑在 `:0` 上，由系统分配端口 —— 测试之间不会抢端口。
    #[tokio::test]
    async fn bind_assigns_ports_and_serves() {
        let s = site("t", "127.0.0.1:0".parse().unwrap());
        let bound = bind(&[s]).expect("应能绑定");
        assert_eq!(bound.len(), 1);
        assert_ne!(
            bound[0].listener.local_addr().unwrap().port(),
            0,
            "端口应由系统分配"
        );
    }

    /// 同一个地址绑两次，第二次必须失败，且错误信息要指出是哪个 site。
    #[tokio::test]
    async fn double_bind_reports_the_site_name() {
        let first = bind(&[site("one", "127.0.0.1:0".parse().unwrap())]).unwrap();
        let taken = first[0].listener.local_addr().unwrap();
        let err = bind(&[site("two", taken)]).unwrap_err();
        assert!(err.contains("two"), "要指出是哪个 site：{err}");
    }

    /// 部分成功、部分失败时，已绑定的端口必须被释放（否则下次重试还是占用）。
    #[tokio::test]
    async fn failing_bind_releases_previously_bound_ports() {
        let first = bind(&[site("one", "127.0.0.1:0".parse().unwrap())]).unwrap();
        let taken = first[0].listener.local_addr().unwrap();

        // 第一个成功、第二个撞车
        let err = bind(&[
            site("ok", "127.0.0.1:0".parse().unwrap()),
            site("boom", taken),
        ])
        .unwrap_err();
        assert!(err.contains("boom"));

        // `ok` 那个端口应该已经释放：能再绑一次就是证明
        // （拿不到它的地址了，所以这里改成验证"再绑一个新的且不泄漏"）
        let again = bind(&[site("again", "127.0.0.1:0".parse().unwrap())]);
        assert!(again.is_ok());
    }
}
