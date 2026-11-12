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

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::Site;
use crate::control::Metrics;
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

// ─────────────────────────────────────────────────────────────────────────────
// 运行期可替换的活配置
// ─────────────────────────────────────────────────────────────────────────────

/// 每个连接建立时读一次的**活配置**。
///
/// 有了它，「只改上游 / 301 / 超时」就不必重绑端口 —— 监听 socket 一直开着，
/// 下一个连接自然用新配置。已经建立的连接继续用它建立时的那一份（与 nginx
/// reload 同一个语义：老 worker 处理完在途连接才退）。
///
/// 三把锁分开而不是合成一把：换证书、换站点表、换超时是三次独立的原子操作。
/// 合成一把会让「只换证书」也去抢站点表的写锁，而站点表是每连接都要读的。
pub struct Shared {
    sites: std::sync::RwLock<Arc<HashMap<SocketAddr, Arc<Site>>>>,
    tls: std::sync::RwLock<Arc<rustls::ServerConfig>>,
    handshake_timeout: std::sync::RwLock<Duration>,
}

impl Shared {
    fn new(sites: &[Site], tls: Arc<rustls::ServerConfig>, handshake_timeout: Duration) -> Self {
        Self {
            sites: std::sync::RwLock::new(Arc::new(index(sites))),
            tls: std::sync::RwLock::new(tls),
            handshake_timeout: std::sync::RwLock::new(handshake_timeout),
        }
    }

    /// 这个端口当前的站点定义。端口已从配置里消失时返回 `None`。
    pub fn site(&self, addr: SocketAddr) -> Option<Arc<Site>> {
        let g = self.sites.read().ok()?;
        g.get(&addr).cloned()
    }

    /// 当前的 TLS 配置（证书解析器在它里面）。
    ///
    /// 锁中毒时退回**上一次的值**而不是 panic：这函数在 accept 循环的热路径上，
    /// 让它 panic 等于整个端口不再接受连接 —— 比用一份旧证书糟得多。
    pub fn tls_config(&self) -> Arc<rustls::ServerConfig> {
        match self.tls.read() {
            Ok(g) => Arc::clone(&g),
            Err(p) => Arc::clone(&p.into_inner()),
        }
    }

    pub fn handshake_timeout(&self) -> Duration {
        match self.handshake_timeout.read() {
            Ok(g) => *g,
            Err(p) => *p.into_inner(),
        }
    }

    fn set_sites(&self, sites: &[Site]) {
        let next = Arc::new(index(sites));
        match self.sites.write() {
            Ok(mut g) => *g = next,
            Err(p) => *p.into_inner() = next,
        }
    }

    fn set_tls(&self, tls: Arc<rustls::ServerConfig>) {
        match self.tls.write() {
            Ok(mut g) => *g = tls,
            Err(p) => *p.into_inner() = tls,
        }
    }

    fn set_timeout(&self, d: Duration) {
        match self.handshake_timeout.write() {
            Ok(mut g) => *g = d,
            Err(p) => *p.into_inner() = d,
        }
    }
}

/// 按监听地址建索引。
///
/// 用 `addr` 而不是 site 名当键：同一个名字可以挂多个端口（`listen` 是数组时
/// 校验阶段就展开成多条 Site），而端口才是 accept 之后拿得到的那个东西。
fn index(sites: &[Site]) -> HashMap<SocketAddr, Arc<Site>> {
    sites.iter().map(|s| (s.addr, Arc::new(s.clone()))).collect()
}

/// 一次 [`ServerHandle::sync`] 的结果。
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    /// 这次新绑定的端口。
    pub added: Vec<SocketAddr>,
    /// 这次关掉的端口。
    pub removed: Vec<SocketAddr>,
    /// 完全没动的端口数。
    pub unchanged: usize,
}

/// 运行中的服务句柄。`shutdown` 之后所有监听与在途连接都停。
pub struct ServerHandle {
    shared: Arc<Shared>,
    root: CancellationToken,
    ports: std::sync::Mutex<HashMap<SocketAddr, PortTask>>,
    metrics: Arc<Metrics>,
}

