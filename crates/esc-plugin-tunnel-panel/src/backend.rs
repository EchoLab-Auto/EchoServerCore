//! 面板 → 后端的窄接口。
//!
//! HTTP 处理器不直接调 `HostClient`，而是面向这个小 trait：生产实现是
//! [`HostCallBackend`]，测试实现是内存假后端。于是 API 映射可以在没有框架、
//! 没有引擎的情况下单测。可选能力用默认实现返回 501，而不是伪装成后端故障。

use axum::http::StatusCode;
use echo_plugin_sdk::HostClient;
use serde_json::Value;

use esc_control_api::{
    ErrorKind, CONTROL_SERVICE, EDGE_ROUTE_SERVICE, PLUGIN_TREE_METHOD, PLUGIN_TREE_SERVICE,
};

/// 引擎侧操作都是毫秒级的，10 秒已是很宽松的上限。
const CONTROL_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 面板对后端的全部依赖。
#[async_trait::async_trait]
pub trait ControlBackend: Send + Sync + 'static {
    /// 调隧道控制面（`esc.control`）。
    async fn call(&self, method: &str, params: Value) -> Result<Value, (StatusCode, String)>;

    /// 读取宿主的插件树。无宿主能力的测试/嵌入后端明确返回 501。
    async fn call_plugins(&self) -> Result<Value, (StatusCode, String)> {
        Err((
            StatusCode::NOT_IMPLEMENTED,
            "此后端没有提供 esc.plugins/tree 插件树服务".to_string(),
        ))
    }

    /// 调边缘路由控制面（`esc.edge`）。
    async fn call_edge(&self, method: &str, params: Value) -> Result<Value, (StatusCode, String)> {
        let _ = (method, params);
        Err((
            StatusCode::NOT_IMPLEMENTED,
            "这台机器没有启用边缘路由插件（esc.edge-router）".to_string(),
        ))
    }
}

/// 生产实现：走插件框架的 `HostCall`。
pub struct HostCallBackend {
    host: HostClient,
}

impl HostCallBackend {
    pub fn new(host: HostClient) -> Self {
        Self { host }
    }

    /// 统一的一次 HostCall：把框架错误翻成 HTTP 状态码 + 消息。
    async fn call_service(
        &self,
        service: &'static str,
        method: &str,
        params: Value,
    ) -> Result<Value, (StatusCode, String)> {
        match self
            .host
            .call_with_timeout(service, method, params, CONTROL_CALL_TIMEOUT)
            .await
        {
            Ok(v) => Ok(v),
            Err(e) => Err(map_host_error(service, method, e)),
        }
    }
}

/// 框架错误 → (HTTP 状态码, 可读消息)。
fn map_host_error(
    service: &str,
    method: &str,
    e: echo_plugin_sdk::HostCallError,
) -> (StatusCode, String) {
    match e {
        echo_plugin_sdk::HostCallError::Timeout => (
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "后端在 {}s 内没有响应（{service}/{method}）——看宿主日志确认是否卡住",
                CONTROL_CALL_TIMEOUT.as_secs()
            ),
        ),
        echo_plugin_sdk::HostCallError::Closed => (
            StatusCode::SERVICE_UNAVAILABLE,
            "与宿主的连接已断开，面板即将退出".to_string(),
        ),
        echo_plugin_sdk::HostCallError::Service { code, message } => {
            // 服务不存在 = 对应能力未启用，不是引擎故障。
            if code == "service_not_found" {
                return (
                    StatusCode::NOT_IMPLEMENTED,
                    format!("宿主没有注册 {service} 服务（该插件未启用）：{message}"),
                );
            }
            let kind = ErrorKind::from_code(&code);
            let status =
                StatusCode::from_u16(kind.http_status()).unwrap_or(StatusCode::BAD_GATEWAY);
            (status, message)
        }
        other => (StatusCode::BAD_GATEWAY, other.to_string()),
    }
}

#[async_trait::async_trait]
impl ControlBackend for HostCallBackend {
    async fn call(&self, method: &str, params: Value) -> Result<Value, (StatusCode, String)> {
        self.call_service(CONTROL_SERVICE, method, params).await
    }

    async fn call_plugins(&self) -> Result<Value, (StatusCode, String)> {
        self.call_service(PLUGIN_TREE_SERVICE, PLUGIN_TREE_METHOD, Value::Null)
            .await
    }

    async fn call_edge(&self, method: &str, params: Value) -> Result<Value, (StatusCode, String)> {
        self.call_service(EDGE_ROUTE_SERVICE, method, params).await
    }
}
