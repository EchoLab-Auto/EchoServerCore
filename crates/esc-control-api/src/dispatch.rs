//! 控制面契约：方法名、JSON 信封、以及引擎侧要实现的那个 trait。
//!
//! # 为什么要有这一层
//!
//! 面板（图形界面）与引擎（隧道内核）是两个**独立进程**，中间还隔着一个宿主。
//! 三方唯一共享的东西就是这个 crate：
//!
//! ```text
//!   esc-plugin-panel ──HTTP/REST──▶ 浏览器
//!          │  HostCall(esc.control, ControlRequest)
//!          ▼
//!   esc-host ──Invoke(esc.tunnel.control)──▶ esc-plugin-tunnel
//!                                                    │ dyn TunnelControl
//!                                                    ▼
//!                                              esc-engine
//! ```
//!
//! 面板不认识 `esc-engine`，引擎不认识 HTTP。只要 [`ControlRequest`] /
//! [`ControlResponse`] 的形状不变，两端可以各自发布新版本。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{
    ControlError, EngineStatus, ExportedConfig, MappingUpsert, MappingView, Role,
};

/// 宿主注册的服务名。面板通过 `HostCall` 调它。
pub const CONTROL_SERVICE: &str = "esc.control";

/// 面板的菜单/命令键。名字进 JSON，所以一律小写点分。
pub mod method {
    pub const STATUS: &str = "status";
    pub const MAPPING_LIST: &str = "mapping.list";
    pub const MAPPING_UPSERT: &str = "mapping.upsert";
    pub const MAPPING_DELETE: &str = "mapping.delete";
    pub const MAPPING_SET_ENABLED: &str = "mapping.set_enabled";
    pub const ENGINE_RELOAD: &str = "engine.reload";
    pub const SPOKES_KICK: &str = "spokes.kick";
    pub const CONFIG_EXPORT: &str = "config.export";
}

/// 一次控制调用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRequest {
    /// 见 [`method`]。
    pub method: String,
    /// 方法入参，结构由方法决定；缺省（`null`）表示无参。
    #[serde(default)]
    pub params: Value,
}

impl ControlRequest {
    pub fn new(method: impl Into<String>) -> Self {
        Self { method: method.into(), params: Value::Null }
    }
    pub fn with<T: Serialize>(method: impl Into<String>, params: T) -> Self {
        Self {
            method: method.into(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        }
    }
    /// 取强类型入参。缺省值用 `Value::Null`，因此带全部默认字段的结构体也能解。
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T, ControlError> {
        let raw = if self.params.is_null() { &Value::Object(Default::default()) } else { &self.params };
        serde_json::from_value(raw.clone())
            .map_err(|e| ControlError::invalid(format!("bad params for {}: {e}", self.method)))
    }
}

/// 一次控制调用的结果。
///
/// 用「一个结构体装两种结果」而不是 `Result`，是为了跨三次序列化边界
/// （引擎 → 插件帧 → 宿主 → HostCall 回包 → 面板）都保持同一形状。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ControlError>,
}

impl ControlResponse {
    pub fn ok<T: Serialize>(data: T) -> Self {
        Self {
            ok: true,
            data: serde_json::to_value(data).unwrap_or(Value::Null),
            error: None,
        }
    }
    pub fn err(error: ControlError) -> Self {
        Self { ok: false, data: Value::Null, error: Some(error) }
    }
    /// 把 `data` 解成强类型；`ok = false` 时直接把错误抛出来。
    pub fn into_result<T: serde::de::DeserializeOwned>(self) -> Result<T, ControlError> {
        if !self.ok {
            return Err(self.error.unwrap_or_else(|| ControlError::other("control call failed")));
        }
        serde_json::from_value(self.data)
            .map_err(|e| ControlError::other(format!("cannot decode control response: {e}")))
    }
}

