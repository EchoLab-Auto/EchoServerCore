//! Hub engine：公网侧的可编程内核。
//!
//! 与原来的 `esc_hub::run` 相比，唯一的本质区别是**公网监听不再在启动时冻结**：
//!
//! * [`HubEngine::ensure_service`] 在运行时绑定一个新端口（或修改已有映射）；
//! * [`HubEngine::remove_service`] 在运行时释放一个端口；
//! * 每个映射自带计数器，面板可以直接显示连接数 / 字节数；
//! * 已连接的 spoke 一句都不用重连——只要它在 `Hello` 里声明过这个服务名，
//!   新映射会立刻被授权给那条现有连接（[`Registry::grant`]）。
//!
//! 控制面的安全模型没有变：spoke 仍然只能“声明”，能不能开端口完全由
//! 本引擎（也就是面板背后的映射表）决定。

pub mod registry;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock};
use tokio_rustls::TlsAcceptor;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use yamux::Mode;

use esc_common::config::{HubSection, HubService, TlsConfig};
use esc_common::mux::{default_config, Mux};
use esc_common::protocol::{self, Ack, DeniedService, Hello, StreamHeader, PROTO_VERSION};
use esc_common::tls;
use esc_control_api::{ControlError, MappingStats, SpokeView};
use registry::{Entry, Registry};

/// Seconds a fresh spoke has to send its `Hello` before we hang up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a public connection waits for a provider when
/// `hub.reject_when_absent = false`.
const GRACE_PERIOD: Duration = Duration::from_secs(5);

/// 一个映射（一个公网端口）的累计计数。
///
/// 放在 `Arc` 里给接受循环与转发任务共享；面板读的时候直接 load，不需要锁。
#[derive(Debug, Default)]
pub struct SvcStats {
    pub total: AtomicU64,
    pub active: AtomicU64,
    pub up: AtomicU64,
    pub down: AtomicU64,
}

impl SvcStats {
    pub fn snapshot(&self) -> MappingStats {
        MappingStats {
            connections_total: self.total.load(Ordering::Relaxed),
            connections_active: self.active.load(Ordering::Relaxed),
            bytes_up: self.up.load(Ordering::Relaxed),
            bytes_down: self.down.load(Ordering::Relaxed),
        }
    }
}