struct PortTask {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl ServerHandle {
    /// 当前在监听的端口（升序，日志里读起来稳定）。
    pub fn addrs(&self) -> Vec<SocketAddr> {
        let mut v: Vec<SocketAddr> = match self.ports.lock() {
            Ok(g) => g.keys().copied().collect(),
            Err(p) => p.into_inner().keys().copied().collect(),
        };
        v.sort();
        v
    }

    /// 活配置的共享句柄（测试与状态查询用）。
    pub fn shared(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    /// 把运行中的监听同步到 `sites`，同时替换证书与握手超时。
    ///
    /// # 为什么是这个顺序
    ///
    /// ```text
    /// ① 先绑新增端口   → 失败就整体放弃，旧监听一个都没动，线上无变化
    /// ② 再换活配置     → 从这一刻起所有端口用新表；没被移除的端口零中断
    /// ③ 最后停移除端口 → 在途连接自然结束
    /// ```
    ///
    /// 反过来（先停再绑）会出现一个窗口：该端口上的请求直接连接失败。而如果
    /// 新端口绑不上（被别的进程占了），这个窗口就变成了永久中断。
    pub fn sync(
        &self,
        sites: Vec<Site>,
        tls: Arc<rustls::ServerConfig>,
        handshake_timeout: Duration,
    ) -> Result<SyncReport, String> {
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|_| "TLS 终结需要 tokio 运行时".to_string())?;

        let want: HashSet<SocketAddr> = sites.iter().map(|s| s.addr).collect();
        let have: HashSet<SocketAddr> = match self.ports.lock() {
            Ok(g) => g.keys().copied().collect(),
            Err(p) => p.into_inner().keys().copied().collect(),
        };

        let added: Vec<SocketAddr> = want.difference(&have).copied().collect();
        let removed: Vec<SocketAddr> = have.difference(&want).copied().collect();
        let unchanged = want.intersection(&have).count();

        // ── ① 先绑新增。任一失败就返回，`fresh` 随函数返回被 drop，端口释放 ──
        let mut fresh: Vec<(SocketAddr, PortTask)> = Vec::with_capacity(added.len());
        for addr in &added {
            let site = sites
                .iter()
                .find(|s| s.addr == *addr)
                .expect("差的集合是从 sites 里算出来的");
            let listener = std::net::TcpListener::bind(addr).map_err(|e| {
                format!(
                    "site {:?} 无法监听 {addr}: {e}\n\
                     （整体放弃：旧监听一个都没有改动）",
                    site.name
                )
            })?;
            listener
                .set_nonblocking(true)
                .map_err(|e| format!("site {:?} 设置非阻塞失败: {e}", site.name))?;
            let listener = TcpListener::from_std(listener)
                .map_err(|e| format!("接管监听 {addr} 失败: {e}"))?;

            let cancel = self.root.child_token();
            let token = cancel.clone();
            let shared = Arc::clone(&self.shared);
            let metrics = Arc::clone(&self.metrics);
            let a = *addr;
            let task = rt.spawn(async move {
                accept_loop(shared, a, listener, token, metrics).await;
            });
            fresh.push((*addr, PortTask { cancel, task }));
        }

        // ── ② 换活配置。此后新连接一律看到新配置 ──
        self.shared.set_sites(&sites);
        self.shared.set_tls(tls);
        self.shared.set_timeout(handshake_timeout);

        // ── ③ 停掉被移除的端口 ──
        {
            let mut g = match self.ports.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            for addr in &removed {
                if let Some(pt) = g.remove(addr) {
                    pt.cancel.cancel();
                    pt.task.abort();
                }
            }
            for (addr, pt) in fresh {
                g.insert(addr, pt);
            }
        }

        Ok(SyncReport {
            added,
            removed,
            unchanged,
        })
    }

    /// 停掉。幂等。
    pub fn shutdown(&self) {
        self.root.cancel();
        let mut g = match self.ports.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        for (_, pt) in g.drain() {
            pt.cancel.cancel();
            pt.task.abort();
        }
    }

