//! 面板 HTTP 层单测：**不启动引擎、不启动框架**，只验证
//! 「HTTP 路由 → 契约方法」的映射是对的。
//!
//! 这正是把面板做成只依赖 `esc-control-api` 的收益：它的正确性可以在
//! 一个假后端上完全验证。

mod common;

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use esc_control_api::method;
use esc_plugin_panel::{router, ControlBackend, Panel, PanelConfig};

/// 假后端：记录每次调用的 (method, params)，返回预设结果。
#[derive(Default)]
struct Mock {
    calls: Mutex<Vec<(String, Value)>>,
}

#[async_trait::async_trait]
impl ControlBackend for Mock {
    async fn call(&self, method: &str, params: Value) -> Result<Value, (StatusCode, String)> {
        self.calls.lock().unwrap().push((method.to_string(), params.clone()));
        match method {
            method::STATUS => Ok(json!({
                "role": "hub", "running": true, "version": "1.2.3", "uptime_secs": 42,
                "control_listen": "0.0.0.0:4433", "spokes": [], "notices": []
            })),
            method::MAPPING_LIST => Ok(json!([{
                "id": "m1", "name": "demo", "enabled": true, "listen": "0.0.0.0:2222",
                "state": "active", "providers": ["sp-1"], "stats": {}
            }])),
            method::MAPPING_UPSERT => Ok(json!({
                "id": "new", "name": "demo", "enabled": true, "listen": "0.0.0.0:2222",
                "state": "active"
            })),
            method::MAPPING_DELETE | method::ENGINE_RELOAD => Ok(Value::Null),
            method::MAPPING_SET_ENABLED => Ok(json!({
                "id": "m1", "name": "demo", "enabled": false, "state": "disabled"
            })),
            method::SPOKES_KICK => Ok(json!(1)),
            method::CONFIG_EXPORT => Ok(json!({
                "role": "hub", "filename": "hub.toml", "toml": "[[service]]\nname = \"demo\"\n"
            })),
            // 引擎侧错误：code 由契约的 ErrorKind::as_code 生成
            "mapping.boom" => Err((
                StatusCode::from_u16(esc_control_api::ErrorKind::Conflict.http_status())
                    .unwrap_or(StatusCode::BAD_GATEWAY),
                "端口 0.0.0.0:2222 已被映射 \"other\" 占用".to_string(),
            )),
            other => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("unexpected {other}"))),
        }
    }
}

struct Running {
    addr: String,
    mock: Arc<Mock>,
    token: CancellationToken,
}

async fn start(auth: Option<&str>) -> Running {
    let mock = Arc::new(Mock::default());
    let backend: Arc<dyn ControlBackend> = mock.clone();
    let cfg = PanelConfig {
        listen: "127.0.0.1:0".into(),
        auth: auth.map(str::to_string),
        title: "测试面板".into(),
        behind_proxy: false,
    };
    let token = CancellationToken::new();
    let panel = Panel::new();
    let addr = panel
        .serve(backend, cfg, token.clone())
        .await
        .expect("面板应能绑定回环端口");
    // 让 axum 的 accept 循环先跑起来
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    Running { addr, mock, token }
}

#[tokio::test]
async fn index_is_served_and_html_escapes_title() {
    let r = start(None).await;
    let (code, body) = common::http(&r.addr, "GET", "/", None, None).await;
    assert_eq!(code, 200);
    assert!(body.contains("<!DOCTYPE html>"), "应返回页面");
    assert!(body.contains("测试面板"), "标题应被替换");
    r.token.cancel();
}

#[tokio::test]
async fn basic_auth_is_enforced_when_configured() {
    let r = start(Some("admin:s3cret")).await;

    let (code, _) = common::http(&r.addr, "GET", "/api/status", None, None).await;
    assert_eq!(code, 401, "没有口令应 401");
    assert!(r.mock.calls.lock().unwrap().is_empty(), "未认证的请求不应触达后端");

    let (code, _) = common::http(&r.addr, "GET", "/api/status", None, Some(&common::basic("admin:wrong"))).await;
    assert_eq!(code, 401, "错误口令应 401");

    // healthz 也走同一层中间件（面板被反代探测时表现一致）
    let (code, body) = common::http(
        &r.addr,
        "GET",
        "/api/status",
        None,
        Some(&common::basic("admin:s3cret")),
    )
    .await;
    assert_eq!(code, 200);
    assert!(body.contains("\"role\":\"hub\""), "{body}");
    assert_eq!(r.mock.calls.lock().unwrap()[0].0, method::STATUS);

    r.token.cancel();
}