/// 在转发任务结束时把「活跃连接数」还回去，异常退出也不会漏计。
struct ActiveGuard(Arc<SvcStats>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 一个正在服务的映射。
struct LiveService {
    spec: HubService,
    bound: SocketAddr,
    stats: Arc<SvcStats>,
    cancel: CancellationToken,
}

/// 一条已建立的 spoke 连接。
struct SpokeHandle {
    spoke_id: String,
    peer: String,
    agent: String,
    /// spoke 在 `Hello` 里声明的全部服务名（含被拒的）。
    declared: Vec<String>,
    granted: Vec<String>,
    since: Instant,
    /// 取消它 = 踢掉这条连接（spoke 会自动重连并重新握手）。
    cancel: CancellationToken,
}

struct HubInner {
    hub: HubSection,
    control_addr: SocketAddr,
    registry: Registry,
    services: RwLock<HashMap<String, LiveService>>,
    spokes: RwLock<HashMap<String, SpokeHandle>>,
    started: Instant,
    shutdown: CancellationToken,
    control_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// 可克隆的 hub 引擎句柄。
#[derive(Clone)]
pub struct HubEngine {
    inner: Arc<HubInner>,
}

impl HubEngine {
    /// 绑定控制端口并开始接受 spoke。公网映射随后由 [`Self::ensure_service`] 添加。
    pub async fn start(hub: HubSection, tls: TlsConfig) -> Result<Self, ControlError> {
        let acceptor = TlsAcceptor::from(
            tls::build_server_config(&tls).map_err(|e| ControlError::other(format!("TLS 配置无效: {e}")))?,
        );
        let control_addr = hub
            .listen
            .parse::<SocketAddr>()
            .map_err(|e| ControlError::invalid(format!("hub.listen 无效 {:?}: {e}", hub.listen)))?;
        let control = TcpListener::bind(control_addr)
            .await
            .map_err(|e| ControlError::bind(format!("无法绑定控制端口 {control_addr}: {e}")))?;
        let bound = control
            .local_addr()
            .map_err(|e| ControlError::bind(format!("无法读取控制端口地址: {e}")))?;

        let inner = Arc::new(HubInner {
            hub: hub.clone(),
            control_addr: bound,
            registry: Registry::new(),
            services: RwLock::new(HashMap::new()),
            spokes: RwLock::new(HashMap::new()),
            started: Instant::now(),
            shutdown: CancellationToken::new(),
            control_task: Mutex::new(None),
        });

        let engine = Self { inner: inner.clone() };
        let task = tokio::spawn(async move { inner.accept_control_loop(control, acceptor).await });
        *engine.inner.control_task.lock().await = Some(task);

        info!(addr = %bound, "hub engine: control listener bound");
        Ok(engine)
    }

    pub fn control_addr(&self) -> SocketAddr {
        self.inner.control_addr
    }

    pub fn uptime(&self) -> Duration {
        self.inner.started.elapsed()
    }

    pub async fn heartbeat_secs(&self) -> u64 {
        self.inner.hub.heartbeat_secs
    }

    /// 让一个映射生效；已存在时按需重绑（listen 或钉住的 spoke 变了）。
    ///
    /// 返回它当前的统计计数器，供上层拼装视图。
    pub async fn ensure_service(
        &self,
        spec: HubService,
    ) -> Result<Arc<SvcStats>, ControlError> {
        let addr = spec
            .listen_addr()
            .map_err(ControlError::invalid)?;
        if addr == self.inner.control_addr {
            return Err(ControlError::invalid(format!(
                "映射 {:?} 不能使用控制端口 {addr}",
                spec.name
            )));
        }

        // 已存在且完全一致 → 幂等返回，避免无谓地断开已有连接。
        {
            let services = self.inner.services.read().await;
            if let Some(live) = services.get(&spec.name) {
                if live.spec.listen == spec.listen && live.spec.spoke == spec.spoke {
                    return Ok(live.stats.clone());
                }
            }
        }

        // 变了就先摘掉旧的（释放端口），再绑新的。
        self.remove_service(&spec.name).await?;

        let listener = TcpListener::bind(addr).await.map_err(|e| {
            ControlError::bind(format!("无法绑定公网端口 {addr}（映射 {:?}）: {e}", spec.name))
        })?;
        let bound = listener.local_addr().unwrap_or(addr);
        let stats = Arc::new(SvcStats::default());
        let cancel = CancellationToken::new();

        {
            let mut services = self.inner.services.write().await;
            services.insert(
                spec.name.clone(),
                LiveService {
                    spec: spec.clone(),
                    bound,
                    stats: stats.clone(),
                    cancel: cancel.clone(),
                },
            );
        }

        // 已经开始监听，就把这个服务授权给「声明过它」的现有连接。
        self.grant_to_existing(&spec).await;

        let inner = self.inner.clone();
        let svc = spec.clone();
        let stats_task = stats.clone();
        tokio::spawn(async move {
            inner.serve_public(svc, listener, stats_task, cancel).await;
        });

        info!(
            service = %spec.name,
            addr = %bound,
            pinned_spoke = spec.spoke.as_deref().unwrap_or("-"),
            "service live"
        );
        Ok(stats)
    }

    /// 释放一个映射占用的公网端口。返回它此前是否存在。
    pub async fn remove_service(&self, name: &str) -> Result<bool, ControlError> {
        let live = {
            let mut services = self.inner.services.write().await;
            services.remove(name)
        };
        let Some(live) = live else { return Ok(false) };
        live.cancel.cancel();
        self.inner.registry.drop_service(name).await;
        info!(service = %name, addr = %live.bound, "service released");
        Ok(true)
    }

    pub async fn service_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.inner.services.read().await.keys().cloned().collect();
        v.sort();
        v
    }

    pub async fn service_info(&self, name: &str) -> Option<(SocketAddr, Arc<SvcStats>)> {
        self.inner
            .services
            .read()
            .await
            .get(name)
            .map(|s| (s.bound, s.stats.clone()))
    }

    /// 当前能提供某服务的 spoke id（面板用来显示「谁在提供服务」）。
    pub async fn providers_of(&self, name: &str) -> Vec<String> {
        self.inner.registry.provider_ids(name).await
    }

    pub async fn spokes(&self) -> Vec<SpokeView> {
        let g = self.inner.spokes.read().await;
        let mut out: Vec<SpokeView> = g
            .values()
            .map(|h| SpokeView {
                id: h.spoke_id.clone(),
                peer: h.peer.clone(),
                agent: h.agent.clone(),
                granted: h.granted.clone(),
                connected_secs: h.since.elapsed().as_secs(),
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// 踢掉指定的（或全部）spoke 连接。返回受影响条数。
    pub async fn kick(&self, id: Option<&str>) -> usize {
        let g = self.inner.spokes.read().await;
        let mut n = 0;
        for h in g.values() {
            if id.map(|want| want == h.spoke_id).unwrap_or(true) {
                h.cancel.cancel();
                n += 1;
            }
        }
        n
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        let services: Vec<(String, CancellationToken)> = {
            let g = self.inner.services.read().await;
            g.iter().map(|(k, v)| (k.clone(), v.cancel.clone())).collect()
        };
        for (_, token) in services {
            token.cancel();
        }
        let task = self.inner.control_task.lock().await.take();
        if let Some(t) = task {
            t.abort();
        }
        info!("hub engine stopped");
    }

    /// 新映射出现后，把服务授权给已经声明过它的连接。
    async fn grant_to_existing(&self, spec: &HubService) {
        let targets: Vec<(String, Vec<String>)> = {
            let spokes = self.inner.spokes.read().await;
            spokes
                .iter()
                .filter(|(_, h)| h.declared.iter().any(|d| d == &spec.name))
                .filter(|(_, h)| match (&spec.spoke, h.spoke_id.as_str()) {
                    (Some(want), got) => want == got,
                    (None, _) => true,
                })
                .map(|(key, _h)| (key.clone(), vec![spec.name.clone()]))
                .collect()
        };
        for (conn_key, names) in targets {
            self.inner.registry.grant(&conn_key, &names).await;
            debug!(conn = %conn_key, service = %spec.name, "service granted to existing spoke");
        }
    }
}

impl HubInner {
    async fn accept_control_loop(self: Arc<Self>, control: TcpListener, acceptor: TlsAcceptor) {
        loop {
            let (sock, peer) = tokio::select! {
                _ = self.shutdown.cancelled() => break,
                r = control.accept() => match r {
                    Ok(v) => v,
                    Err(e) => {
                        debug!(error = %e, "control accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                },
            };
            let me = self.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Err(e) = me.handle_spoke(sock, peer, acceptor).await {
                    debug!(%peer, error = %e, "spoke connection ended");
                }
            });
        }
    }

    /// Accept loop for one publicly-exposed port.
    async fn serve_public(
        self: Arc<Self>,
        svc: HubService,
        listener: TcpListener,
        stats: Arc<SvcStats>,
        cancel: CancellationToken,
    ) {
        let reject_when_absent = self.hub.reject_when_absent;
        loop {
            let (sock, peer) = tokio::select! {
                _ = cancel.cancelled() => break,
                _ = self.shutdown.cancelled() => break,
                r = listener.accept() => match r {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(service = %svc.name, error = %e, "public accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                },
            };

            let entry = self.registry.pick(&svc.name, svc.spoke.as_deref()).await;
            let entry = match entry {
                Some(e) => e,
                None if !reject_when_absent => {
                    match wait_for_provider(&self.registry, &svc, peer).await {
                        Some(e) => e,
                        None => {
                            warn!(service = %svc.name, %peer, "no spoke appeared within the grace period, closing");
                            drop(sock);
                            continue;
                        }
                    }
                }
                None => {
                    warn!(service = %svc.name, %peer, "no spoke currently serves this service, closing");
                    drop(sock);
                    continue;
                }
            };

            let svc_name = svc.name.clone();
            let stats = stats.clone();
            stats.total.fetch_add(1, Ordering::Relaxed);
            stats.active.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(async move {
                let _guard = ActiveGuard(stats.clone());
                match forward(sock, entry, svc_name.clone(), stats.clone()).await {
                    Ok((up, down)) => {
                        debug!(service = %svc_name, %peer, up, down, "connection finished");
                    }
                    Err(e) => debug!(service = %svc_name, %peer, error = %e, "connection failed"),
                }
            });
        }
        debug!(service = %svc.name, "public listener stopped");
    }

    /// Handle one authenticated spoke connection for its whole lifetime.
    async fn handle_spoke(
        self: Arc<Self>,
        sock: TcpStream,
        peer: SocketAddr,
        acceptor: TlsAcceptor,
    ) -> Result<()> {
        let _ = sock.set_nodelay(true);
        let conn_key = format!("{peer}#{}", next_conn_seq());

        let tls_stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(sock))
            .await
            .map_err(|_| anyhow::anyhow!("TLS handshake timed out"))?
            .context("TLS handshake failed")?;

        let cert_bytes = tls_stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|certs| certs.first().map(|c| c.as_ref().len()))
            .unwrap_or(0);

        let (mux, mut inbound) = Mux::spawn(
            tls_stream.compat(),
            Mode::Server,
            default_config(512),
            format!("spoke@{peer}"),
        );

        let ctrl = tokio::time::timeout(HANDSHAKE_TIMEOUT, inbound.recv())
            .await
            .map_err(|_| anyhow::anyhow!("spoke opened no control stream"))?
            .ok_or_else(|| anyhow::anyhow!("connection closed before control stream"))?;
        let mut ctrl = ctrl.compat();

        let hello: Hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, protocol::read_msg(&mut ctrl))
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for Hello"))?
            .context("cannot read Hello")?;

        if hello.version != PROTO_VERSION {
            let msg = format!(
                "protocol version mismatch: spoke sent {}, hub speaks {}",
                hello.version, PROTO_VERSION
            );
            warn!(%peer, spoke = %hello.spoke_id, "{msg}");
            let _ = protocol::write_msg(
                &mut ctrl,
                &Ack {
                    version: PROTO_VERSION,
                    granted: vec![],
                    denied: vec![DeniedService { name: "*".into(), reason: msg.clone() }],
                },
            )
            .await;
            anyhow::bail!(msg);
        }

        // Decide what this spoke is allowed to serve, against the **live** set.
        let (allowed, pinned): (Vec<String>, HashMap<String, String>) = {
            let services = self.services.read().await;
            let mut allowed = Vec::new();
            let mut pinned = HashMap::new();
            for (name, live) in services.iter() {
                allowed.push(name.clone());
                if let Some(sp) = &live.spec.spoke {
                    pinned.insert(name.clone(), sp.clone());
                }
            }
            (allowed, pinned)
        };

        let mut granted = Vec::new();
        let mut denied = Vec::new();
        for decl in &hello.services {
            if !allowed.iter().any(|a| a == &decl.name) {
                denied.push(DeniedService {
                    name: decl.name.clone(),
                    reason: "not exposed by this hub (yet?)".into(),
                });
                continue;
            }
            if let Some(want) = pinned.get(&decl.name) {
                if want != &hello.spoke_id {
                    denied.push(DeniedService {
                        name: decl.name.clone(),
                        reason: format!("hub pins this service to spoke {want:?}"),
                    });
                    continue;
                }
            }
            granted.push(decl.name.clone());
        }

        protocol::write_msg(
            &mut ctrl,
            &Ack { version: PROTO_VERSION, granted: granted.clone(), denied: denied.clone() },
        )
        .await
        .context("cannot send Ack")?;

        for d in &denied {
            warn!(%peer, spoke = %hello.spoke_id, service = %d.name, reason = %d.reason, "service denied");
        }

        info!(
            %peer,
            spoke = %hello.spoke_id,
            agent = %hello.agent,
            client_cert_bytes = cert_bytes,
            granted = ?granted,
            declared = hello.services.len(),
            "spoke connected"
        );

        registry_register(&self.registry, &conn_key, &hello.spoke_id, &mux, &granted).await;

        let conn_cancel = CancellationToken::new();
        {
            let mut spokes = self.spokes.write().await;
            spokes.insert(
                conn_key.clone(),
                SpokeHandle {
                    spoke_id: hello.spoke_id.clone(),
                    peer: peer.to_string(),
                    agent: hello.agent.clone(),
                    declared: hello.services.iter().map(|s| s.name.clone()).collect(),
                    granted: granted.clone(),
                    since: Instant::now(),
                    cancel: conn_cancel.clone(),
                },
            );
        }

        // Keep the control stream alive for the connection's lifetime.
        let mut sink = [0u8; 256];
        let heartbeat = self.hub.heartbeat_secs.max(5);
        let mut idle = tokio::time::interval(Duration::from_secs(heartbeat));
        idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        idle.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                _ = conn_cancel.cancelled() => {
                    info!(spoke = %hello.spoke_id, "spoke kicked by control plane");
                    break;
                }
                _ = self.shutdown.cancelled() => break,
                r = ctrl.read(&mut sink) => match r {
                    Ok(0) => {
                        debug!(spoke = %hello.spoke_id, "control stream closed by peer");
                        break;
                    }
                    Ok(n) => {
                        debug!(spoke = %hello.spoke_id, bytes = n, "unexpected control frame (ignored)");
                    }
                    Err(e) => {
                        debug!(spoke = %hello.spoke_id, error = %e, "control stream read error");
                        break;
                    }
                },
                _ = idle.tick() => {
                    if registry_providers_alive(&self.registry, &granted).await == 0 && !granted.is_empty() {
                        debug!(spoke = %hello.spoke_id, "spoke vanished from registry");
                        break;
                    }
                }
            }
        }

        self.registry.unregister(&conn_key).await;
        self.spokes.write().await.remove(&conn_key);
        info!(spoke = %hello.spoke_id, "spoke disconnected");
        Ok(())
    }
}

async fn registry_register(
    registry: &Registry,
    conn_key: &str,
    spoke_id: &str,
    mux: &Mux,
    granted: &[String],
) {
    registry
        .register(
            Entry {
                conn_key: conn_key.to_string(),
                spoke_id: spoke_id.to_string(),
                mux: mux.clone(),
            },
            granted,
        )
        .await;
}

async fn registry_providers_alive(registry: &Registry, granted: &[String]) -> usize {
    let mut n = 0;
    for svc in granted {
        n += registry.providers(svc).await;
    }
    n
}

/// Poll for a provider for up to [`GRACE_PERIOD`].
async fn wait_for_provider(registry: &Registry, svc: &HubService, peer: SocketAddr) -> Option<Entry> {
    let deadline = tokio::time::Instant::now() + GRACE_PERIOD;
    loop {
        if let Some(e) = registry.pick(&svc.name, svc.spoke.as_deref()).await {
            debug!(service = %svc.name, %peer, "provider appeared during grace period");
            return Some(e);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Bridge one accepted public socket to a new mux stream towards the spoke.
async fn forward(
    sock: TcpStream,
    entry: Entry,
    service: String,
    stats: Arc<SvcStats>,
) -> Result<(u64, u64)> {
    let _ = sock.set_nodelay(true);

    let stream = entry
        .mux
        .open()
        .await
        .map_err(|e| anyhow::anyhow!("cannot open stream to spoke {}: {e}", entry.spoke_id))?;
    let mut stream = stream.compat();

    protocol::write_msg(&mut stream, &StreamHeader { service: service.clone() })
        .await
        .context("cannot send stream header")?;

    // 用带计数的搬运：面板在这个过程中就能读到流量，而不是等连接结束。
    let (up, down) = crate::pipe::duplex(sock, stream, &stats.up, &stats.down)
        .await
        .context("copy failed")?;
    Ok((up, down))
}

fn next_conn_seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}
