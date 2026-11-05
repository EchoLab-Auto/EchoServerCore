//! 插件配置：`listen` / `routes` / `default_port`。
//!
//! 配置从宿主来（`registry.set_config("esc.edge-router", json)`），所以在
//! TOML 里长这样：
//!
//! ```toml
//! [edge_router]
//! enabled = true
//! listen  = ["0.0.0.0:2229", "0.0.0.0:80"]
//! store   = "/var/lib/esc/edge-routes.toml"   # 可选：运行期改动的落盘位置
//!
//! [edge_router.routes]
//! cad   = 2228                     # cad.任意域名      -> 127.0.0.1:2228
//! panel = 8080                     # panel.任意域名    -> 127.0.0.1:8080
//! "cad.example.com" = 2229         # 完整域名精确匹配（键里带点即视为完整域名）
//! ```
//!
//! 校验在 [`RouterConfig::from_json`] 里一次做完并**fail-closed**：配置写错
//! 就不挂载，而不是起一个把所有请求都 421 的空路由。理由跟隧道插件一致 ——
//! 一个"起来了但什么都不转发"的边缘路由比没起来更难查。

use std::collections::BTreeMap;
use std::path::PathBuf;

use esc_control_api::ControlError;
use serde::Deserialize;

/// 前缀 / 域名的合法字符集：小写字母、数字、`-`、`_`、`.`（完整域名用）。
///
/// 之所以不让 `*` 之类进来：路由键是**精确匹配**的，通配到不存在的后端只会
/// 制造 502；真要泛域名，DNS 里加 `*.example.com` 即可，前缀仍然是具体的
/// `cad` / `panel`。
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 253
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && !key.starts_with('.')
        && !key.ends_with('.')
}

/// 一个监听地址。保留原始字符串是为了报错时能原样回显（`0.0.0.0:80` 比
/// `SocketAddr` 的 Debug 输出更好读），真正 bind 时再解析。
fn valid_listen(s: &str) -> bool {
    s.parse::<std::net::SocketAddr>().is_ok()
}

/// 插件配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterConfig {
    /// 监听地址列表，如 `["0.0.0.0:80", "0.0.0.0:2229"]`。
    pub listen: Vec<String>,
    /// 路由表：键是域名前缀（`cad`）或完整域名（`cad.example.com`），值是本机端口。
    pub routes: BTreeMap<String, u16>,
    /// 没有前缀（纯 IP 访问、或 Host 只有一段）时的兜底端口。
    pub default_port: Option<u16>,
    /// 转发超时（毫秒）。0 表示不设超时（长下载）。
    pub timeout_ms: u64,
    /// 路由表的**状态文件**路径（可选）。
    ///
    /// 配了它，运行期经面板/控制面的增删就会落盘，重启后仍在；没配则改动只活
    /// 在内存里，重启回到 `routes` 的基线。为什么是独立文件而不是回写
    /// `host.toml` —— 见 [`crate::store`] 的模块注释。
    pub store: Option<PathBuf>,
}

const DEFAULT_TIMEOUT_MS: u64 = 300_000;

/// `listen` 允许写成单个字符串或数组：`listen = "0.0.0.0:80"` 也认。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    listen: Option<OneOrMany>,
    #[serde(default)]
    routes: BTreeMap<String, u16>,
    #[serde(default)]
    default_port: Option<u16>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    store: Option<String>,
}

