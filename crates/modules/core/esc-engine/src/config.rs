//! 引擎配置文件：**既能读旧的 `hub.toml` / `spoke.toml`，也能带一张映射表**。
//!
//! 这是为了「改造不打断现有部署」：现网的两份配置文件一个字段都不用改，
//! 引擎按 `[hub]` / `[spoke]` 段落自动判断角色，并把原来的 `[[service]]`
//! 作为**初始映射**播种进映射表。之后的增删改都在映射表里，由面板负责。
//!
//! ```toml
//! # 角色可以显式写，也可以省略（看哪个段落在）
//! role = "hub"
//!
//! # 映射表存哪。省略时 = 本文件同目录下的 mappings.toml
//! store = "/etc/esc/mappings.toml"
//!
//! [hub]
//! listen = "0.0.0.0:4433"
//!
//! [tls]
//! ca_cert = "/etc/esc/ca.pem"
//! cert    = "/etc/esc/hub.pem"
//! key     = "/etc/esc/hub.key"
//!
//! # 首次启动时用来播种映射表；有 mappings.toml 之后就不再看了。
//! [[service]]
//! name   = "4090-8080"
//! listen = "0.0.0.0:2222"
//! spoke  = "evence-4090"
//! ```
//!
//! 也可以直接把映射写进这个文件（优先于 `[[service]]`）：
//!
//! ```toml
//! [[mapping]]
//! name    = "4090-8080"
//! enabled = true
//! listen  = "0.0.0.0:2222"
//! local   = "127.0.0.1:8080"
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

use esc_common::config::{HubSection, HubService, SpokeSection, SpokeService, TlsConfig};
use esc_control_api::{ControlError, Mapping, Role};

/// 磁盘上的原始形态。
#[derive(Debug, Clone, Deserialize)]
struct RawConfig {
    #[serde(default)]
    role: Option<Role>,
    tls: TlsConfig,
    #[serde(default)]
    hub: Option<HubSection>,
    #[serde(default)]
    spoke: Option<SpokeSection>,
    #[serde(default)]
    store: Option<PathBuf>,
    /// 旧的静态服务声明，用作播种。
    #[serde(default, rename = "service")]
    service: Vec<toml::Value>,
    /// 直接内联的映射，优先于 `[[service]]`。
    #[serde(default, rename = "mapping")]
    mapping: Vec<Mapping>,
}

/// 引擎启动所需的全部输入。
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub role: Role,
    pub tls: TlsConfig,
    pub hub: HubSection,
    pub spoke: SpokeSection,
    /// 映射表路径。`None` = 不落盘（CLI 的静态用法，行为与改造前完全一致）。
    pub store_path: Option<PathBuf>,
    /// 映射表为空时用来播种的记录。
    pub seed: Vec<Mapping>,
    /// 这份配置来自哪个文件（仅用于日志与导出）。
    pub source: Option<PathBuf>,
}

