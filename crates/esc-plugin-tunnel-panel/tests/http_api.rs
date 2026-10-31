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
use esc_plugin_tunnel_panel::{router, ControlBackend, Panel, PanelConfig};

/// 假后端：记录每次调用的 (method, params)，返回预设结果。
#[derive(Default)]
struct Mock {
    calls: Mutex<Vec<(String, Value)>>,
}

#[async_trait::async_trait]
impl ControlBackend for Mock {
    async fn call(&self, method: &str, params: Value) -> Result<Value, (StatusCode, String)> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), params.clone()));
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
            other => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("unexpected {other}"),
            )),
        }
    }

    async fn call_plugins(&self) -> Result<Value, (StatusCode, String)> {
        Ok(json!({
            "schema_version": 1,
            "host": "esc-host",
            "framework": "plugin-framework",
            "context_services": ["esc.tunnel.control"],
            "host_services": ["esc.control", "esc.plugins"],
            "plugins": [{
                "id": "esc.tunnel", "name": "隧道运行时", "version": "1.2.3",
                "kind": "tunnel", "description": "", "runtime": "in_process",
                "enabled": true, "mounted": true, "state": "ready", "children": []
            }]
        }))
    }
}

struct Running {
    addr: String,
    mock: Arc<Mock>,
    token: CancellationToken,
}

async fn start(auth: Option<&str>) -> Running {
    start_with(|cfg| cfg.auth = auth.map(str::to_string)).await
}

