//! Spoke engine：内网侧的可编程内核。
//!
//! 与原 `esc_spoke::run` 的差别：
//!
//! * 要暴露的 `name → local` 表是**可变的**；改完立刻生效（见 [`SpokeEngine::set_services`]）。
//! * 变更触发一次重新握手，因为 hub 的授权（grant）发生在 `Hello` 阶段。
//!   重连成本很低（实测 hub 重启恢复 ~0.4 s），换来的是「改配置立即生效」这个
//!   面板必须具备的性质。
//! * 数据面每次开流都查实时表，所以即使不重连，改过的 `local` 也会���后续连接用上。
//! * 每个服务自带计数器，面板能显示走了多少字节。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::{watch, Mutex, RwLock};
use tokio_rustls::TlsConnector;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use yamux::Mode;

use esc_common::config::{SpokeSection, TlsConfig};
use esc_common::mux::{default_config, Mux};
use esc_common::protocol::{self, Ack, Hello, ServiceDecl, StreamHeader, PROTO_VERSION};
use esc_common::tls;
use esc_control_api::ControlError;

use crate::hub::SvcStats;

/// How often to send a keepalive comment frame on the control stream.
const CONTROL_PING: Duration = Duration::from_secs(20);

/// 一次连接尝试是怎么结束的。不同结局决定要不要重置退避。
enum Closed {
    /// 宿主让它停。
    Shutdown,
    /// 配置变了，正在重连。
    Reconfigured,
    /// 对端（hub）主动断开：重启 / 踢人 / 控制流 EOF。
    PeerGone,
}

struct SpokeInner {
    spoke: SpokeSection,
    tls: TlsConfig,
    /// 服务名 → 本地目标。可变。
    services: RwLock<HashMap<String, String>>,
    stats: RwLock<HashMap<String, Arc<SvcStats>>>,
    /// 每次改配置就 +1；连接尝试监听它，一变就主动断开重连。
    generation: watch::Sender<u64>,
    shutdown: CancellationToken,
    connected: AtomicBool,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    started: Instant,
}

/// 可克隆的 spoke 引擎句柄。
#[derive(Clone)]
pub struct SpokeEngine {
    inner: Arc<SpokeInner>,
}

impl SpokeEngine {
    pub async fn start(spoke: SpokeSection, tls: TlsConfig) -> Result<Self, ControlError> {
        let inner = Arc::new(SpokeInner {
            spoke,
            tls,
            services: RwLock::new(HashMap::new()),
            stats: RwLock::new(HashMap::new()),
            generation: watch::channel(0u64).0,
            shutdown: CancellationToken::new(),
            connected: AtomicBool::new(false),
            task: Mutex::new(None),
            started: Instant::now(),
        });
        let engine = Self { inner: inner.clone() };
        let task = tokio::spawn(async move { inner.run().await });
        *engine.inner.task.lock().await = Some(task);
        Ok(engine)
    }

    pub fn spoke_id(&self) -> String {
        self.inner.spoke.id.clone()
    }

    pub fn server(&self) -> String {
        self.inner.spoke.server.clone()
    }

    pub fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::Relaxed)
    }

    pub fn uptime(&self) -> Duration {
        self.inner.started.elapsed()
    }

    pub async fn services(&self) -> HashMap<String, String> {
        self.inner.services.read().await.clone()
    }

    pub async fn stats_for(&self, name: &str) -> Option<Arc<SvcStats>> {
        self.inner.stats.read().await.get(name).cloned()
    }

    /// 整体替换服务表。表变了就踢一次连接，让新的声明生效。
    pub async fn set_services(&self, map: HashMap<String, String>) {
        let changed = {
            let mut guard = self.inner.services.write().await;
            if *guard == map {
                false
            } else {
                *guard = map.clone();
                true
            }
        };
        {
            let mut stats = self.inner.stats.write().await;
            for name in map.keys() {
                stats.entry(name.clone()).or_insert_with(|| Arc::new(SvcStats::default()));
            }
            stats.retain(|k, _| map.contains_key(k));
        }
        if changed {
            self.inner.reconnect("services changed");
        }
    }

    /// 踢掉当前连接（自动重连）。
    pub fn reconnect(&self, why: &str) {
        self.inner.reconnect(why);
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        let task = self.inner.task.lock().await.take();
        if let Some(t) = task {
            t.abort();
        }
        info!("spoke engine stopped");
    }
}

