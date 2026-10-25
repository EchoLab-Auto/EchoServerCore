//! `esc-engine`：隧道内核，可在运行时被控制。
//!
//! 这一层完全不认识「插件」「Web」「宿主」——它只提供
//! [`TunnelControl`](esc_control_api::TunnelControl) 这个契约的实现。
//! 谁来调用它（CLI、插件、测试）它不关心。
//!
//! ```text
//!   esc-cli / esc-hub ──▶ Engine（静态配置，向后兼容）
//!   esc-plugin-tunnel ──▶ Engine（受面板驱动）
//! ```

pub mod config;
pub mod export;
pub mod hub;
pub mod pipe;
pub mod spoke;
pub mod store;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, RwLock};

use esc_common::config::{HubSection, HubService, SpokeSection, TlsConfig};
use esc_control_api::{
    ControlError, EngineStatus, ExportedConfig, Mapping, MappingState, MappingUpsert, MappingView,
    Role, TunnelControl,
};

pub use config::EngineConfig;
pub use hub::{HubEngine, SvcStats};
pub use spoke::SpokeEngine;
pub use store::MappingStore;

/// 引擎门面：把「角色 + 映射表 + 运行时」粘在一起，并实现控制面契约。
pub struct Engine {
    role: Role,
    tls: TlsConfig,
    hub_section: HubSection,
    spoke_section: SpokeSection,
    store: MappingStore,
    hub: Option<HubEngine>,
    spoke: Option<SpokeEngine>,
    /// 串行化所有变更操作：绑定端口 / 写映射表不能并发。
    ops: Mutex<()>,
    /// mapping id → 最近一次应用失败的原因。
    errors: RwLock<HashMap<String, String>>,
    running: AtomicBool,
    started: Instant,
    notices: RwLock<Vec<String>>,
}

impl Engine {
    pub async fn start(cfg: EngineConfig) -> Result<Arc<Self>, ControlError> {
        let store = match &cfg.store_path {
            Some(path) => {
                let store = MappingStore::load(path)?;
                // 首次启动：用配置里的 [[service]] 播种，之后映射表就是唯一事实来源。
                if store.is_empty().await && !cfg.seed.is_empty() {
                    let seeded = store.replace_all(cfg.seed.clone()).await?;
                    tracing::info!(
                        count = seeded.len(),
                        path = %path.display(),
                        "seeded mapping store from static config"
                    );
                }
                store
            }
            // 静态模式：配置就是事实，不落盘（`esc hub` / `esc spoke` 的既有语义）。
            None => MappingStore::in_memory(cfg.seed.clone()),
        };

        let (hub, spoke) = match cfg.role {
            Role::Hub => {
                let hub = HubEngine::start(cfg.hub.clone(), cfg.tls.clone()).await?;
                (Some(hub), None)
            }
            Role::Spoke => {
                let spoke = SpokeEngine::start(cfg.spoke.clone(), cfg.tls.clone()).await?;
                (None, Some(spoke))
            }
        };

        let engine = Arc::new(Self {
            role: cfg.role,
            tls: cfg.tls,
            hub_section: cfg.hub,
            spoke_section: cfg.spoke,
            store,
            hub,
            spoke,
            ops: Mutex::new(()),
            errors: RwLock::new(HashMap::new()),
            running: AtomicBool::new(true),
            started: Instant::now(),
            notices: RwLock::new(Vec::new()),
        });

        engine.reconcile().await;
        Ok(engine)
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// hub 角色的控制端口地址（spoke 角色返回 `None`）。
    pub fn control_addr(&self) -> Option<std::net::SocketAddr> {
        self.hub.as_ref().map(|h| h.control_addr())
    }

    pub async fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = &self.hub {
            h.shutdown().await;
        }
        if let Some(s) = &self.spoke {
            s.shutdown().await;
        }
    }

    pub async fn notices(&self) -> Vec<String> {
        self.notices.read().await.clone()
    }

