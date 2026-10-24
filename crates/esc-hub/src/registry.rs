//! Registry of currently-connected spokes, indexed by service name.
//!
//! The hub is the only authority on which services are exposed: a spoke may
//! *declare* anything it likes in its `Hello`, but only services that appear in
//! the hub's own `[[service]]` list are ever registered here.

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

#[derive(Debug, Default)]
struct Inner {
    /// service name → connections able to serve it
    services: HashMap<String, Vec<Entry>>,
    /// conn_key → declared spoke id, for O(1) cleanup
    by_conn: HashMap<String, String>,
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

    /// Publish `entry` under each of `services` with the given weight ordering.
    pub async fn register(&self, entry: Entry, services: &[String]) {
        let mut g = self.inner.write().await;
        g.by_conn
            .insert(entry.conn_key.clone(), entry.spoke_id.clone());
        for svc in services {
            g.services
                .entry(svc.clone())
                .or_default()
                .push(entry.clone());
        }
    }

    /// Remove every service mapping created by a single connection.
    pub async fn unregister(&self, conn_key: &str) -> Option<String> {
        let mut g = self.inner.write().await;
        let spoke_id = g.by_conn.remove(conn_key)?;
        g.services.retain(|_svc, entries| {
            entries.retain(|e| e.conn_key != conn_key);
            !entries.is_empty()
        });
        Some(spoke_id)
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
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Snapshot for logging / a future `esc status` command.
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;
    use yamux::Mode;

    /// A mux whose transport is a duplex pipe with the peer end leaked, so the
    /// connection stays "alive" for registry bookkeeping without any real I/O.
    fn fake_mux(label: &str) -> Mux {
        use tokio_util::compat::TokioAsyncReadCompatExt;
        let (a, b) = duplex(1024);
        let (mux, rx) = Mux::spawn(
            a.compat(),
            Mode::Client,
            esc_common::mux::default_config(8),
            label,
        );
        // Leak both the peer end and the stream receiver: this test only cares
        // about `is_alive()`, and leaking keeps the pipe open for its lifetime.
        std::mem::forget(b);
        std::mem::forget(rx);
        mux
    }

    fn entry(conn: &str, spoke: &str) -> Entry {
        Entry {
            conn_key: conn.to_string(),
            spoke_id: spoke.to_string(),
            mux: fake_mux(conn),
        }
    }

    #[tokio::test]
    async fn register_and_pick() {
        let reg = Registry::new();
        reg.register(entry("c1", "nas"), &["grafana".into()]).await;
        assert_eq!(reg.providers("grafana").await, 1);
        let picked = reg.pick("grafana", None).await.expect("should pick");
        assert_eq!(picked.spoke_id, "nas");
    }

    #[tokio::test]
    async fn unregister_removes_all_services() {
        let reg = Registry::new();
        reg.register(entry("c1", "nas"), &["a".into(), "b".into()]).await;
        assert_eq!(reg.unregister("c1").await.as_deref(), Some("nas"));
        assert!(reg.pick("a", None).await.is_none());
        assert!(reg.pick("b", None).await.is_none());
        assert_eq!(reg.providers("a").await, 0);
    }

    #[tokio::test]
    async fn stale_conn_key_does_not_remove_fresh_entry() {
        let reg = Registry::new();
        reg.register(entry("c1", "nas"), &["a".into()]).await;
        reg.register(entry("c2", "nas"), &["a".into()]).await;
        reg.unregister("c1").await;
        assert_eq!(reg.providers("a").await, 1);
        assert!(reg.pick("a", None).await.is_some());
    }

    #[tokio::test]
    async fn pinned_spoke_is_respected() {
        let reg = Registry::new();
        reg.register(entry("c1", "nas"), &["a".into()]).await;
        reg.register(entry("c2", "pi"), &["a".into()]).await;
        let picked = reg.pick("a", Some("pi")).await.unwrap();
        assert_eq!(picked.spoke_id, "pi");
        assert!(reg.pick("a", Some("nobody")).await.is_none());
    }
}