impl EngineConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ControlError> {
        let path = path.as_ref();
        let raw_text = std::fs::read_to_string(path)
            .map_err(|e| ControlError::other(format!("无法读取配置 {}: {e}", path.display())))?;
        Self::parse(&raw_text, Some(path))
    }

    /// 从 JSON 构造。给插件用（`Hello.config` / `MountContext.config` 都是 JSON）。
    ///
    /// 两种形态：
    ///
    /// ```json
    /// { "config": "/etc/esc/engine.toml", "store": "/etc/esc/mappings.toml" }
    /// ```
    ///
    /// 或者直接把 TOML 的字段写成 JSON（便于测试与宿主内联）。
    pub fn from_json(value: &serde_json::Value) -> Result<Self, ControlError> {
        if let Some(path) = value.get("config").and_then(|v| v.as_str()) {
            let mut cfg = Self::load(path)?;
            if let Some(store) = value.get("store").and_then(|v| v.as_str()) {
                cfg.store_path = Some(PathBuf::from(store));
            }
            if let Some(role) = value.get("role").and_then(|v| v.as_str()) {
                cfg.role = match role {
                    "hub" => Role::Hub,
                    "spoke" => Role::Spoke,
                    other => {
                        return Err(ControlError::invalid(format!(
                            "role 只能是 hub 或 spoke，收到 {other:?}"
                        )))
                    }
                };
            }
            return Ok(cfg);
        }
        let raw: RawConfig = serde_json::from_value(value.clone()).map_err(|e| {
            ControlError::invalid(format!("插件配置不是合法的引擎配置: {e}；也可以只写 {{\"config\": \"<toml 路径>\"}}"))
        })?;
        Self::from_raw(raw, None)
    }

    pub fn parse(text: &str, source: Option<&Path>) -> Result<Self, ControlError> {
        let raw: RawConfig = toml::from_str(text)
            .map_err(|e| ControlError::invalid(format!("配置解析失败: {e}")))?;
        Self::from_raw(raw, source)
    }

    fn from_raw(raw: RawConfig, source: Option<&Path>) -> Result<Self, ControlError> {

        // 角色：显式优先，否则看哪个段落存在。
        let role = match raw.role {
            Some(r) => r,
            None => match (&raw.hub, &raw.spoke) {
                (Some(_), None) => Role::Hub,
                (None, Some(_)) => Role::Spoke,
                (Some(_), Some(_)) => {
                    return Err(ControlError::invalid(
                        "配置里同时有 [hub] 和 [spoke]；请用 role = \"hub\"|\"spoke\" 明确指定",
                    ))
                }
                (None, None) => {
                    return Err(ControlError::invalid(
                        "配置里既没有 [hub] 也没有 [spoke]，无法判断角色",
                    ))
                }
            },
        };

        let store_path = match (&raw.store, source) {
            (Some(p), _) => Some(p.clone()),
            (None, Some(src)) => Some(
                src.parent()
                    .map(|d| d.join("mappings.toml"))
                    .unwrap_or_else(|| PathBuf::from("mappings.toml")),
            ),
            (None, None) => None,
        };

        // 播种映射
        let seed = if raw.mapping.is_empty() {
            match role {
                Role::Hub => raw
                    .service
                    .iter()
                    .filter_map(|v| v.clone().try_into::<HubService>().ok())
                    .map(|s| Mapping {
                        id: Mapping::new_id(),
                        name: s.name,
                        enabled: true,
                        listen: Some(s.listen),
                        spoke: s.spoke,
                        local: None,
                        note: String::new(),
                    })
                    .collect(),
                Role::Spoke => raw
                    .service
                    .iter()
                    .filter_map(|v| v.clone().try_into::<SpokeService>().ok())
                    .map(|s| Mapping {
                        id: Mapping::new_id(),
                        name: s.name,
                        enabled: true,
                        listen: None,
                        spoke: None,
                        local: Some(s.local),
                        note: String::new(),
                    })
                    .collect(),
            }
        } else {
            raw.mapping
                .into_iter()
                .map(|mut m| {
                    if m.id.is_empty() {
                        m.id = Mapping::new_id();
                    }
                    m
                })
                .collect()
        };

        Ok(Self {
            role,
            tls: raw.tls,
            hub: raw.hub.unwrap_or_default(),
            spoke: raw.spoke.unwrap_or_default(),
            store_path,
            seed,
            source: source.map(|p| p.to_path_buf()),
        })
    }

    /// 供测试与宿主内联配置使用的最小构造。
    pub fn for_hub(tls: TlsConfig, hub: HubSection, store_path: impl Into<Option<PathBuf>>) -> Self {
        Self {
            role: Role::Hub,
            tls,
            hub,
            spoke: SpokeSection::default(),
            store_path: store_path.into(),
            seed: Vec::new(),
            source: None,
        }
    }

    /// 从旧的 `HubConfig` 构造**静态**引擎配置：映射只来自配置文件，
    /// 不落盘、不持久化。给 `esc hub --config` 用，保证改造前后行为一致。
    pub fn from_hub_config(cfg: &esc_common::config::HubConfig) -> Self {
        Self {
            role: Role::Hub,
            tls: cfg.tls.clone(),
            hub: cfg.hub.clone(),
            spoke: SpokeSection::default(),
            store_path: None,
            seed: cfg
                .services
                .iter()
                .map(|s| Mapping {
                    id: Mapping::new_id(),
                    name: s.name.clone(),
                    enabled: true,
                    listen: Some(s.listen.clone()),
                    spoke: s.spoke.clone(),
                    local: None,
                    note: String::new(),
                })
                .collect(),
            source: None,
        }
    }

    /// 从旧的 `SpokeConfig` 构造静态引擎配置。
    pub fn from_spoke_config(cfg: &esc_common::config::SpokeConfig) -> Self {
        Self {
            role: Role::Spoke,
            tls: cfg.tls.clone(),
            hub: HubSection::default(),
            spoke: cfg.spoke.clone(),
            store_path: None,
            seed: cfg
                .services
                .iter()
                .map(|s| Mapping {
                    id: Mapping::new_id(),
                    name: s.name.clone(),
                    enabled: true,
                    listen: None,
                    spoke: None,
                    local: Some(s.local.clone()),
                    note: String::new(),
                })
                .collect(),
            source: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TLS: &str = r#"
[tls]
ca_cert = "/etc/esc/ca.pem"
cert    = "/etc/esc/hub.pem"
key     = "/etc/esc/hub.key"
"#;

    fn parse(body: &str) -> EngineConfig {
        EngineConfig::parse(body, Some(Path::new("/etc/esc/hub.toml"))).unwrap()
    }

    #[test]
    fn legacy_hub_config_is_still_valid() {
        let cfg = parse(&format!(
            r#"
[hub]
listen = "0.0.0.0:4433"

[[service]]
name   = "4090-8080"
listen = "0.0.0.0:2222"
spoke  = "evence-4090"
{TLS}"#
        ));
        assert_eq!(cfg.role, Role::Hub);
        assert_eq!(cfg.store_path.as_deref(), Some(Path::new("/etc/esc/mappings.toml")));
        assert_eq!(cfg.seed.len(), 1);
        assert_eq!(cfg.seed[0].name, "4090-8080");
        assert_eq!(cfg.seed[0].listen.as_deref(), Some("0.0.0.0:2222"));
        assert_eq!(cfg.seed[0].spoke.as_deref(), Some("evence-4090"));
    }

    #[test]
    fn legacy_spoke_config_is_still_valid() {
        let cfg = parse(&format!(
            r#"
[spoke]
id     = "evence-4090"
server = "114.132.241.46:4433"

[[service]]
name  = "4090-8080"
local = "127.0.0.1:8080"
{TLS}"#
        ));
        assert_eq!(cfg.role, Role::Spoke);
        assert_eq!(cfg.spoke.id, "evence-4090");
        assert_eq!(cfg.seed[0].local.as_deref(), Some("127.0.0.1:8080"));
    }

    #[test]
    fn ambiguous_config_is_rejected() {
        let err = EngineConfig::parse(
            &format!(
                "[hub]\nlisten = \"0.0.0.0:4433\"\n[spoke]\nserver = \"x:1\"\n{TLS}"
            ),
            None,
        )
        .unwrap_err();
        assert!(err.message.contains("同时有"));
    }

    #[test]
    fn inline_mappings_win_over_service_blocks() {
        let cfg = parse(&format!(
            r#"
[hub]
listen = "0.0.0.0:4433"

[[service]]
name   = "old"
listen = "0.0.0.0:1000"

[[mapping]]
name    = "new"
enabled = true
listen  = "0.0.0.0:2000"
local   = "127.0.0.1:3000"
{TLS}"#
        ));
        assert_eq!(cfg.seed.len(), 1);
        assert_eq!(cfg.seed[0].name, "new");
    }
}