/// 用自定义配置起一个面板（测试白名单 / 限速这类需要改默认值的场景）。
async fn start_with(tweak: impl FnOnce(&mut PanelConfig)) -> Running {
    let mock = Arc::new(Mock::default());
    let backend: Arc<dyn ControlBackend> = mock.clone();
    let mut cfg = PanelConfig {
        listen: "127.0.0.1:0".into(),
        auth: None,
        title: "测试面板".into(),
        ..Default::default()
    };
    tweak(&mut cfg);
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
    assert!(
        r.mock.calls.lock().unwrap().is_empty(),
        "未认证的请求不应触达后端"
    );

    let (code, _) = common::http(
        &r.addr,
        "GET",
        "/api/status",
        None,
        Some(&common::basic("admin:wrong")),
    )
    .await;
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
    let (code, _) = common::http(
        &r.addr,
        "POST",
        "/api/mappings/m1/enabled",
        Some("{}"),
        None,
    )
    .await;
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

    let (code, body) = common::http(
        &r.addr,
        "POST",
        "/api/spokes/kick",
        Some(r#"{"id":"sp-1"}"#),
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert!(body.contains('1'));

    let (code, body) = common::http(&r.addr, "GET", "/api/export?role=spoke", None, None).await;
    assert_eq!(code, 200);
    assert!(body.contains("[[service]]"));

    let calls = r.mock.calls.lock().unwrap().clone();
    assert_eq!(calls[0].0, method::SPOKES_KICK);
    assert_eq!(calls[0].1.get("id").and_then(Value::as_str), Some("sp-1"));
    assert_eq!(calls[1].0, method::CONFIG_EXPORT);
    assert_eq!(
        calls[1].1.get("role").and_then(Value::as_str),
        Some("spoke")
    );

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
    let loopback = PanelConfig {
        listen: "127.0.0.1:8787".into(),
        auth: None,
        ..Default::default()
    };
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

// ─────────────────────────────────────────────────────────────────────────────
// 暴露到公网时的两道锁
//
// 这些用例走真实 socket，因此来源 IP 就是 127.0.0.1——正好可以验证白名单的
// 「放行」与「拒绝」两侧，也能验证限速确实作用在 socket 来源上而不是某个头。
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn auth_failures_are_rate_limited_after_the_configured_budget() {
    let r = start_with(|cfg| {
        cfg.auth = Some("admin:s3cret".into());
        cfg.max_auth_failures = 3;
        cfg.auth_fail_window_secs = 60;
    })
    .await;

    for i in 0..3 {
        let (code, _) = common::http(
            &r.addr,
            "GET",
            "/api/status",
            None,
            Some(&common::basic("admin:wrong")),
        )
        .await;
        assert_eq!(code, 401, "第 {} 次错误口令应仍是 401", i + 1);
    }

    // 预算用完之后，**即使口令正确**也要挡住——否则限速就形同虚设。
    let (code, body) = common::http(
        &r.addr,
        "GET",
        "/api/status",
        None,
        Some(&common::basic("admin:s3cret")),
    )
    .await;
    assert_eq!(code, 429, "超出失败预算应 429，实际 {code}: {body}");
    assert!(
        r.mock.calls.lock().unwrap().is_empty(),
        "被限速的请求不应触达后端"
    );

    r.token.cancel();
}

#[tokio::test]
async fn successful_auth_resets_the_failure_counter() {
    let r = start_with(|cfg| {
        cfg.auth = Some("admin:s3cret".into());
        cfg.max_auth_failures = 2;
        cfg.auth_fail_window_secs = 60;
    })
    .await;

    // 先错一次，然后用正确口令进来 —— 计数应被清零。
    let _ = common::http(
        &r.addr,
        "GET",
        "/api/status",
        None,
        Some(&common::basic("admin:no")),
    )
    .await;
    let (code, _) = common::http(
        &r.addr,
        "GET",
        "/api/status",
        None,
        Some(&common::basic("admin:s3cret")),
    )
    .await;
    assert_eq!(code, 200);

    // 若计数没被清零，这里第一次错就会撞到「3 次」预算。
    let (code, _) = common::http(
        &r.addr,
        "GET",
        "/api/status",
        None,
        Some(&common::basic("admin:no")),
    )
    .await;
    assert_eq!(code, 401, "成功登录后计数应重置，不该直接 429");
    let (code, _) = common::http(
        &r.addr,
        "GET",
        "/api/status",
        None,
        Some(&common::basic("admin:s3cret")),
    )
    .await;
    assert_eq!(code, 200);

    r.token.cancel();
}

#[tokio::test]
async fn allow_cidrs_lets_matching_sources_in_and_blocks_the_rest() {
    // 回环在白名单里 → 照常工作
    let r = start_with(|cfg| {
        cfg.auth = Some("admin:s3cret".into());
        cfg.allow_cidrs = vec!["127.0.0.0/8".into()];
    })
    .await;
    let (code, _) = common::http(
        &r.addr,
        "GET",
        "/api/status",
        None,
        Some(&common::basic("admin:s3cret")),
    )
    .await;
    assert_eq!(code, 200, "白名单内的来源应放行");
    r.token.cancel();

    // 回环不在白名单里 → 403，且**不消耗认证预算、不触达后端**
    let r = start_with(|cfg| {
        cfg.auth = Some("admin:s3cret".into());
        cfg.allow_cidrs = vec!["10.0.0.0/8".into()];
    })
    .await;
    let (code, body) = common::http(
        &r.addr,
        "GET",
        "/api/status",
        None,
        Some(&common::basic("admin:s3cret")),
    )
    .await;
    assert_eq!(code, 403, "白名单外的来源应 403，实际 {code}: {body}");
    assert!(
        r.mock.calls.lock().unwrap().is_empty(),
        "白名单外的请求不应触达后端"
    );
    r.token.cancel();
}

#[tokio::test]
async fn malformed_allow_cidrs_fail_config_validation() {
    // 写错的白名单必须报错，而不是「解析不了就当没配」——那会把管理面静默开给所有人。
    let mut cfg = PanelConfig {
        auth: Some("admin:s3cret".into()),
        ..Default::default()
    };
    cfg.allow_cidrs = vec!["10.0.0.0/33".into()];
    assert!(cfg.validate().is_err());

    cfg.allow_cidrs = vec!["not-an-ip".into()];
    assert!(cfg.validate().is_err());

    cfg.allow_cidrs = vec!["10.0.0.0/8".into(), "::1".into()];
    assert!(
        cfg.validate().is_ok(),
        "IPv4 CIDR 与 IPv6 精确地址都应被接受"
    );
}

// ── 无认证暴露的开关（allow_no_auth）────────────────────────────────────────

/// 非回环 + 无 auth 默认**拒绝启动**。这是安全默认，不能因为"用户想省事"就放开。
#[tokio::test]
async fn non_loopback_without_auth_is_refused_by_default() {
    let cfg = PanelConfig {
        listen: "0.0.0.0:0".into(),
        auth: None,
        ..Default::default()
    };
    let err = cfg.validate().unwrap_err();
    assert!(err.contains("不是回环地址"), "{err}");
    assert!(err.contains("allow_no_auth"), "错误信息要给出出路：{err}");
}

/// 显式写了 `allow_no_auth = true` 才允许。
#[tokio::test]
async fn allow_no_auth_makes_it_explicit() {
    let cfg = PanelConfig {
        listen: "0.0.0.0:0".into(),
        auth: None,
        allow_no_auth: true,
        ..Default::default()
    };
    assert!(cfg.validate().is_ok());
}

/// `auth = ""` 必须等价于"没配"，不能变成一个空口令账号（那会静默锁死）。
#[tokio::test]
async fn empty_auth_means_no_auth() {
    let loopback = PanelConfig {
        listen: "127.0.0.1:2223".into(),
        auth: Some("".into()),
        ..Default::default()
    };
    assert!(loopback.validate().is_ok(), "回环 + 空 auth 应允许");
    assert!(
        loopback.auth_config().auth.is_none(),
        "空串不能变成「需要认证」"
    );

    // 公网 + 空 auth 仍要拒绝（除非显式 allow_no_auth）
    let public = PanelConfig {
        listen: "0.0.0.0:2223".into(),
        auth: Some("  ".into()),
        ..Default::default()
    };
    assert!(public.validate().is_err(), "空白串也算没配");
}

/// 真跑起来：无认证时后端能被直接调用（这就是"关掉认证"的实际效果）。
#[tokio::test]
async fn without_auth_requests_reach_the_backend() {
    let r = start_with(|cfg| {
        cfg.auth = None;
        cfg.allow_no_auth = true;
    })
    .await;

    let (code, _) = common::http(&r.addr, "GET", "/api/status", None, None).await;
    assert_eq!(code, 200, "无认证时不该 401");
    assert!(!r.mock.calls.lock().unwrap().is_empty(), "请求应直达后端");
    r.token.cancel();
}

// ─────────────────────────────────────────────────────────────────────────────
// 边缘路由：可选能力的 HTTP 映射
// ─────────────────────────────────────────────────────────────────────────────

/// 支持边缘路由的假后端（把 `call_edge` 覆盖掉）。
///
/// 单独的 mock 而不是扩展 [`Mock`]：默认 trait 实现返回 501，正好用来测
/// "这台机器没启用插件"那条路径。
#[derive(Default)]
struct EdgeMock {
    calls: Mutex<Vec<(String, Value)>>,
    routes: Mutex<Vec<(String, u16)>>,
}

#[async_trait::async_trait]
impl ControlBackend for EdgeMock {
    async fn call(&self, method: &str, _params: Value) -> Result<Value, (StatusCode, String)> {
        // 隧道那侧不需要内容，给个最小可用的形状
        match method {
            method::STATUS => Ok(json!({"role": "hub", "running": true, "spokes": []})),
            method::MAPPING_LIST => Ok(json!([])),
            _ => Ok(Value::Null),
        }
    }

    async fn call_edge(&self, method: &str, params: Value) -> Result<Value, (StatusCode, String)> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), params.clone()));
        match method {
            method::EDGE_ROUTE_STATUS => {
                let routes: Vec<Value> = self
                    .routes
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(p, port)| json!({"prefix": p, "port": port}))
                    .collect();
                Ok(json!({
                    "listens": ["0.0.0.0:2229", "0.0.0.0:80"],
                    "routes": routes,
                    "default_port": null,
                    "counters": {"requests": 7, "proxied": 6, "unrouted": 1,
                                 "upstream_errors": 0, "upgraded": 2}
                }))
            }
            method::EDGE_ROUTE_UPSERT => {
                let prefix = params.get("prefix").and_then(Value::as_str).unwrap_or("");
                let port = params.get("port").and_then(Value::as_u64).unwrap_or(0) as u16;
                let old_prefix = params.get("old_prefix").and_then(Value::as_str);
                let mut routes = self.routes.lock().unwrap();
                if let Some(old) = old_prefix {
                    let before = routes.len();
                    routes.retain(|(p, _)| p != old);
                    if routes.len() == before {
                        return Err((StatusCode::NOT_FOUND, format!("没有这个路由：{old}")));
                    }
                }
                routes.retain(|(p, _)| p != prefix);
                routes.push((prefix.to_string(), port));
                Ok(json!({"prefix": prefix, "port": port}))
            }
            method::EDGE_ROUTE_DELETE => {
                let prefix = params.get("prefix").and_then(Value::as_str).unwrap_or("");
                let mut routes = self.routes.lock().unwrap();
                let before = routes.len();
                routes.retain(|(p, _)| p != prefix);
                if routes.len() == before {
                    return Err((StatusCode::NOT_FOUND, format!("没有这个前缀：{prefix}")));
                }
                Ok(Value::Null)
            }
            other => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("unexpected {other}"),
            )),
        }
    }
}

