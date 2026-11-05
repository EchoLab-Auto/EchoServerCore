//! 测试共用的小工具：一个极简 HTTP/1.1 客户端 + 证书夹具。
//!
//! 为什么不用 reqwest：面板只暴露几个 JSON 端点，裸 TCP 足够，而且不给
//! 依赖表添一条重量级条目。

#![allow(dead_code)]

use std::path::PathBuf;

/// 发一个 HTTP/1.1 请求，返回 `(状态码, 响应体)`。
///
/// 一律 `Connection: close`，所以读到 EOF 就是响应结束，不需要解析 chunked。
pub async fn http(
    addr: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
    auth: Option<&str>,
) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut sock = tokio::net::TcpStream::connect(addr)
        .await
        .unwrap_or_else(|e| panic!("connect {addr}: {e}"));
    let body = body.unwrap_or("");
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: Basic {a}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    sock.write_all(req.as_bytes()).await.unwrap();

    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("无法解析状态行: {text}"));
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

/// 把 `user:password` 编成 Basic 认证头里那一段 base64。
pub fn basic(user_pass: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(user_pass)
}

/// 一套可用的证书。
pub struct CertFixture {
    pub ca: PathBuf,
    /// hub 侧（serverAuth，SAN 含 127.0.0.1）
    pub hub_cert: PathBuf,
    pub hub_key: PathBuf,
    /// spoke 侧（clientAuth）
    pub spoke_cert: PathBuf,
    pub spoke_key: PathBuf,
    dir: PathBuf,
}

impl CertFixture {
    /// 只发 server 证书（面板/HTTP 层测试够用）。
    pub fn hub() -> Self {
        Self::make(true, false)
    }

    /// 两侧都发（要跑真实数据面时用）。
    pub fn both() -> Self {
        Self::make(true, true)
    }

    fn make(server: bool, client: bool) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "esc-panel-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ca = esc_common::pki::Ca::generate("panel-test-ca", 2).unwrap();
        let ca_path = esc_common::pki::write_ca_cert(&dir, "ca", &ca).unwrap();

        let (hub_cert, hub_key) = if server {
            let pair = ca
                .issue_server(
                    "hub",
                    &["127.0.0.1".to_string(), "localhost".to_string()],
                    1,
                )
                .unwrap();
            esc_common::pki::write_pair(&dir.join("hub"), "hub", &pair).unwrap()
        } else {
            (ca_path.clone(), ca_path.clone())
        };

        let (spoke_cert, spoke_key) = if client {
            let pair = ca.issue_client("e2e-spoke", 1).unwrap();
            esc_common::pki::write_pair(&dir.join("spoke"), "spoke", &pair).unwrap()
        } else {
            (hub_cert.clone(), hub_key.clone())
        };

        Self {
            ca: ca_path,
            hub_cert,
            hub_key,
            spoke_cert,
            spoke_key,
            dir,
        }
    }
}

impl Drop for CertFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
