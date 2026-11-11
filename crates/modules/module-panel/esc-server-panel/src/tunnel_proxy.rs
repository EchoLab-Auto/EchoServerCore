//! 把隧道面板的 REST API 转发进来，让「一个页面管全部」成为可能。
//!
//! # 为什么是服务端转发，而不是浏览器直接连 18787
//!
//! 隧道面板只监听 `127.0.0.1:18787`（它自己不做认证，靠回环隔离）。浏览器
//! 访问不到那个地址，也不该访问到——把它暴露出去等于把一个能改端口映射的
//! 管理面直接放到公网。
//!
//! 所以走这条窄缝：浏览器 → 本面板（已由网关认证）→ 回环上的隧道面板。
//!
//! # 转发哪些、转发多深
//!
//! 只转发 `/api/*`（也就是 JSON 接口）。**不转发**隧道面板自己的 HTML 和
//! `/ui/*` 静态资源——那些东西的意义就是被本面板的 Vue 取代了，留着两条
//! 前端路径只会让人以后分不清在改哪一个。
//!
//! # 方法白名单
//!
//! 只放行 GET / POST / DELETE。隧道面板目前不需要 PUT/PATCH，而"能转发任意
//! 方法"意味着以后上游加了个 `PUT /api/reset-everything` 会被自动暴露——
//! 白名单让新端点必须显式出现在这里。

use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};

#[derive(Clone)]
pub struct TunnelClient {
    client: reqwest::Client,
    /// 形如 `http://127.0.0.1:18787`（不带路径）。
    base: reqwest::Url,
}

impl TunnelClient {
    /// `base` 是隧道面板的根地址，例如 `http://127.0.0.1:18787`。
    pub fn new(base: &str) -> Result<Self, String> {
        let base = reqwest::Url::parse(base).map_err(|e| format!("tunnel_url 无效: {e}"))?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err("tunnel_url 只支持 http:// 或 https://".into());
        }
        if base.host_str().is_none() {
            return Err("tunnel_url 缺少主机名".into());
        }
        if !base.username().is_empty() || base.password().is_some() {
            return Err("不要把凭据写进 tunnel_url".into());
        }
        // 隧道面板自己能改端口映射，明文跨机等于把管理面交给链路上的任何人。
        // 跨主机必须 HTTPS；回环上的明文是本项目的标准部署。
        if base.scheme() == "http" {
            let loopback = matches!(base.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
            if !loopback {
                return Err(
                    "tunnel_url 使用非回环地址时必须用 HTTPS（明文 HTTP 仅允许本机）".into(),
                );
            }
        }
        let client = reqwest::Client::builder()
            // 端口映射的保存/删除要实打实地绑端口，给它 30 秒；
            // 再长就是上游卡死，早点报错比挂着强。
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("无法创建隧道面板 HTTP 客户端: {e}"))?;
        Ok(Self { client, base })
    }

    /// 转发一次请求。
    ///
    /// `path` 是**去掉** `/api/tunnel` 前缀之后的部分（含前导斜杠），
    /// `query` 原样带过去。
    pub async fn forward(
        &self,
        method: &Method,
        path: &str,
        query: Option<&str>,
        body: Vec<u8>,
    ) -> Response {
        let allowed = matches!(*method, Method::GET | Method::POST | Method::DELETE);
        if !allowed {
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                axum::Json(serde_json::json!({
                    "error": "隧道代理只放行 GET / POST / DELETE"
                })),
            )
                .into_response();
        }

        let mut url = match self.base.join(&format!("api{path}")) {
            Ok(url) => url,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({ "error": format!("路径不合法: {e}") })),
                )
                    .into_response()
            }
        };
        if let Some(q) = query {
            url.set_query(Some(q));
        }

        let mut request = self.client.request(
            match *method {
                Method::GET => reqwest::Method::GET,
                Method::POST => reqwest::Method::POST,
                _ => reqwest::Method::DELETE,
            },
            url,
        );
        if !body.is_empty() {
            request = request
                .header("content-type", "application/json")
                .body(body);
        }

        let response = match request.send().await {
            Ok(r) => r,
            Err(e) => {
                // 上游不可达要说清楚"是隧道面板连不上"，而不是让前端以为
                // 自己发错了请求。这两种错误前端处理方式完全不同。
                return (
                    StatusCode::BAD_GATEWAY,
                    axum::Json(serde_json::json!({
                        "error": format!("连接隧道面板失败: {e}。请确认 esc-host 在跑，且 tunnel_url 指向它的面板端口。")
                    })),
                )
                    .into_response();
            }
        };

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();
        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    axum::Json(serde_json::json!({
                        "error": format!("读取隧道面板响应失败: {e}")
                    })),
                )
                    .into_response()
            }
        };

        let mut out = Response::new(Body::from(bytes));
        *out.status_mut() = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        if let Ok(v) = content_type.parse() {
            out.headers_mut().insert("content-type", v);
        }
        // 端口映射随时在变，任何一层缓存留下它都会让人看到"已经删掉的端口还在"。
        out.headers_mut()
            .insert("cache-control", "no-store".parse().unwrap());
        out
    }
}


/// 只保留与业务有关的头。
///
/// 不转发 Cookie / Authorization：本面板自己的认证已经做完了，把凭据再往
/// 上游送一份没有任何好处，反而扩大了"凭据在多处可见"的面。
pub fn forwarded_headers(_incoming: &HeaderMap) -> Vec<(&'static str, &'static str)> {
    // 目前一个都不需要：隧道面板无认证、只吃 JSON。
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_plaintext_is_accepted() {
        assert!(TunnelClient::new("http://127.0.0.1:18787").is_ok());
        assert!(TunnelClient::new("https://tunnel.example.com").is_ok());
        // 跨机明文：把能改端口映射的管理面交给链路上的任何人
        assert!(TunnelClient::new("http://tunnel.example.com").is_err());
        // 凭据不该出现在 URL 里
        assert!(TunnelClient::new("http://a:b@127.0.0.1:18787").is_err());
        assert!(TunnelClient::new("不是地址").is_err());
    }

    /// 路径拼接：`/api/tunnel/mappings` 的剩余部分 `/mappings` 要变成
    /// 上游的 `/api/mappings`。
    #[test]
    fn path_join_keeps_the_api_prefix() {
        let c = TunnelClient::new("http://127.0.0.1:18787").unwrap();
        let joined = c.base.join(&format!("api{}", "/mappings")).unwrap();
        assert_eq!(joined.as_str(), "http://127.0.0.1:18787/api/mappings");
        let joined = c.base.join(&format!("api{}", "/mappings/abc/enabled")).unwrap();
        assert_eq!(
            joined.as_str(),
            "http://127.0.0.1:18787/api/mappings/abc/enabled"
        );
    }
}
