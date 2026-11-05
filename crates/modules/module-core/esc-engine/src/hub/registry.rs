//! Registry of currently-connected spokes, indexed by service name.
//!
//! The hub is the only authority on which services are exposed: a spoke may
//! *declare* anything it likes in its `Hello`, but only services that appear in
//! the hub's own, **live** service set are ever registered here.
//!
//! Compared to the pre-engine version this registry additionally supports
//! removing every entry for a service ([`Registry::drop_service`]) so that a
//! mapping deleted at runtime stops being routable immediately instead of
//! lingering until the spoke disconnects.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use esc_common::mux::Mux;

/// One live spoke connection.
#[derive(Clone)]
pub struct Entry {
    /// Unique per connection (a reconnecting spoke gets a new key), so a stale
    /// unregister cannot remove a fresh entry.
    pub conn_key: String,
    pub spoke_id: String,
    pub mux: Mux,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("conn_key", &self.conn_key)
            .field("spoke_id", &self.spoke_id)
            .finish_non_exhaustive()
    }
}

/// 一条连接的身份 + 打开流的句柄。
///
/// 单独存一份（而不是从 `services` 里找），是因为一个 spoke 可能
/// 一个服务都没被授权（`granted` 为空），但它随时可能在 hub 新增映射后
/// 被动态授权——那时仍然需要它的 `Mux`。
#[derive(Clone, Debug)]
struct ConnInfo {
    spoke_id: String,
    mux: Mux,
}

#[derive(Debug, Default)]
struct Inner {
    /// service name → connections able to serve it
    services: HashMap<String, Vec<Entry>>,
    /// conn_key → 连接信息，for O(1) cleanup 与动态授权
    conns: HashMap<String, ConnInfo>,
}

/// Thread-safe, cheaply cloneable registry.
#[derive(Clone, Default, Debug)]
pub struct Registry {
    inner: Arc<RwLock<Inner>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish `entry` under each of `services`.
    pub async fn register(&self, entry: Entry, services: &[String]) {
        let mut g = self.inner.write().await;
        g.conns.insert(
            entry.conn_key.clone(),
            ConnInfo { spoke_id: entry.spoke_id.clone(), mux: entry.mux.clone() },
        );
        for svc in services {
            g.services
                .entry(svc.clone())
                .or_default()
                .push(entry.clone());
        }
    }

    /// Grant additional services to an already-registered connection.
    ///
    /// Used when a mapping is added at runtime and the spoke had already
    /// declared that name: no reconnect is needed, the existing mux can serve
    /// it right away.
    pub async fn grant(&self, conn_key: &str, services: &[String]) {
        let mut g = self.inner.write().await;
        let Some(info) = g.conns.get(conn_key).cloned() else {
            return;
        };
        for svc in services {
            let list = g.services.entry(svc.clone()).or_default();
            if !list.iter().any(|e| e.conn_key == conn_key) {
                list.push(Entry {
                    conn_key: conn_key.to_string(),
                    spoke_id: info.spoke_id.clone(),
                    mux: info.mux.clone(),
                });
            }
        }
    }

    /// Remove every service mapping created by a single connection.
    pub async fn unregister(&self, conn_key: &str) -> Option<String> {
        let mut g = self.inner.write().await;
        let spoke_id = g.conns.remove(conn_key)?.spoke_id;
        g.services.retain(|_svc, entries| {
            entries.retain(|e| e.conn_key != conn_key);
            !entries.is_empty()
        });
        Some(spoke_id)
    }

    /// Forget a service entirely (called when its mapping is deleted/disabled).
    pub async fn drop_service(&self, service: &str) {
        let mut g = self.inner.write().await;
        g.services.remove(service);
    }

    /// Pick a live connection able to serve `service`.
    ///
    /// `want` pins the choice to one spoke id; `None` means "anyone who has it".
    /// Dead handles are pruned on the way through, so a spoke that vanished
    /// without closing cleanly stops being a candidate.
    pub async fn pick(&self, service: &str, want: Option<&str>) -> Option<Entry> {
        let mut g = self.inner.write().await;
        let entries = g.services.get_mut(service)?;

        let mut chosen = None;
        entries.retain(|e| {
            if !e.mux.is_alive() {
                return false;
            }
            if chosen.is_some() {
                return true;
            }
            match want {
                Some(id) if e.spoke_id != id => true,
                _ => {
                    chosen = Some(e.clone());
                    true
                }
            }
        });
        if entries.is_empty() {
            g.services.remove(service);
        }
        chosen
    }

    /// Number of live connections offering `service`.
    pub async fn providers(&self, service: &str) -> usize {
        self.inner
            .read()
            .await
            .services
            .get(service)
            .map(|v| v.iter().filter(|e| e.mux.is_alive()).count())
            .unwrap_or(0)
    }

    /// Sorted ids of the live providers of `service`.
    pub async fn provider_ids(&self, service: &str) -> Vec<String> {
        let g = self.inner.read().await;
        let mut ids: Vec<String> = g
            .services
            .get(service)
            .map(|v| {
                v.iter()
                    .filter(|e| e.mux.is_alive())
                    .map(|e| e.spoke_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Snapshot for logging / a `status` command.
    pub async fn snapshot(&self) -> Vec<(String, Vec<String>)> {
        let g = self.inner.read().await;
        let mut out: Vec<(String, Vec<String>)> = g
            .services
            .iter()
            .map(|(svc, entries)| {
                let mut ids: Vec<String> = entries.iter().map(|e| e.spoke_id.clone()).collect();
                ids.sort();
                ids.dedup();
                (svc.clone(), ids)
            })
            .collect();
        out.sort();
        out
    }
}