impl RouterConfig {
    /// 从插件配置 JSON 解析。空对象（`Null` 视为空对象）会报错：`listen`
    /// 必须有值 —— 默认绑一个特权端口（`:80`）比明确要求写出来更危险。
    pub fn from_json(cfg: &serde_json::Value) -> Result<Self, ControlError> {
        let cfg = if cfg.is_null() { &serde_json::json!({}) } else { cfg };
        let raw: RawConfig = serde_json::from_value(cfg.clone())
            .map_err(|e| ControlError::invalid(format!("edge_router 配置格式无效: {e}")))?;

        let listen = match raw.listen {
            None => {
                return Err(ControlError::invalid(
                    "edge_router.listen 不能为空（例：listen = [\"0.0.0.0:2229\"]）",
                ))
            }
            Some(OneOrMany::One(s)) => vec![s],
            Some(OneOrMany::Many(v)) => v,
        };
        if listen.is_empty() {
            return Err(ControlError::invalid("edge_router.listen 不能是空数组"));
        }
        for addr in &listen {
            if !valid_listen(addr) {
                return Err(ControlError::invalid(format!(
                    "edge_router.listen 里的 {addr:?} 不是合法的 host:port"
                )));
            }
        }

        // 前缀统一小写（Host 本来就是大小写不敏感的，nginx 也会转小写）。
        let mut routes = BTreeMap::new();
        for (key, port) in raw.routes {
            let key = key.trim().to_ascii_lowercase();
            if !valid_key(&key) {
                return Err(ControlError::invalid(format!(
                    "edge_router.routes 的键 {key:?} 只能含小写字母/数字/-/_/.，且不能以点开头结尾"
                )));
            }
            if port == 0 {
                return Err(ControlError::invalid(format!(
                    "edge_router.routes.{key} 的端口不能是 0"
                )));
            }
            routes.insert(key, port);
        }

        if let Some(0) = raw.default_port {
            return Err(ControlError::invalid("edge_router.default_port 不能是 0"));
        }

        // 状态文件路径：空字符串按"未配置"处理（写成 `store = ""` 不该
        // 变成"把路由写到当前目录下一个叫空名的文件"）。
        let store = raw
            .store
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);

        Ok(Self {
            listen,
            routes,
            default_port: raw.default_port,
            timeout_ms: raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            store,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_the_documented_toml_shape() {
        let cfg = RouterConfig::from_json(&json!({
            "listen": ["0.0.0.0:2229", "0.0.0.0:80"],
            "routes": { "cad": 2228, "panel": 8080 },
            "default_port": 9999
        }))
        .unwrap();
        assert_eq!(cfg.listen, vec!["0.0.0.0:2229", "0.0.0.0:80"]);
        assert_eq!(cfg.routes.get("cad"), Some(&2228));
        assert_eq!(cfg.routes.get("panel"), Some(&8080));
        assert_eq!(cfg.default_port, Some(9999));
        assert_eq!(cfg.timeout_ms, DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn listen_accepts_a_lone_string() {
        let cfg = RouterConfig::from_json(&json!({ "listen": "0.0.0.0:2229" })).unwrap();
        assert_eq!(cfg.listen, vec!["0.0.0.0:2229"]);
    }

    #[test]
    fn missing_listen_fails_instead_of_guessing_port_80() {
        let err = RouterConfig::from_json(&json!({ "routes": { "cad": 2228 } })).unwrap_err();
        assert_eq!(err.kind, esc_control_api::ErrorKind::Invalid);
        assert!(err.message.contains("listen"), "{err}");
    }

    #[test]
    fn bad_listen_and_bad_keys_are_rejected() {
        for bad in [
            json!({ "listen": ["nonsense"] }),
            json!({ "listen": [] }),
            json!({ "listen": ["0.0.0.0:1"], "routes": { "a b": 2228 } }),
            json!({ "listen": ["0.0.0.0:1"], "routes": { "*.x": 2228 } }),
            json!({ "listen": ["0.0.0.0:1"], "routes": { ".cad": 2228 } }),
            json!({ "listen": ["0.0.0.0:1"], "routes": { "cad": 0 } }),
            json!({ "listen": ["0.0.0.0:1"], "default_port": 0 }),
        ] {
            assert!(
                RouterConfig::from_json(&bad).is_err(),
                "应当拒绝：{bad}"
            );
        }
    }

    #[test]
    fn route_keys_are_lowercased_so_host_case_folding_is_harmless() {
        // JSON 里写成 CAD 也接受（转小写）：配置文件是人写的，不该因为大小写失败。
        let cfg = RouterConfig::from_json(&json!({
            "listen": ["0.0.0.0:1"], "routes": { "CAD": 2228 }
        }))
        .unwrap();
        assert_eq!(cfg.routes.get("cad"), Some(&2228));
    }
}