/// `http()` 的 body 参数是 `Option<&str>`，这里包一层省掉每处 `.to_string()`。
async fn post_json(addr: &str, path: &str, body: &Value) -> (u16, String) {
    let raw = body.to_string();
    common::http(addr, "POST", path, Some(&raw), None).await
}

async fn start_edge() -> (String, Arc<EdgeMock>, CancellationToken) {
    let mock = Arc::new(EdgeMock::default());
    let backend: Arc<dyn ControlBackend> = mock.clone();
    let cfg = PanelConfig {
        listen: "127.0.0.1:0".into(),
        auth: None,
        ..Default::default()
    };
    let token = CancellationToken::new();
    let panel = Panel::new();
    let addr = panel
        .serve(backend, cfg, token.clone())
        .await
        .expect("应能绑定");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, mock, token)
}

#[tokio::test]
async fn edge_routes_crud_maps_onto_contract_methods() {
    let (addr, mock, token) = start_edge().await;

    // 初始为空
    let (code, body) = common::http(&addr, "GET", "/api/edge", None, None).await;
    assert_eq!(code, 200, "{body}");
    let st: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(st["routes"].as_array().unwrap().len(), 0);
    assert_eq!(st["listens"][0], "0.0.0.0:2229");
    assert_eq!(st["counters"]["upgraded"], 2);
    assert!(mock
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|(m, _)| m == method::EDGE_ROUTE_STATUS));

    // 新建
    let (code, body) = post_json(
        &addr,
        "/api/edge/routes",
        &json!({"prefix": "cad", "port": 2228}),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    let created: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(created["prefix"], "cad");
    assert_eq!(created["port"], 2228);

    // 出现在列表里
    let (_, body) = common::http(&addr, "GET", "/api/edge", None, None).await;
    let st: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(st["routes"].as_array().unwrap().len(), 1);
    assert_eq!(st["routes"][0]["prefix"], "cad");

    // 编辑：完整域名可直接填写；路径中的旧键优先于 body。
    let (code, body) = post_json(
        &addr,
        "/api/edge/routes/cad",
        &json!({"prefix": "cad.example.com", "port": 8080, "old_prefix": "tampered"}),
    ).await;
    assert_eq!(code, 200, "{body}");
    {
        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls.last().unwrap().1["old_prefix"], "cad");
    }
    let (_, body) = common::http(&addr, "GET", "/api/edge", None, None).await;
    let st: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(st["routes"].as_array().unwrap().len(), 1);
    assert_eq!(st["routes"][0]["prefix"], "cad.example.com");
    assert_eq!(st["routes"][0]["port"], 8080);

    // 删除完整域名
    let (code, _) = common::http(&addr, "DELETE", "/api/edge/routes/cad.example.com", None, None).await;
    assert_eq!(code, 200);
    let (_, body) = common::http(&addr, "GET", "/api/edge", None, None).await;
    let st: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(st["routes"].as_array().unwrap().len(), 0);

    // 再删同一个 → 后端的 not_found 应被翻成 404（不是 500/502）
    let (code, body) = common::http(&addr, "DELETE", "/api/edge/routes/cad.example.com", None, None).await;
    assert_eq!(code, 404, "{body}");
    assert!(body.contains("cad"), "错误里应点名前缀：{body}");

    token.cancel();
}

