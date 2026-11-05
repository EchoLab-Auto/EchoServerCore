//! 从隧道面板读取 `esc-host` 插件树的只读 HTTP 客户端。
//!
//! 服务器面板不是宿主插件，不能走 `HostCall`；它复用隧道面板已经认证过的
//! `/api/plugins` 作为窄上游。这里只转发 JSON，不接触引擎、Docker 或插件句柄。

use serde_json::Value;

#[derive(Clone)]
pub struct PluginTreeClient {
    client: reqwest::Client,
    url: reqwest::Url,
    auth: Option<(String, String)>,
}

impl PluginTreeClient {
    pub fn new(url: &str, auth: Option<&str>) -> Result<Self, String> {
        let url = reqwest::Url::parse(url).map_err(|e| format!("plugin_tree_url 无效: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("plugin_tree_url 只支持 http:// 或 https://".into());
        }
        if url.host_str().is_none() {
            return Err("plugin_tree_url 缺少主机名".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("不要把凭据写进 plugin_tree_url；请使用 plugin_tree_auth".into());
        }
        // 本机 HTTP 是正常部署；跨主机必须用 HTTPS，避免 Basic 凭据与插件信息明文上链路。
        if url.scheme() == "http" {
            let loopback = matches!(
                url.host_str(),
                Some("localhost" | "127.0.0.1" | "::1")
            );
            if !loopback {
                return Err(
                    "plugin_tree_url 使用非回环地址时必须用 HTTPS（明文 HTTP 仅允许本机）".into(),
                );
            }
        }
        let auth = auth
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|raw| {
                let (user, password) = raw
                    .split_once(':')
                    .ok_or_else(|| "plugin_tree_auth 应为 \"user:password\"".to_string())?;
                if user.is_empty() {
                    return Err("plugin_tree_auth 的用户名不能为空".to_string());
                }
                Ok((user.to_string(), password.to_string()))
            })
            .transpose()?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("无法创建插件树 HTTP 客户端: {e}"))?;
        Ok(Self { client, url, auth })
    }

    pub async fn fetch(&self) -> Result<Value, String> {
        let mut request = self.client.get(self.url.clone());
        if let Some((user, password)) = &self.auth {
            request = request.basic_auth(user, Some(password));
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("连接服务器核心插件树上失败: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "服务器核心插件树上返回 HTTP {status}{}；请检查 plugin_tree_url / plugin_tree_auth",
                if status == reqwest::StatusCode::UNAUTHORIZED {
                    "（认证失败）"
                } else {
                    ""
                }
            ));
        }
        response
            .json::<Value>()
            .await
            .map_err(|e| format!("服务器核心插件树不是有效 JSON: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_remote_upstream_and_url_credentials_are_rejected() {
        assert!(PluginTreeClient::new("http://127.0.0.1:8787/api/plugins", None).is_ok());
        assert!(PluginTreeClient::new("https://core.example/api/plugins", None).is_ok());
        assert!(PluginTreeClient::new("http://core.example/api/plugins", None).is_err());
        assert!(PluginTreeClient::new("http://admin:pw@127.0.0.1/api/plugins", None).is_err());
    }

    #[test]
    fn auth_shape_is_checked() {
        assert!(PluginTreeClient::new("http://127.0.0.1/api/plugins", Some("admin:pw")).is_ok());
        assert!(PluginTreeClient::new("http://127.0.0.1/api/plugins", Some("admin")).is_err());
    }
}