    /// 把运行时对齐到映射表：多的摘掉、少的补上、变了的重绑。
    pub async fn reconcile(&self) {
        match self.role {
            Role::Hub => {
                if let Some(hub) = &self.hub {
                    let mappings = self.store.list().await;
                    let wanted: HashMap<String, Mapping> = mappings
                        .iter()
                        .filter(|m| m.enabled && m.listen.is_some())
                        .map(|m| (m.name.clone(), m.clone()))
                        .collect();

                    for name in hub.service_names().await {
                        if !wanted.contains_key(&name) {
                            let _ = hub.remove_service(&name).await;
                        }
                    }
                    for (name, m) in wanted {
                        let spec = HubService {
                            name: name.clone(),
                            listen: m.listen.clone().unwrap_or_default(),
                            spoke: m.spoke.clone(),
                        };
                        match hub.ensure_service(spec).await {
                            Ok(_) => {
                                self.errors.write().await.remove(&m.id);
                            }
                            Err(e) => {
                                tracing::warn!(mapping = %m.name, error = %e.message, "mapping failed to apply");
                                self.errors.write().await.insert(m.id.clone(), e.message);
                            }
                        }
                    }
                }
            }
            Role::Spoke => {
                if let Some(spoke) = &self.spoke {
                    let mappings = self.store.list().await;
                    let mut map = HashMap::new();
                    for m in mappings.iter().filter(|m| m.enabled) {
                        if let Some(local) = &m.local {
                            map.insert(m.name.clone(), local.clone());
                        } else {
                            self.errors
                                .write()
                                .await
                                .insert(m.id.clone(), "spoke 角色需要 local 字段".into());
                        }
                    }
                    spoke.set_services(map).await;
                }
            }
        }
    }

    async fn unapply(&self, mapping: &Mapping) {
        if let Some(hub) = &self.hub {
            let _ = hub.remove_service(&mapping.name).await;
        }
        // spoke 侧不需要单独撤销：reconcile 会按映射表重建。
    }

    async fn apply(&self, mapping: &Mapping) {
        match self.role {
            Role::Hub => {
                let Some(hub) = &self.hub else { return };
                if mapping.enabled {
                    if mapping.listen.is_none() {
                        self.errors.write().await.insert(
                            mapping.id.clone(),
                            "hub 角色的映射需要 listen 字段（公网监听地址）".into(),
                        );
                        return;
                    }
                    if let Some(listen) = &mapping.listen {
                        let spec = HubService {
                            name: mapping.name.clone(),
                            listen: listen.clone(),
                            spoke: mapping.spoke.clone(),
                        };
                        match hub.ensure_service(spec).await {
                            Ok(_) => {
                                self.errors.write().await.remove(&mapping.id);
                            }
                            Err(e) => {
                                tracing::warn!(mapping = %mapping.name, error = %e.message, "mapping failed to apply");
                                self.errors.write().await.insert(mapping.id.clone(), e.message);
                            }
                        }
                    }
                } else {
                    let _ = hub.remove_service(&mapping.name).await;
                    self.errors.write().await.remove(&mapping.id);
                }
            }
            Role::Spoke => {
                self.reconcile().await;
                self.errors.write().await.remove(&mapping.id);
            }
        }
    }

    async fn view_of(&self, m: &Mapping) -> MappingView {
        let errors = self.errors.read().await.clone();
        let err = errors.get(&m.id).cloned();

        match self.role {
            Role::Hub => {
                // 结构上不会发生（角色与引擎同时构造），但这里是状态查询路径，
                // 宁可返回一条 error 视图也不要 panic 掉整个进程。
                let Some(hub) = self.hub.as_ref() else {
                    return MappingView {
                        mapping: m.clone(),
                        state: MappingState::Error,
                        error: Some("引擎内部状态异常：hub 角色缺少 hub 引擎".into()),
                        bound: None,
                        providers: Vec::new(),
                        stats: Default::default(),
                    };
                };
                let live = hub.service_info(&m.name).await;
                let providers = hub.providers_of(&m.name).await;
                let state = if !m.enabled {
                    MappingState::Disabled
                } else if live.is_some() {
                    MappingState::Active
                } else if err.is_some() {
                    MappingState::Error
                } else {
                    MappingState::Pending
                };
                MappingView {
                    mapping: m.clone(),
                    state,
                    error: err,
                    bound: live.as_ref().map(|(addr, _)| addr.to_string()),
                    providers,
                    stats: live.map(|(_, s)| s.snapshot()).unwrap_or_default(),
                }
            }
            Role::Spoke => {
                let Some(spoke) = self.spoke.as_ref() else {
                    return MappingView {
                        mapping: m.clone(),
                        state: MappingState::Error,
                        error: Some("引擎内部状态异常：spoke 角色缺少 spoke 引擎".into()),
                        bound: None,
                        providers: Vec::new(),
                        stats: Default::default(),
                    };
                };
                let published = spoke.services().await.contains_key(&m.name);
                let state = if !m.enabled {
                    MappingState::Disabled
                } else if published {
                    MappingState::Active
                } else if err.is_some() {
                    MappingState::Error
                } else {
                    MappingState::Pending
                };
                let mut view = MappingView {
                    mapping: m.clone(),
                    state,
                    error: err,
                    bound: None,
                    providers: if published { vec![spoke.spoke_id()] } else { vec![] },
                    stats: Default::default(),
                };
                if let Some(s) = spoke.stats_for(&m.name).await {
                    view.stats = s.snapshot();
                }
                view
            }
        }
    }
}

