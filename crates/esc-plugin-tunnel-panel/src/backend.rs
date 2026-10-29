//! 面板 → 后端的窄接口。
//!
//! HTTP 处理器不直接调 `HostClient`，而是面向这个只有两个方法的 trait：
//! 生产实现是 [`HostCallBackend`]，测试实现是内存假后端。于是「API 是否正确
//! 映射到契约方法」可以在没有框架、没有引擎的情况下单测。
//!
//! 为什么 `call_edge` 有默认实现：边缘路由是**可选插件**（默认不挂载）。
//! 假后端与"这台机器没开边缘路由"都不该被迫写一个永远返回错误的实现 ——
//! 默认返回 `501 Not Implemented` 恰好就是这两种情况的正确语义。

use axum::http::StatusCode;
use echo_plugin_sdk::HostClient;
use serde_json::Value;

use esc_control_api::{ErrorKind, CONTROL_SERVICE, EDGE_ROUTE_SERVICE};

/// 引擎侧的操作（bind、写映射表、查状态）都是毫秒级的，所以 10 秒已经是
/// 非常宽松的上限。默认的 30 秒会让浏览器在一个真的卡住的调用上白等半分钟，
/// 而这时用户想看到的是「引擎没响应」而不是转圈。
const CONTROL_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 面板对后端的全部依赖。
#[async_trait::async_trait]
pub trait ControlBackend: Send + Sync + 'static {
    /// 调隧道控制面（`esc.control`）。
    async fn call(&self, method: &str, params: Value) -> Result<Value, (StatusCode, String)>;

    /// 调边缘路由控制面（`esc.edge`）。
    ///
    /// 默认：`501` + 明确说明。这样"没装边缘路由插件"与"假后端"都得到同一个
    /// 诚实的答复，而不是伪装成 502/500 让用户去查日志。
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
    ///
    /// `service` 是宿主的服务名（`esc.control` / `esc.edge`）。两个服务的错误
    /// 映射完全一样，所以共用这一段，避免出现"隧道报 404、路由报 502"的漂移。
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
    // 用 `code` 而不是解析 `Display` 文本来决定状态码：错误文本会变，
    // 机器可读的 code 是契约的一部分。
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
            // `service_not_found` 不来自契约码表，它由框架发出：说明宿主根本
            // 没注册这个服务 = 该插件没启用。翻成 501（能力不存在），而不是
            // 让 ErrorKind::Other 的 502 把用户引向"后端坏了"。
            if code == "service_not_found" {
                return (
                    StatusCode::NOT_IMPLEMENTED,
                    format!(
                        "宿主没有注册 {service} 服务（该插件未启用）：{message}"
                    ),
                );
            }
            // 码表在契约里（`ErrorKind::as_code` / `http_status`），
            // 面板只管把码翻成状态码，不维护第二份字符串表。
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

    async fn call_edge(&self, method: &str, params: Value) -> Result<Value, (StatusCode, String)> {
        self.call_service(EDGE_ROUTE_SERVICE, method, params).await
    }
}