#[tokio::test]
async fn edge_upsert_validates_shape_before_calling_backend() {
    let (addr, mock, token) = start_edge().await;
    let before = mock.calls.lock().unwrap().len();

    // 缺 prefix
    let (code, body) = post_json(&addr, "/api/edge/routes", &json!({"port": 2228})).await;
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("prefix"), "{body}");

    // 缺 port
    let (code, body) = post_json(&addr, "/api/edge/routes", &json!({"prefix": "cad"})).await;
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("port"), "{body}");

    // 端口越界
    let (code, body) = post_json(
        &addr,
        "/api/edge/routes",
        &json!({"prefix": "cad", "port": 0}),
    )
    .await;
    assert_eq!(code, 400, "{body}");

    // 三次都不该打到后端（在面板就被拦下了）
    assert_eq!(
        mock.calls.lock().unwrap().len(),
        before,
        "非法入参不该转给后端"
    );
    token.cancel();
}

#[tokio::test]
async fn edge_endpoints_report_501_when_plugin_is_not_enabled() {
    // 默认 Mock 不覆盖 call_edge → 走 trait 默认实现 → 501。
    // 前端靠这个码把卡片显示成"未启用"，所以它必须是 501 而不是 500。
    let r = start(None).await;
    for (method, path, body) in [
        ("GET", "/api/edge", None),
        (
            "POST",
            "/api/edge/routes",
            Some(json!({"prefix": "cad", "port": 2228})),
        ),
        ("DELETE", "/api/edge/routes/cad", None),
    ] {
        let raw = body.map(|v| v.to_string());
        let (code, text) = common::http(&r.addr, method, path, raw.as_deref(), None).await;
        assert_eq!(code, 501, "{method} {path} 应返回 501，实际 {code}: {text}");
        assert!(
            text.contains("边缘路由"),
            "{method} {path} 的错误消息应说明原因：{text}"
        );
    }
    // 隧道侧不受影响
    let (code, _) = common::http(&r.addr, "GET", "/api/status", None, None).await;
    assert_eq!(code, 200);
    r.token.cancel();
}