impl SpokeInner {
    fn reconnect(self: &Arc<Self>, why: &str) {
        info!(reason = why, "forcing reconnect");
        let next = *self.generation.borrow() + 1;
        let _ = self.generation.send(next);
    }

    async fn run(self: Arc<Self>) {
        let connector = match tls::build_client_config(&self.tls) {
            Ok(c) => TlsConnector::from(c),
            Err(e) => {
                warn!(error = %format!("{e:#}"), "cannot build TLS client config; spoke idle");
                return;
            }
        };

        let mut backoff_ms = self.spoke.backoff_min_ms.max(1);
        let max_backoff = self.spoke.backoff_max_ms.max(backoff_ms);
        let mut attempt: u64 = 0;

        loop {
            if self.shutdown.is_cancelled() {
                break;
            }
            attempt += 1;
            info!(
                attempt,
                server = %self.spoke.server,
                spoke = %self.spoke.id,
                "connecting to hub"
            );

            // 每次尝试拿一份新的 generation 订阅：它代表「这次尝试的配置版本」。
            let mut gen_rx = self.generation.subscribe();
            let outcome = self.connect_once(&connector, &mut gen_rx).await;

            match outcome {
                Ok(Closed::Shutdown) => break,
                Ok(Closed::Reconfigured) => {
                    // 配置变了：立刻用新配置重连，重置退避。
                    backoff_ms = self.spoke.backoff_min_ms.max(1);
                }
                Ok(Closed::PeerGone) => {
                    info!("connection closed by hub, reconnecting");
                    backoff_ms = self.spoke.backoff_min_ms.max(1);
                }
                Err(e) => {
                    debug!(error = %format!("{e:#}"), "connection failed");
                }
            }
            self.connected.store(false, Ordering::Relaxed);

            // 抖动避免 hub 重启时的惊群。
            let jitter = backoff_ms / 5;
            let sleep_ms = if jitter > 0 {
                backoff_ms - jitter + (rand_u64() % (2 * jitter + 1))
            } else {
                backoff_ms
            };
            debug!(sleep_ms, "backing off");
            let mut gen_rx = self.generation.subscribe();
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = gen_rx.changed() => {
                    backoff_ms = self.spoke.backoff_min_ms.max(1);
                    continue;
                }
                _ = tokio::time::sleep(Duration::from_millis(sleep_ms)) => {}
            }
            backoff_ms = (backoff_ms.saturating_mul(2)).min(max_backoff);
        }
        self.connected.store(false, Ordering::Relaxed);
        info!("spoke run loop exited");
    }

    async fn connect_once(
        self: &Arc<Self>,
        connector: &TlsConnector,
        gen_rx: &mut watch::Receiver<u64>,
    ) -> Result<Closed> {
        let tcp = TcpStream::connect(&self.spoke.server)
            .await
            .with_context(|| format!("cannot connect to {}", self.spoke.server))?;
        let _ = tcp.set_nodelay(true);

        let server_name_raw = match &self.spoke.server_name {
            Some(n) => n.clone(),
            None => protocol::split_host_port(&self.spoke.server)
                .map(|(h, _)| h)
                .unwrap_or_else(|| self.spoke.server.clone()),
        };
        let server_name = tls::server_name(&server_name_raw)?;
        let stream = connector
            .connect(server_name, tcp)
            .await
            .context("TLS handshake with hub failed")?;

        let (mux, mut inbound) = Mux::spawn(
            stream.compat(),
            Mode::Client,
            default_config(self.spoke.max_streams),
            format!("hub:{}", self.spoke.server),
        );

        let ctrl = mux
            .open()
            .await
            .map_err(|e| anyhow::anyhow!("cannot open control stream: {e}"))?;
        let mut ctrl = ctrl.compat();

        let declared: Vec<ServiceDecl> = {
            let services = self.services.read().await;
            services
                .iter()
                .map(|(name, local)| ServiceDecl { name: name.clone(), local: local.clone() })
                .collect()
        };

        let hello = Hello {
            version: PROTO_VERSION,
            spoke_id: self.spoke.id.clone(),
            services: declared,
            agent: format!("esc/{}", env!("CARGO_PKG_VERSION")),
        };
        protocol::write_msg(&mut ctrl, &hello)
            .await
            .context("cannot send Hello")?;

        let ack: Ack = protocol::read_msg(&mut ctrl)
            .await
            .context("cannot read Ack")?;
        if ack.version != PROTO_VERSION {
            anyhow::bail!("hub speaks protocol {}, we speak {}", ack.version, PROTO_VERSION);
        }
        for d in &ack.denied {
            warn!(service = %d.name, reason = %d.reason, "hub denied service");
        }
        if ack.granted.is_empty() {
            warn!("hub granted no services; check that names match on both sides");
        }
        info!(granted = ?ack.granted, "connected");
        self.connected.store(true, Ordering::Relaxed);

        // 控制流拆成读/写两半：读侧发现「hub 走了」，写侧发保活帧。
        // 两者不能共用 `&mut`，否则下面的 select! 需要同一变量的两个可变借用。
        let (mut ctrl_r, mut ctrl_w) = tokio::io::split(ctrl);
        let mut sink = [0u8; 128];
        let mut ping = tokio::time::interval(CONTROL_PING);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await;

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => return Ok(Closed::Shutdown),
                _ = gen_rx.changed() => return Ok(Closed::Reconfigured),
                maybe = inbound.recv() => {
                    let Some(stream) = maybe else {
                        debug!("mux closed");
                        return Ok(Closed::PeerGone);
                    };
                    let me = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = me.handle_data_stream(stream).await {
                            debug!(error = %format!("{e:#}"), "forwarded stream failed");
                        }
                    });
                }
                r = ctrl_r.read(&mut sink) => match r {
                    Ok(0) => {
                        debug!("control stream closed by hub");
                        return Ok(Closed::PeerGone);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        debug!(error = %e, "control stream read error");
                        return Ok(Closed::PeerGone);
                    }
                },
                _ = ping.tick() => {
                    // 任何一次写都会刷新 yamux 的保活；空对象是最小帧。
                    if let Err(e) = protocol::write_msg(&mut ctrl_w, &serde_json::json!({})).await {
                        warn!(error = %e, "control ping failed");
                        return Ok(Closed::PeerGone);
                    }
                }
            }
        }
    }

    async fn handle_data_stream(self: Arc<Self>, stream: yamux::Stream) -> Result<()> {
        let mut stream = stream.compat();
        let header: StreamHeader = protocol::read_msg(&mut stream)
            .await
            .context("cannot read stream header")?;

        // 每次都查实时表：改过的 local 对后续连接立即生效。
        let local = {
            let services = self.services.read().await;
            services.get(&header.service).cloned()
        };
        let local = local.ok_or_else(|| {
            anyhow::anyhow!(
                "hub asked for service {:?} which is not configured here",
                header.service
            )
        })?;

        let stats = {
            let mut stats = self.stats.write().await;
            stats
                .entry(header.service.clone())
                .or_insert_with(|| Arc::new(SvcStats::default()))
                .clone()
        };
        stats.total.fetch_add(1, Ordering::Relaxed);
        stats.active.fetch_add(1, Ordering::Relaxed);
        struct Guard(Arc<SvcStats>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.active.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let _guard = Guard(stats.clone());

        let upstream = TcpStream::connect(&local)
            .await
            .with_context(|| format!("cannot connect to local target {local} for {:?}", header.service))?;
        let _ = upstream.set_nodelay(true);

        let (up, down) = crate::pipe::duplex(upstream, stream, &stats.up, &stats.down)
            .await
            .context("copy failed")?;
        debug!(service = %header.service, up, down, "stream finished");
        Ok(())
    }
}

/// 一个只在退避抖动里用的小 PRNG（不值得为此引入 `rand`）。
fn rand_u64() -> u64 {
    use std::cell::Cell;
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|s| {
        let mut v = s.get();
        if v == 0 {
            let mut h = RandomState::new().build_hasher();
            h.write_u128(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            );
            v = h.finish() | 1;
        }
        v ^= v >> 12;
        v ^= v << 25;
        v ^= v >> 27;
        s.set(v);
        v.wrapping_mul(0x2545_F491_4F6C_DD1D)
    })
}