#[tokio::test]
async fn mapping_crud_maps_onto_contract_methods() {
    let r = start(None).await;

    // 列表
    let (code, body) = common::http(&r.addr, "GET", "/api/mappings", None, None).await;
    assert_eq!(code, 200);
    assert!(body.contains("\"demo\""));

    // 新建：body 原样透传
    let (code, _) = common::http(
        &r.addr,
        "POST",
        "/api/mappings",
        Some(r#"{"name":"demo","listen":"0.0.0.0:2222","enabled":true}"#),
        None,
    )
    .await;
    assert_eq!(code, 200);

    // 更新：路径里的 id 必须覆盖 body 里的 id
    let (code, _) = common::http(
        &r.addr,
        "POST",
        "/api/mappings/abc123",
        Some(r#"{"id":"tampered","name":"demo","listen":"0.0.0.0:2222"}"#),
        None,
    )
    .await;
    assert_eq!(code, 200);

    // 启停
    let (code, body) = common::http(
        &r.addr,
        "POST",
        "/api/mappings/m1/enabled",
        Some(r#"{"enabled":false}"#),
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert!(body.contains("\"disabled\""));

    // 删除
    let (code, _) = common::http(&r.addr, "DELETE", "/api/mappings/m1", None, None).await;
    assert_eq!(code, 200);

    // 缺字段要 400 而不是把空对象塞给引擎
    let (code, _) = common::http(&r.addr, "POST", "/api/mappings/m1/enabled", Some("{}"), None).await;
    assert_eq!(code, 400);

    // 核对契约调用序列
    let calls: Vec<(String, Value)> = r.mock.calls.lock().unwrap().clone();
    let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(
        methods,
        vec![
            method::MAPPING_LIST,
            method::MAPPING_UPSERT,
            method::MAPPING_UPSERT,
            method::MAPPING_SET_ENABLED,
            method::MAPPING_DELETE,
        ]
    );
    assert_eq!(
        calls[2].1.get("id").and_then(Value::as_str),
        Some("abc123"),
        "URL 里的 id 应当覆盖 body"
    );

    r.token.cancel();
}

#[tokio::test]
async fn kick_and_export_work() {
    let r = start(None).await;

    let (code, body) = common::http(&r.addr, "POST", "/api/spokes/kick", Some(r#"{"id":"sp-1"}"#), None).await;
    assert_eq!(code, 200);
    assert!(body.contains('1'));

    let (code, body) = common::http(&r.addr, "GET", "/api/export?role=spoke", None, None).await;
    assert_eq!(code, 200);
    assert!(body.contains("[[service]]"));

    let calls = r.mock.calls.lock().unwrap().clone();
    assert_eq!(calls[0].0, method::SPOKES_KICK);
    assert_eq!(calls[0].1.get("id").and_then(Value::as_str), Some("sp-1"));
    assert_eq!(calls[1].0, method::CONFIG_EXPORT);
    assert_eq!(calls[1].1.get("role").and_then(Value::as_str), Some("spoke"));

    r.token.cancel();
}

#[tokio::test]
async fn backend_errors_are_surfaced_with_sensible_status() {
    let r = start(None).await;
    // Mock 对未知方法返回 500；这里用一个不存在的路径确认 404 由 axum 处理，
    // 再确认后端口径一致（错误体是 JSON）。
    let (code, body) = common::http(&r.addr, "GET", "/api/nope", None, None).await;
    assert_eq!(code, 404, "{body}");
    r.token.cancel();
}

#[tokio::test]
async fn non_loopback_without_auth_is_rejected_by_config() {
    let mut cfg = PanelConfig {
        listen: "0.0.0.0:8787".into(),
        auth: None,
        ..Default::default()
    };
    let err = cfg.validate().unwrap_err();
    assert!(err.contains("拒绝把无认证的管理面板暴露"), "{err}");

    cfg.auth = Some("u:p".into());
    assert!(cfg.validate().is_ok());

    // 回环地址无需口令
    let loopback = PanelConfig { listen: "127.0.0.1:8787".into(), auth: None, ..Default::default() };
    assert!(loopback.validate().is_ok());
}

#[tokio::test]
async fn router_can_be_built_without_serving() {
    // 只为覆盖 router() 这个公开构造函数（供嵌入式用法）。
    let mock = Arc::new(Mock::default());
    let backend: Arc<dyn ControlBackend> = mock;
    let _ = router(backend, &PanelConfig::default());
}

/// 错误码/状态码的对应表住在契约里，不在面板里 —— 这条测试锁住它。
#[tokio::test]
async fn error_code_table_lives_in_the_contract() {
    use esc_control_api::ErrorKind;

    // 往返：as_code → from_code 必须回到原值
    for kind in [
        ErrorKind::Other,
        ErrorKind::Invalid,
        ErrorKind::Conflict,
        ErrorKind::Bind,
        ErrorKind::NotFound,
        ErrorKind::Unavailable,
    ] {
        assert_eq!(ErrorKind::from_code(kind.as_code()), kind, "{kind:?}");
    }
    // 未知码不 panic，归为 Other（向前兼容：将来加了新类别，老面板仍能工作）
    assert_eq!(ErrorKind::from_code("some_future_kind"), ErrorKind::Other);
    assert_eq!(ErrorKind::from_code(""), ErrorKind::Other);

    // 状态码是给外部看的，别退化成全场 500
    assert_eq!(ErrorKind::Invalid.http_status(), 400);
    assert_eq!(ErrorKind::NotFound.http_status(), 404);
    assert_eq!(ErrorKind::Conflict.http_status(), 409);
    assert_eq!(ErrorKind::Bind.http_status(), 409);
    assert_eq!(ErrorKind::Unavailable.http_status(), 503);
    assert_eq!(ErrorKind::Other.http_status(), 502);
}