#[async_trait::async_trait]
impl TunnelControl for Engine {
    async fn status(&self) -> Result<EngineStatus, ControlError> {
        let (control_listen, spokes) = match &self.hub {
            Some(h) => (Some(h.control_addr().to_string()), h.spokes().await),
            None => (None, Vec::new()),
        };
        let (hub_server, hub_connected) = match &self.spoke {
            Some(s) => (Some(s.server()), Some(s.is_connected())),
            None => (None, None),
        };
        let uptime = match (&self.hub, &self.spoke) {
            (Some(h), _) => h.uptime(),
            (_, Some(s)) => s.uptime(),
            _ => self.started.elapsed(),
        };
        Ok(EngineStatus {
            role: self.role,
            running: self.running.load(Ordering::Relaxed),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_secs: uptime.as_secs(),
            control_listen,
            hub_server,
            hub_connected,
            spokes,
            notices: self.notices().await,
        })
    }

    async fn list_mappings(&self) -> Result<Vec<MappingView>, ControlError> {
        let mappings = self.store.list().await;
        let mut out = Vec::with_capacity(mappings.len());
        for m in &mappings {
            out.push(self.view_of(m).await);
        }
        Ok(out)
    }

    async fn upsert_mapping(&self, spec: MappingUpsert) -> Result<MappingView, ControlError> {
        let _guard = self.ops.lock().await;
        let old = match spec.id.as_ref().filter(|s| !s.is_empty()) {
            Some(id) => self.store.get(id).await,
            None => None,
        };
        let mapping = self.store.upsert(spec).await?;
        // 改名了：旧名字的端口要放掉，否则会一直占着。
        if let Some(old) = &old {
            if old.name != mapping.name {
                self.unapply(old).await;
            }
        }
        self.apply(&mapping).await;
        Ok(self.view_of(&mapping).await)
    }

    async fn delete_mapping(&self, id: &str) -> Result<(), ControlError> {
        let _guard = self.ops.lock().await;
        let removed = self.store.remove(id).await?;
        self.unapply(&removed).await;
        self.errors.write().await.remove(&removed.id);
        Ok(())
    }

    async fn set_mapping_enabled(&self, id: &str, enabled: bool) -> Result<MappingView, ControlError> {
        let _guard = self.ops.lock().await;
        let mapping = self.store.set_enabled(id, enabled).await?;
        if enabled {
            self.apply(&mapping).await;
        } else {
            self.unapply(&mapping).await;
            self.errors.write().await.remove(&mapping.id);
        }
        Ok(self.view_of(&mapping).await)
    }

    async fn reload(&self) -> Result<EngineStatus, ControlError> {
        let _guard = self.ops.lock().await;
        self.store.reload_from_disk().await?;
        self.errors.write().await.clear();
        self.reconcile().await;
        self.notices
            .write()
            .await
            .push("已从磁盘重新加载映射表".to_string());
        self.status().await
    }

    async fn kick_spokes(&self, id: Option<&str>) -> Result<usize, ControlError> {
        let Some(hub) = &self.hub else {
            return Err(ControlError::unavailable("spoke 角色没有 spoke 连接可踢"));
        };
        Ok(hub.kick(id).await)
    }

    async fn export_config(&self, role: Role) -> Result<ExportedConfig, ControlError> {
        let mappings = self.store.list().await;
        Ok(match role {
            Role::Hub => export::export_hub(&mappings, &self.hub_section, &self.tls),
            Role::Spoke => export::export_spoke(&mappings, &self.spoke_section, &self.tls),
        })
    }
}

/// 引擎版本号，给面板显示。
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 给测试用的「未启动引擎」的最小构造：直接跑一个内存映射表。
pub async fn in_memory_hub(
    tls: TlsConfig,
    hub: HubSection,
    mappings: Vec<Mapping>,
) -> Result<Arc<Engine>, ControlError> {
    let hub_engine = HubEngine::start(hub.clone(), tls.clone()).await?;
    let engine = Arc::new(Engine {
        role: Role::Hub,
        tls,
        hub_section: hub,
        spoke_section: SpokeSection::default(),
        store: MappingStore::in_memory(mappings),
        hub: Some(hub_engine),
        spoke: None,
        ops: Mutex::new(()),
        errors: RwLock::new(HashMap::new()),
        running: AtomicBool::new(true),
        started: Instant::now(),
        notices: RwLock::new(Vec::new()),
    });
    engine.reconcile().await;
    Ok(engine)
}
