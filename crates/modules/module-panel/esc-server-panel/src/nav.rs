//! 导航页：把「这台机器上有什么服务」变成一屏可以点进去的入口。
//!
//! # 为什么需要它
//!
//! 一台生产机上散落着隧道面板、服务器面板（就是你正在看的这个）、网关管理页、
//! 边缘路由后面的各个工具站。它们的端口号不在任何地方被记全，于是每次要用都得
//! 翻文档或者 `ss -ltn`。导航页把这件事收成配置里的几行。
//!
//! # 探测的语义（容易写错，先说清）
//!
//! **拿到任何一个 HTTP 状态码就算「活着」，只有连不上才是「挂了」。**
//!
//! 直觉会写成「200 才算 ok」，但那样 401/403/302 的服务全会被标红——而带认证的
//! 面板正常情况下**就是** 401/302。把一个活得好好的服务显示成宕机，比不显示更糟。
//!
//! 所以判据是「TCP 连上了、TLS 握完了、HTTP 回话了」，状态码是多少与死活无关。
//!
//! # 不跟随重定向
//!
//! 探测跟随 302 会被带到别处：可能是一个慢得多的地址（拖长整个接口），也可能
//! 是登录页（于是「服务活着」被误判成「服务把我踢了」）。302 本身就是**响亮的
//! 证据**说明对面活着，没有任何理由继续跟。

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// 一条导航条目。
///
/// 直接就是配置里 `[[nav]]` 的形状——没有中间层：配置里写什么，接口就返回什么，
/// 少一层转换就少一处「文档和实现不一致」的机会。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NavEntry {
    /// 显示名。
    pub name: String,
    /// 点开去的地址。
    pub url: String,
    /// 一句话说明（可选）。
    #[serde(default)]
    pub desc: Option<String>,
    /// 分组名（可选，默认「服务」）。
    #[serde(default)]
    pub group: Option<String>,
    /// 探测地址（可选，默认用 `url`）。
    ///
    /// 值得单独配置的典型场景：`url` 是公网 HTTPS 域名，而本机探测走回环明文
    /// 更快也更可靠（不依赖 DNS、不依赖证书）。
    #[serde(default)]
    pub check: Option<String>,
    /// 标签（可选）。
    #[serde(default)]
    pub tags: Vec<String>,
}

impl NavEntry {
    /// 探测用的地址。
    fn probe_url(&self) -> &str {
        self.check.as_deref().unwrap_or(&self.url)
    }

    fn group(&self) -> &str {
        self.group.as_deref().unwrap_or("服务")
    }

    /// 校验：只放行 http/https，且不接受内嵌凭据。
    ///
    /// 内嵌凭据（`https://user:pw@host/`）要看就得拒绝：导航页是给人点的，
    /// 把口令写进一个会被浏览器历史记录、书签、截图带走的地方不划算。
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("nav 条目的 name 不能为空".into());
        }
        for (field, raw) in [("url", Some(&self.url)), ("check", self.check.as_ref())] {
            let Some(raw) = raw else { continue };
            let parsed = reqwest::Url::parse(raw)
                .map_err(|e| format!("nav 条目「{}」的 {field} 无效: {e}", self.name))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(format!(
                    "nav 条目「{}」的 {field} 只支持 http:// 或 https://，收到 {:?}",
                    self.name, parsed.scheme()
                ));
            }
            if !parsed.username().is_empty() || parsed.password().is_some() {
                return Err(format!(
                    "nav 条目「{}」的 {field} 里不要写凭据；需要认证的页面会自己弹登录框",
                    self.name
                ));
            }
            if parsed.host_str().is_none() {
                return Err(format!("nav 条目「{}」的 {field} 缺少主机名", self.name));
            }
        }
        Ok(())
    }
}

/// 探测结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// 对面回话了（任何状态码）。
    Up,
    /// 连不上 / 超时 / TLS 失败。
    Down,
    /// 没配探测、或探测地址非法。
    Skip,
}

/// 一条带探测结果的条目（接口返回的形状）。
#[derive(Debug, Clone, Serialize)]
pub struct NavItem {
    pub name: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    pub group: String,
    pub tags: Vec<String>,
    pub status: Status,
    /// 探测耗时（毫秒）。`None` = 没探测。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    /// 失败原因（只有 `status = down` 时才有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 探测单个地址。
///
/// 见模块头：**任何 HTTP 状态码都算活着**。
async fn probe(client: &reqwest::Client, url: &str) -> (Status, Option<u64>, Option<String>) {
    let started = Instant::now();
    match client.get(url).send().await {
        Ok(response) => (
            Status::Up,
            Some(started.elapsed().as_millis() as u64),
            // 状态码不是死活判据，但记下来对排查有用（比如全是 502 说明上游挂了）。
            Some(format!("HTTP {}", response.status().as_u16())),
        ),
        Err(error) => (
            Status::Down,
            Some(started.elapsed().as_millis() as u64),
            Some(describe_reqwest_error(&error)),
        ),
    }
}

/// 把 reqwest 的错误压成一句人能看懂的话。
///
/// 默认的 `Display` 会套好几层（`error sending request for url (...): error trying
/// to connect ...: Connection refused`），在卡片上放不下也没人读。
fn describe_reqwest_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return "超时".into();
    }
    if error.is_connect() {
        // 往下挖一层拿最具体的原因；挖不到就用原文。
        let mut source: Option<&(dyn std::error::Error + 'static)> =
            std::error::Error::source(error);
        let mut deepest = String::new();
        while let Some(err) = source {
            deepest = err.to_string();
            source = err.source();
        }
        return if deepest.is_empty() {
            "连接失败".into()
        } else {
            format!("连接失败：{deepest}")
        };
    }
    if error.is_decode() {
        return "响应无法解析".into();
    }
    error.to_string()
}

