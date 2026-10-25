//! 映射表：面板编辑的对象，也是引擎唯一的事实来源。
//!
//! 落盘格式刻意极简（就是一堆 `[[mapping]]`），好处是：
//!
//! * 运维可以直接用编辑器改，改完在面板点「重新加载」；
//! * git 可以 diff；
//! * 没有数据库依赖，`scp` 一份就是完整备份。
//!
//! 写入采用「写临时文件 + rename」，避免半截文件把服务卡在无法启动的状态。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use esc_control_api::{ControlError, Mapping, MappingUpsert};

/// 磁盘上的文件形态。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default, rename = "mapping")]
    mapping: Vec<Mapping>,
}

/// 映射表（内存 + 磁盘）。
pub struct MappingStore {
    path: Option<PathBuf>,
    inner: RwLock<Vec<Mapping>>,
}

impl MappingStore {
    /// 从磁盘加载；文件不存在视为空表（首次启动）。
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, ControlError> {
        let path = path.into();
        let mappings = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| ControlError::other(format!("无法读取映射表 {}: {e}", path.display())))?;
            let file: StoreFile = toml::from_str(&raw)
                .map_err(|e| ControlError::invalid(format!("无法解析映射表 {}: {e}", path.display())))?;
            file.mapping
        } else {
            Vec::new()
        };
        Ok(Self { path: Some(path), inner: RwLock::new(mappings) })
    }

    /// 纯内存表，测试与「不允许落盘」的场景用。
    pub fn in_memory(mappings: Vec<Mapping>) -> Self {
        Self { path: None, inner: RwLock::new(mappings) }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub async fn list(&self) -> Vec<Mapping> {
        self.inner.read().await.clone()
    }

    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }

    pub async fn get(&self, id: &str) -> Option<Mapping> {
        self.inner.read().await.iter().find(|m| m.id == id).cloned()
    }

    pub async fn find_by_name(&self, name: &str) -> Option<Mapping> {
        self.inner.read().await.iter().find(|m| m.name == name).cloned()
    }

    /// 新建或更新。`spec.id` 为空即新建。
    pub async fn upsert(&self, spec: MappingUpsert) -> Result<Mapping, ControlError> {
        let name = spec.name.trim().to_string();
        if name.is_empty() {
            return Err(ControlError::invalid("映射名不能为空"));
        }
        if let Some(listen) = spec.listen.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            listen
                .parse::<std::net::SocketAddr>()
                .map_err(|e| ControlError::invalid(format!("监听地址 {listen:?} 无效: {e}")))?;
        }
        if let Some(local) = spec.local.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if local.parse::<std::net::SocketAddr>().is_err() && !local.contains(':') {
                return Err(ControlError::invalid(format!(
                    "本地目标 {local:?} 需要 host:port 形式"
                )));
            }
        }

        let mut guard = self.inner.write().await;

        // 名字唯一：别的记录不能占用同一个 name
        if let Some(dup) = guard
            .iter()
            .find(|m| m.name == name && Some(&m.id) != spec.id.as_ref())
        {
            return Err(ControlError::conflict(format!(
                "服务名 {name:?} 已被另一条映射占用（id={}）",
                dup.id
            )));
        }
        // 监听地址唯一：两条映射抢同一个端口没有意义，提前拦下
        if let Some(listen) = spec.listen.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(dup) = guard.iter().find(|m| {
                m.listen.as_deref() == Some(listen) && Some(&m.id) != spec.id.as_ref()
            }) {
                return Err(ControlError::conflict(format!(
                    "监听地址 {listen:?} 已被映射 {:?} 占用",
                    dup.name
                )));
            }
        }

        let normalised = |id: String| Mapping {
            id,
            name: name.clone(),
            enabled: spec.enabled,
            listen: spec.listen.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
            spoke: spec.spoke.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
            local: spec.local.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
            note: spec.note.clone(),
        };

        let updated = match spec.id.as_ref().filter(|id| !id.is_empty()) {
            Some(id) => {
                let slot = guard
                    .iter_mut()
                    .find(|m| &m.id == id)
                    .ok_or_else(|| ControlError::not_found(format!("映射 id={id} 不存在")))?;
                *slot = normalised(id.clone());
                slot.clone()
            }
            None => {
                let m = normalised(Mapping::new_id());
                guard.push(m.clone());
                m
            }
        };

        self.persist_locked(&guard)
            .map_err(|e| ControlError::other(format!("保存映射表失败: {e}")))?;
        Ok(updated)
    }

    pub async fn remove(&self, id: &str) -> Result<Mapping, ControlError> {
        let mut guard = self.inner.write().await;
        let idx = guard
            .iter()
            .position(|m| m.id == id)
            .ok_or_else(|| ControlError::not_found(format!("映射 id={id} 不存在")))?;
        let removed = guard.remove(idx);
        self.persist_locked(&guard)
            .map_err(|e| ControlError::other(format!("保存映射表失败: {e}")))?;
        Ok(removed)
    }

    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<Mapping, ControlError> {
        let mut guard = self.inner.write().await;
        let slot = guard
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or_else(|| ControlError::not_found(format!("映射 id={id} 不存在")))?;
        slot.enabled = enabled;
        let out = slot.clone();
        self.persist_locked(&guard)
            .map_err(|e| ControlError::other(format!("保存映射表失败: {e}")))?;
        Ok(out)
    }

    /// 整体替换（首次播种、或从磁盘重载时用）。
    pub async fn replace_all(&self, mappings: Vec<Mapping>) -> Result<Vec<Mapping>, ControlError> {
        let mut guard = self.inner.write().await;
        *guard = mappings;
        self.persist_locked(&guard)
            .map_err(|e| ControlError::other(format!("保存映射表失败: {e}")))?;
        Ok(guard.clone())
    }

    /// 丢掉内存状态，重新读磁盘。
    pub async fn reload_from_disk(&self) -> Result<Vec<Mapping>, ControlError> {
        let Some(path) = &self.path else {
            return Ok(self.list().await);
        };
        if !path.exists() {
            return Ok(self.list().await);
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| ControlError::other(format!("无法读取映射表 {}: {e}", path.display())))?;
        let file: StoreFile = toml::from_str(&raw)
            .map_err(|e| ControlError::invalid(format!("无法解析映射表 {}: {e}", path.display())))?;
        let mut guard = self.inner.write().await;
        *guard = file.mapping;
        Ok(guard.clone())
    }

    /// 调用方已持有写锁。
    fn persist_locked(&self, mappings: &[Mapping]) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let body = toml::to_string_pretty(&StoreFile { mapping: mappings.to_vec() })
            .unwrap_or_default();
        let text = format!(
            "# esc 映射表 —— 面板编辑的就是这个文件。\n\
             # 手工改完在面板点「重新加载」即可生效，不需要重启。\n\n{body}"
        );
        let tmp = path.with_extension("tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            // 映射表可能含内网地址，按 0600 保存。
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
            }
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use esc_control_api::MappingUpsert;

    fn spec(name: &str, listen: &str) -> MappingUpsert {
        MappingUpsert {
            name: name.into(),
            enabled: true,
            listen: Some(listen.into()),
            local: Some("127.0.0.1:8080".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn upsert_is_idempotent_by_id_and_rejects_duplicate_names() {
        let store = MappingStore::in_memory(vec![]);
        let m = store.upsert(spec("a", "0.0.0.0:2001")).await.unwrap();
        assert_eq!(m.name, "a");

        // 同名再建 → 冲突
        let err = store.upsert(spec("a", "0.0.0.0:2002")).await.unwrap_err();
        assert!(err.message.contains("已被另一条映射占用"));

        // 同 id 更新 → 就地替换
        let updated = store
            .upsert(MappingUpsert { id: Some(m.id.clone()), ..spec("b", "0.0.0.0:2001") })
            .await
            .unwrap();
        assert_eq!(updated.id, m.id);
        assert_eq!(updated.name, "b");
        assert_eq!(store.list().await.len(), 1);
    }

    #[tokio::test]
    async fn duplicate_listen_is_rejected() {
        let store = MappingStore::in_memory(vec![]);
        store.upsert(spec("a", "0.0.0.0:2001")).await.unwrap();
        let err = store.upsert(spec("b", "0.0.0.0:2001")).await.unwrap_err();
        assert!(err.message.contains("监听地址"));
    }

    #[tokio::test]
    async fn persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("esc-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("mappings.toml");
        {
            let store = MappingStore::load(&path).unwrap();
            store.upsert(spec("a", "0.0.0.0:2001")).await.unwrap();
            store.upsert(spec("b", "0.0.0.0:2002")).await.unwrap();
        }
        let store = MappingStore::load(&path).unwrap();
        assert_eq!(store.list().await.len(), 2);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[mapping]]"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