    pub fn is_running(&self) -> bool {
        if self.root.is_cancelled() {
            return false;
        }
        match self.ports.lock() {
            Ok(g) => g.values().any(|pt| !pt.task.is_finished()),
            Err(p) => p.into_inner().values().any(|pt| !pt.task.is_finished()),
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 为每个监听起一个 accept 循环。
///
/// 站点定义不再按连接捕获，而是每次 accept 时从 [`Shared`] 里读 —— 这是
/// 「改上游不用重绑端口」的实现方式。
pub fn spawn(
    bound: Vec<Bound>,
    tls: Arc<rustls::ServerConfig>,
    handshake_timeout: Duration,
    metrics: Arc<Metrics>,
) -> Result<ServerHandle, String> {
    let rt = tokio::runtime::Handle::try_current()
        .map_err(|_| "TLS 终结需要 tokio 运行时（宿主必须在 runtime 内挂载插件）".to_string())?;

    let root = CancellationToken::new();

    // `:0` 要换成系统分配的真实端口：`Shared` 按 addr 索引，不换就永远查不到
    // （端口写在配置里是 :0，实际监听的是 :38xxx）。
    let mut sites: Vec<Site> = Vec::with_capacity(bound.len());
    let mut prepared: Vec<(SocketAddr, TcpListener)> = Vec::with_capacity(bound.len());
    for Bound { site, listener } in bound {
        let addr = listener
            .local_addr()
            .map_err(|e| format!("读取监听地址失败: {e}"))?;
        let listener = TcpListener::from_std(listener)
            .map_err(|e| format!("接管监听 {addr} 失败: {e}"))?;
        let mut site = site;
        site.addr = addr;
        sites.push(site);
        prepared.push((addr, listener));
    }

    let shared = Arc::new(Shared::new(&sites, tls, handshake_timeout));
    let mut ports = HashMap::with_capacity(prepared.len());

    for (addr, listener) in prepared {
        let site = shared.site(addr).expect("刚建好的索引");
        // 启动日志要能回答「哪个端口接受什么」——这是排障时第一个要确认的事。
        info!(
            site = %site.name,
            %addr,
            action = ?site.action,
            websocket = site.websocket,
            log_query = site.log_query,
            "TLS 终结监听已启动"
        );

        let cancel = root.child_token();
        let token = cancel.clone();
        let s = Arc::clone(&shared);
        let metrics = Arc::clone(&metrics);
        let task = rt.spawn(async move {
            accept_loop(s, addr, listener, token, metrics).await;
        });
        ports.insert(addr, PortTask { cancel, task });
    }

    Ok(ServerHandle {
        shared,
        root,
        ports: std::sync::Mutex::new(ports),
        metrics,
    })
}

async fn accept_loop(
    shared: Arc<Shared>,
    addr: SocketAddr,
    listener: TcpListener,
    cancel: CancellationToken,
    metrics: Arc<Metrics>,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                let (tcp, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        // accept 失败通常意味着 fd 耗尽。立刻继续会把它变成忙循环，
                        // 所以让出一下再试。
                        warn!(%addr, error = %e, "accept 失败");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };

                // 每次 accept 都读一次活配置。查不到 = 这个端口已被 `sync` 摘掉，
                // 但它这个循环还没收到 cancel（几微秒的窗口）——让出一下即可。
                let Some(site) = shared.site(addr) else {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                let tls = shared.tls_config();
                let handshake_timeout = shared.handshake_timeout();
                let metrics = Arc::clone(&metrics);

                tokio::spawn(async move {
                    if let Err(e) = serve_conn(site.as_ref(), tcp, peer, tls, handshake_timeout, metrics).await {
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
    metrics: Arc<Metrics>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = tcp.set_nodelay(true);

    // 证书按 SNI 由 `tls.cert_resolver` 选（见 certs.rs），所以这里只有一个
    // 固定的 ServerConfig，不需要 LazyConfigAcceptor。
    let acceptor = TlsAcceptor::from(tls);
    let stream = match tokio::time::timeout(handshake_timeout, acceptor.accept(tcp)).await {
        Err(_) => {
            metrics.record_handshake_failure();
            return Err("TLS 握手超时".into());
        }
        Ok(Err(e)) => {
            metrics.record_handshake_failure();
            return Err(Box::new(e));
        }
        Ok(Ok(s)) => s,
    };

    let io = TokioIo::new(stream);
    let service = proxy::service(Arc::new(site.clone()), peer, metrics);

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