/// 探测一批条目，返回带结果的分组。
///
/// 并发探测而不是串行：一个超时的条目不该把关掉 15 个正常条目的展示 2.5 秒。
/// 并发上限靠 `buffer_unordered` 之外的做法——这里条目数是个位到几十，
/// `join_all` 的代价可以忽略，不值得引入额外的限流器。
pub async fn probe_all(entries: &[NavEntry]) -> Vec<NavItem> {
    let client = match reqwest::Client::builder()
        // 单条 2.5 秒。比这更慢的服务，用户点进去也是等，标红是更诚实的信息。
        .timeout(Duration::from_secs(3))
        // 不跟随：见模块头。
        .redirect(reqwest::redirect::Policy::none())
        // 探测不携带任何 Cookie（这个客户端本来也没有），也接受自签证书：
        // 导航页要回答的是「进程活着吗」，不是「证书可信吗」——后者浏览器会告诉你。
        .danger_accept_invalid_certs(true)
        .user_agent(concat!("esc-server-panel/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(client) => client,
        // 连客户端都建不起来（几乎不可能），全部标 skip 并说明原因。
        Err(error) => {
            let detail = format!("无法创建探测客户端：{error}");
            return entries
                .iter()
                .map(|entry| NavItem {
                    name: entry.name.clone(),
                    url: entry.url.clone(),
                    desc: entry.desc.clone(),
                    group: entry.group().to_string(),
                    tags: entry.tags.clone(),
                    status: Status::Skip,
                    ms: None,
                    detail: Some(detail.clone()),
                })
                .collect();
        }
    };

    let futures = entries.iter().map(|entry| {
        let client = client.clone();
        async move {
            let (status, ms, detail) = probe(&client, entry.probe_url()).await;
            NavItem {
                name: entry.name.clone(),
                url: entry.url.clone(),
                desc: entry.desc.clone(),
                group: entry.group().to_string(),
                tags: entry.tags.clone(),
                status,
                ms,
                detail,
            }
        }
    });
    futures::future::join_all(futures).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(url: &str) -> NavEntry {
        NavEntry {
            name: "x".into(),
            url: url.into(),
            desc: None,
            group: None,
            check: None,
            tags: vec![],
        }
    }

    #[test]
    fn only_http_schemes_are_accepted() {
        assert!(entry("https://example.com/").validate().is_ok());
        assert!(entry("http://127.0.0.1:8080/").validate().is_ok());
        // 这些不是「不太合适」，是明确的错误：file:// 读本机文件、ssh:// 是另一个协议
        assert!(entry("file:///etc/shadow").validate().is_err());
        assert!(entry("ssh://host/").validate().is_err());
        assert!(entry("不是地址").validate().is_err());
    }

    #[test]
    fn credentials_in_url_are_rejected() {
        assert!(entry("https://admin:pw@example.com/").validate().is_err());
        assert!(entry("https://admin@example.com/").validate().is_err());
    }

    #[test]
    fn empty_name_is_rejected() {
        let mut e = entry("https://example.com/");
        e.name = "   ".into();
        assert!(e.validate().is_err());
    }

    #[test]
    fn check_falls_back_to_url_and_group_has_a_default() {
        let e = entry("https://example.com/");
        assert_eq!(e.probe_url(), "https://example.com/");
        assert_eq!(e.group(), "服务");

        let mut with_check = entry("https://example.com/");
        with_check.check = Some("http://127.0.0.1:9/".into());
        with_check.group = Some("设计工具".into());
        assert_eq!(with_check.probe_url(), "http://127.0.0.1:9/");
        assert_eq!(with_check.group(), "设计工具");
        assert!(with_check.validate().is_ok());
    }

    /// 探测一个必然关着的本地端口：必须是 Down，且带得出原因。
    ///
    /// 这条测的是「不会把连接失败误判成 up」——反过来（把 401 判成 down）由
    /// `any_http_status_counts_as_up` 守着。
    #[tokio::test]
    async fn closed_port_is_down() {
        let e = entry("http://127.0.0.1:1/");
        let items = probe_all(&[e]).await;
        assert_eq!(items[0].status, Status::Down);
        assert!(items[0].detail.is_some());
    }

    /// 起一个只会回 401 的极小服务，断言它被判为 up。
    ///
    /// 这正是真实世界的形状：服务器面板、隧道面板、网关管理页在未登录时都回
    /// 401/302。把它们标成宕机是最容易犯也最难发现的错。
    #[tokio::test]
    async fn any_http_status_counts_as_up() {
        use axum::http::StatusCode;
        use axum::routing::get;
        use axum::Router;

        let app = Router::new().route("/", get(|| async { StatusCode::UNAUTHORIZED }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let e = entry(&format!("http://{addr}/"));
        let items = probe_all(&[e]).await;
        assert_eq!(items[0].status, Status::Up, "{:?}", items[0].detail);
        assert_eq!(items[0].detail.as_deref(), Some("HTTP 401"));
    }

    /// 302 也算 up，而且**不能被跟随**。
    ///
    /// 顺手验证不跟随后端行为：目标地址指向一个必然关着的端口，如果客户端跟随了
    /// 就会变成 Down。
    #[tokio::test]
    async fn redirect_is_up_and_not_followed() {
        use axum::http::{header, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::get;
        use axum::Router;

        let app = Router::new().route(
            "/",
            get(|| async {
                (StatusCode::FOUND, [(header::LOCATION, "http://127.0.0.1:1/")]).into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let e = entry(&format!("http://{addr}/"));
        let items = probe_all(&[e]).await;
        assert_eq!(items[0].status, Status::Up, "{:?}", items[0].detail);
    }
}