/// 引擎实现这一组能力，控制面就完整了。
///
/// 之所以是 trait 而不是直接暴露具体类型：面板侧只需要它的**形状**
/// （编译期无需依赖 `esc-engine`），宿主侧也只需要转发 JSON。
#[async_trait::async_trait]
pub trait TunnelControl: Send + Sync + 'static {
    async fn status(&self) -> Result<EngineStatus, ControlError>;
    async fn list_mappings(&self) -> Result<Vec<MappingView>, ControlError>;
    /// 新建或更新一条映射（`id` 为空即新建），返回它的最新运行态。
    async fn upsert_mapping(&self, spec: MappingUpsert) -> Result<MappingView, ControlError>;
    async fn delete_mapping(&self, id: &str) -> Result<(), ControlError>;
    async fn set_mapping_enabled(&self, id: &str, enabled: bool) -> Result<MappingView, ControlError>;
    /// 丢掉所有运行态，从磁盘上的映射表重建。用于「配置在外部被改了」。
    async fn reload(&self) -> Result<EngineStatus, ControlError>;
    /// hub 角色：踢掉某条 spoke 连接（它通常会自动重连，从而重新握手拿到新授权）。
    /// `id` 为空表示踢掉全部。
    async fn kick_spokes(&self, id: Option<&str>) -> Result<usize, ControlError>;
    /// 生成一份静态 TOML，供 `esc hub` / `esc spoke` 直接使用。
    async fn export_config(&self, role: Role) -> Result<ExportedConfig, ControlError>;
}

/// 单点分发：把 [`ControlRequest`] 路由到 [`TunnelControl`] 的某个方法。
///
/// 引擎与测试共用这一个实现，保证「面板能调到的」和「引擎真正支持的」
/// 永远是同一份清单。
pub async fn dispatch(ctl: &dyn TunnelControl, req: ControlRequest) -> ControlResponse {
    use method::*;
    macro_rules! run {
        ($f:expr) => {
            match $f.await {
                Ok(v) => v,
                Err(e) => return ControlResponse::err(e),
            }
        };
    }
    match req.method.as_str() {
        STATUS => ControlResponse::ok(run!(ctl.status())),
        MAPPING_LIST => ControlResponse::ok(run!(ctl.list_mappings())),
        MAPPING_UPSERT => {
            let spec: MappingUpsert = match req.parse() {
                Ok(v) => v,
                Err(e) => return ControlResponse::err(e),
            };
            ControlResponse::ok(run!(ctl.upsert_mapping(spec)))
        }
        MAPPING_DELETE => {
            #[derive(Deserialize)]
            struct P {
                id: String,
            }
            let p: P = match req.parse() {
                Ok(v) => v,
                Err(e) => return ControlResponse::err(e),
            };
            match ctl.delete_mapping(&p.id).await {
                Ok(()) => ControlResponse { ok: true, data: Value::Null, error: None },
                Err(e) => ControlResponse::err(e),
            }
        }
        MAPPING_SET_ENABLED => {
            #[derive(Deserialize)]
            struct P {
                id: String,
                enabled: bool,
            }
            let p: P = match req.parse() {
                Ok(v) => v,
                Err(e) => return ControlResponse::err(e),
            };
            ControlResponse::ok(run!(ctl.set_mapping_enabled(&p.id, p.enabled)))
        }
        ENGINE_RELOAD => ControlResponse::ok(run!(ctl.reload())),
        SPOKES_KICK => {
            #[derive(Deserialize, Default)]
            struct P {
                #[serde(default)]
                id: Option<String>,
            }
            let p: P = match req.parse() {
                Ok(v) => v,
                Err(e) => return ControlResponse::err(e),
            };
            ControlResponse::ok(run!(ctl.kick_spokes(p.id.as_deref())))
        }
        CONFIG_EXPORT => {
            #[derive(Deserialize)]
            struct P {
                role: Role,
            }
            let p: P = match req.parse() {
                Ok(v) => v,
                Err(e) => return ControlResponse::err(e),
            };
            ControlResponse::ok(run!(ctl.export_config(p.role)))
        }
        other => ControlResponse::err(ControlError::invalid(format!(
            "unknown control method {other:?}"
        ))),
    }
}