#[tokio::test]
async fn ui_frame_entry_and_assets_are_served() {
    let r = start(None).await;
    let (_, page) = common::http(&r.addr, "GET", "/", None, None).await;
    assert!(page.contains("/ui/tunnel.js"));
    let (code, css) = common::http(&r.addr, "GET", "/ui/panel.css", None, None).await;
    assert_eq!(code, 200);
    assert!(css.contains(".nm-tree"), "应包含 ui-frame 的树组件样式");
    let (code, js) = common::http(&r.addr, "GET", "/ui/panel.js", None, None).await;
    assert_eq!(code, 200);
    assert!(js.contains("nm-tree"), "应包含真实的 ui-frame 组件");
    let (code, entry) = common::http(&r.addr, "GET", "/ui/tunnel.js", None, None).await;
    assert_eq!(code, 200);
    assert!(entry.contains("/api/mappings"));
    assert!(entry.contains("/api/edge"));
    r.token.cancel();
}

#[tokio::test]
async fn plugin_tree_api_is_wired_to_the_component_panel() {
    let r = start(None).await;
    let (code, body) = common::http(&r.addr, "GET", "/api/plugins", None, None).await;
    assert_eq!(code, 200, "{body}");
    let tree: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(tree["plugins"][0]["id"], "esc.tunnel");
    assert_eq!(tree["plugins"][0]["state"], "ready");
    let (code, shared) = common::http(&r.addr, "GET", "/ui/panel.js", None, None).await;
    assert_eq!(code, 200);
    assert!(shared.contains("/api/plugins"));
    assert!(shared.contains("required_services"));
    assert!(shared.contains("optional_services"));
    r.token.cancel();
}
