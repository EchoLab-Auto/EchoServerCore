//! Docker Engine API 的最小客户端：**只说 HTTP/1.1 over Unix socket**。
//!
//! 不用 `bollard` / `shiplift` 这类 SDK：它们会带进一整棵依赖树，而这里需要的
//! 只是「往 `/var/run/docker.sock` 发几个 GET 和 POST」。手写的代价是必须自己
//! 处理 chunked 编码与 Docker 的复用流格式，好处是**协议层面没有黑箱**——
//! 出问题时能直接 `curl --unix-socket` 对照。
//!
//! 支持的端点只覆盖面板真正用到的：
//!
//! | 方法 | 端点 | 用途 |
//! |---|---|---|
//! | GET | `/version`、`/info` | 是否装、什么版本 |
//! | GET | `/containers/json?all=1` | 容器列表 |
//! | GET | `/containers/{id}/json` | 单容器详情 |
//! | POST | `/containers/{id}/{start,stop,restart,pause,unpause}` | 启停 |
//! | GET | `/containers/{id}/logs` | 日志（解码复用流） |
//! | GET | `/containers/{id}/stats?stream=false` | 实时资源 |
//!
//! **刻意不实现**：创建/删除容器、exec、镜像 pull、volume/network 管理。
//! 面板的定位是「看不正常的东西并把它拉起来」，不是替代 Docker CLI；每多一个
//! 写操作就多一个能被滥用的入口，而删容器/起特权容器等于直接给 root。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// 单次请求的超时。Docker 在拉镜像时可能很慢，但面板只读状态和启停，
/// 5 秒足够；超时说明 daemon 卡住了，早点告诉使用者比挂着强。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Docker {
    sock: PathBuf,
}

#[derive(Debug)]
pub enum DockerError {
    /// socket 不存在 / 连不上 —— 通常等于「这台机器没装 Docker」。
    Unavailable(String),
    /// 连上了但请求失败（401、404、500、超时……）。
    Failed { status: u16, message: String },
    /// 响应不是合法 UTF-8 / JSON。
    Protocol(String),
}

impl std::fmt::Display for DockerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(m) => write!(f, "Docker 不可用：{m}"),
            Self::Failed { status, message } => write!(f, "Docker 返回 {status}：{message}"),
            Self::Protocol(m) => write!(f, "Docker 响应无法解析：{m}"),
        }
    }
}

impl Docker {
    pub fn new(sock: impl Into<PathBuf>) -> Self {
        Self { sock: sock.into() }
    }

    pub fn socket_path(&self) -> &Path {
        &self.sock
    }

    /// socket 是否存在。便宜，用于决定前端要不要显示 Docker 区块。
    pub fn socket_exists(&self) -> bool {
        self.sock.exists()
    }

    /// 探活 + 取版本。失败即视为「不可用」。
    pub async fn probe(&self) -> Result<Value, DockerError> {
        self.get_json("/version").await
    }

    pub async fn info(&self) -> Result<Value, DockerError> {
        self.get_json("/info").await
    }

    /// 容器列表（含已停止的）。
    pub async fn containers(&self) -> Result<Vec<Container>, DockerError> {
        let raw: Value = self.get_json("/containers/json?all=1").await?;
        let arr = raw
            .as_array()
            .ok_or_else(|| DockerError::Protocol("容器列表不是数组".into()))?;
        Ok(arr.iter().map(Container::from_api).collect())
    }

    pub async fn container(&self, id: &str) -> Result<Value, DockerError> {
        self.get_json(&format!("/containers/{}/json", enc(id))).await
    }

    /// 启停类动作。**只有这里列出来的动作能被调用**（见 `ALLOWED_ACTIONS`）。
    pub async fn action(&self, id: &str, action: &str) -> Result<(), DockerError> {
        if !ALLOWED_ACTIONS.contains(&action) {
            return Err(DockerError::Failed {
                status: 400,
                message: format!("不支持的操作 {action:?}"),
            });
        }
        let path = match action {
            "stop" | "restart" => format!("/containers/{}/{action}?t=10", enc(id)),
            _ => format!("/containers/{}/{action}", enc(id)),
        };
        let (status, body) = self.request("POST", &path, None).await?;
        // 204 = 成功且无内容；304 = 已经处于目标状态（Docker 用这个表示幂等）
        if status == 204 || status == 304 || (200..300).contains(&status) {
            Ok(())
        } else {
            Err(DockerError::Failed {
                status,
                message: error_message(&body),
            })
        }
    }

    /// 日志尾部。解码 Docker 的「复用流」帧格式。
    pub async fn logs(&self, id: &str, tail: u32, timestamps: bool) -> Result<String, DockerError> {
        let path = format!(
            "/containers/{}/logs?stdout=1&stderr=1&tail={}&timestamps={}",
            enc(id),
            tail.clamp(1, 2000),
            if timestamps { 1 } else { 0 }
        );
        let (status, body) = self.request("GET", &path, None).await?;
        if !(200..300).contains(&status) {
            return Err(DockerError::Failed {
                status,
                message: error_message(&body),
            });
        }
        Ok(decode_multiplexed(&body))
    }

    /// 单次资源采样。`stream=false` 时 Docker 仍会给出 `precpu_stats`，
    /// 于是可以算出这一瞬间的 CPU%。
    pub async fn stats(&self, id: &str) -> Result<ContainerStats, DockerError> {
        let raw: Value = self
            .get_json(&format!("/containers/{}/stats?stream=false", enc(id)))
            .await?;
        Ok(ContainerStats::from_api(&raw))
    }

    pub async fn images(&self) -> Result<Vec<Image>, DockerError> {
        let raw: Value = self.get_json("/images/json?all=0").await?;
        Ok(raw
            .as_array()
            .map(|a| a.iter().map(Image::from_api).collect())
            .unwrap_or_default())
    }

    async fn get_json(&self, path: &str) -> Result<Value, DockerError> {
        let (status, body) = self.request("GET", path, None).await?;
        if !(200..300).contains(&status) {
            return Err(DockerError::Failed {
                status,
                message: error_message(&body),
            });
        }
        serde_json::from_slice(&body)
            .map_err(|e| DockerError::Protocol(format!("{e}: {}", String::from_utf8_lossy(&body[..body.len().min(200)]))))
    }

    /// 发一个请求，返回 (状态码, body)。
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), DockerError> {
        let fut = self.request_inner(method, path, body);
        match tokio::time::timeout(REQUEST_TIMEOUT, fut).await {
            Ok(r) => r,
            Err(_) => Err(DockerError::Failed {
                status: 504,
                message: format!("请求 {method} {path} 超过 {}s 未返回", REQUEST_TIMEOUT.as_secs()),
            }),
        }
    }

    async fn request_inner(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), DockerError> {
        let mut stream = UnixStream::connect(&self.sock)
            .await
            .map_err(|e| DockerError::Unavailable(format!("{}: {e}", self.sock.display())))?;

        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n");
        if let Some(b) = body {
            req.push_str("Content-Type: application/json\r\n");
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        req.push_str("\r\n");

        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| DockerError::Unavailable(e.to_string()))?;
        if let Some(b) = body {
            stream
                .write_all(b)
                .await
                .map_err(|e| DockerError::Unavailable(e.to_string()))?;
        }
        stream
            .flush()
            .await
            .map_err(|e| DockerError::Unavailable(e.to_string()))?;

        let mut reader = BufReader::new(stream);

        // ── 状态行 ──
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .await
            .map_err(|e| DockerError::Protocol(e.to_string()))?
            == 0
        {
            return Err(DockerError::Protocol("对方没有返回状态行".into()));
        }
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| DockerError::Protocol(format!("状态行无法解析：{line:?}")))?;

        // ── 响应头 ──
        let mut content_length: Option<usize> = None;
        let mut chunked = false;
        loop {
            let mut h = String::new();
            let n = reader
                .read_line(&mut h)
                .await
                .map_err(|e| DockerError::Protocol(e.to_string()))?;
            if n == 0 || h == "\r\n" || h == "\n" {
                break;
            }
            let lower = h.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                content_length = v.trim().parse().ok();
            } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
                chunked = true;
            }
        }

        // ── body ──
        let mut body = Vec::new();
        if chunked {
            read_chunked(&mut reader, &mut body).await?;
        } else if let Some(len) = content_length {
            body.resize(len, 0);
            reader
                .read_exact(&mut body)
                .await
                .map_err(|e| DockerError::Protocol(format!("读取 {len} 字节响应体失败：{e}")))?;
        } else {
            // 既没长度也没分块：读到 EOF（我们发了 Connection: close，合法）
            reader
                .read_to_end(&mut body)
                .await
                .map_err(|e| DockerError::Protocol(e.to_string()))?;
        }

        Ok((status, body))
    }
}

/// 面板允许调用的容器动作。**这是唯一的允许清单**，加东西前先想清楚
/// 「攻击者拿到它之后能做什么」。
pub const ALLOWED_ACTIONS: &[&str] = &["start", "stop", "restart", "pause", "unpause"];

async fn read_chunked<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
) -> Result<(), DockerError> {
    loop {
        let mut size_line = String::new();
        let n = reader
            .read_line(&mut size_line)
            .await
            .map_err(|e| DockerError::Protocol(e.to_string()))?;
        if n == 0 {
            // 没有终止块就 EOF：Docker 在长连接被中断时会这样，当作结束而不是错误
            return Ok(());
        }
        let size_str = size_line.trim().split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).map_err(|_| {
            DockerError::Protocol(format!("分块长度无法解析：{size_line:?}"))
        })?;
        if size == 0 {
            // 读掉 trailer（可能为空行）
            loop {
                let mut t = String::new();
                let m = reader
                    .read_line(&mut t)
                    .await
                    .map_err(|e| DockerError::Protocol(e.to_string()))?;
                if m == 0 || t == "\r\n" || t == "\n" {
                    break;
                }
            }
            return Ok(());
        }
        let start = out.len();
        out.resize(start + size, 0);
        reader
            .read_exact(&mut out[start..])
            .await
            .map_err(|e| DockerError::Protocol(format!("读取分块 body 失败：{e}")))?;
        // 每块后面跟 CRLF
        let mut crlf = [0u8; 2];
        reader
            .read_exact(&mut crlf)
            .await
            .map_err(|e| DockerError::Protocol(e.to_string()))?;
    }
}

/// 解码 Docker 的「复用流」格式（非 TTY 容器的 logs/attach）。
///
/// 帧布局：`[stream:1][0][0][0][size:4 BE][payload:size]`，stream 1=stdout 2=stderr。
///
/// 但 TTY 容器返回的是**纯文本**。判别办法：逐帧解析，任何一帧不合法就退回
/// 「整段当文本」——这比先查容器配置再解码少一次往返，也不会因为配置过期出错。
fn decode_multiplexed(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0usize;
    let mut frames = 0usize;
    while i + 8 <= raw.len() {
        let stream = raw[i];
        let zeroed = raw[i + 1] == 0 && raw[i + 2] == 0 && raw[i + 3] == 0;
        let size = u32::from_be_bytes([raw[i + 4], raw[i + 5], raw[i + 6], raw[i + 7]]) as usize;
        if !zeroed || !(stream == 0 || stream == 1 || stream == 2) || i + 8 + size > raw.len() {
            return String::from_utf8_lossy(raw).into_owned();
        }
        out.extend_from_slice(&raw[i + 8..i + 8 + size]);
        i += 8 + size;
        frames += 1;
    }
    // 有残留尾巴（不足一帧）时，说明不是复用流
    if frames == 0 || i != raw.len() {
        return String::from_utf8_lossy(raw).into_owned();
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn error_message(body: &[u8]) -> String {
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        if let Some(m) = v.get("message").and_then(Value::as_str) {
            return m.to_string();
        }
    }
    let text = String::from_utf8_lossy(body);
    if text.trim().is_empty() {
        "(无响应体)".into()
    } else {
        text.chars().take(300).collect()
    }
}

fn enc(s: &str) -> String {
    // 容器 id/name 只含 [a-zA-Z0-9_.-]，直接放行；其余字符做百分号编码，
    // 避免把 `../` 之类拼进路径。
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'.' | b'-' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// 数据类型
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Container {
    pub id: String,
    /// `Names[0]` 去掉前导 `/`。
    pub name: String,
    pub image: String,
    pub image_id: String,
    pub state: String,
    pub status: String,
    pub created: i64,
    pub ports: Vec<PortMapping>,
    pub labels: Vec<(String, String)>,
    /// `compose` 项目名（有的话）——面板按它分组。
    pub compose_project: Option<String>,
    pub compose_service: Option<String>,
}

impl Container {
    fn from_api(v: &Value) -> Self {
        let name = v
            .get("Names")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_start_matches('/')
            .to_string();
        let labels: Vec<(String, String)> = v
            .get("Labels")
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let get_label = |key: &str| {
            labels
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        Self {
            id: v
                .get("Id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            name,
            image: v
                .get("Image")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            image_id: v
                .get("ImageID")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim_start_matches("sha256:")
                .chars()
                .take(12)
                .collect(),
            state: v
                .get("State")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            status: v
                .get("Status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            created: v.get("Created").and_then(Value::as_i64).unwrap_or(0),
            ports: v
                .get("Ports")
                .and_then(Value::as_array)
                .map(|a| {
                    // Docker 对同一个映射会给出 IPv4 与 IPv6 两条（0.0.0.0 与 ::），
                    // 面板上没必要重复显示——按 (私人端口, 公开端口, 协议) 去重。
                    let mut seen = std::collections::HashSet::new();
                    a.iter()
                        .map(|p| PortMapping {
                            ip: p
                                .get("IP")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            private: p.get("PrivatePort").and_then(Value::as_u64).unwrap_or(0) as u16,
                            public: p.get("PublicPort").and_then(Value::as_u64).map(|p| p as u16),
                            kind: p
                                .get("Type")
                                .and_then(Value::as_str)
                                .unwrap_or("tcp")
                                .to_string(),
                        })
                        .filter(|m| seen.insert((m.private, m.public, m.kind.clone())))
                        .collect()
                })
                .unwrap_or_default(),
            compose_project: get_label("com.docker.compose.project"),
            compose_service: get_label("com.docker.compose.service"),
            labels,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PortMapping {
    pub ip: String,
    pub private: u16,
    pub public: Option<u16>,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Image {
    pub id: String,
    pub tags: Vec<String>,
    pub size_bytes: u64,
    pub created: i64,
}

impl Image {
    fn from_api(v: &Value) -> Self {
        Self {
            id: v
                .get("Id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim_start_matches("sha256:")
                .chars()
                .take(12)
                .collect(),
            tags: v
                .get("RepoTags")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            size_bytes: v.get("Size").and_then(Value::as_u64).unwrap_or(0),
            created: v.get("Created").and_then(Value::as_i64).unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerStats {
    pub cpu_pct: f64,
    pub mem_used_bytes: u64,
    pub mem_limit_bytes: u64,
    pub mem_pct: f64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
    pub block_read_bytes: u64,
    pub block_write_bytes: u64,
    pub pids: u64,
}

impl ContainerStats {
    fn from_api(v: &Value) -> Self {
        let cpu = v.get("cpu_stats");
        let pre = v.get("precpu_stats");

        let total = cpu
            .and_then(|c| c.get("cpu_usage"))
            .and_then(|u| u.get("total_usage"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let pre_total = pre
            .and_then(|c| c.get("cpu_usage"))
            .and_then(|u| u.get("total_usage"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let sys = cpu
            .and_then(|c| c.get("system_cpu_usage"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let pre_sys = pre
            .and_then(|c| c.get("system_cpu_usage"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let online = cpu
            .and_then(|c| c.get("online_cpus"))
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get() as u64)
                    .unwrap_or(1)
            });

        let cpu_delta = total.saturating_sub(pre_total) as f64;
        let sys_delta = sys.saturating_sub(pre_sys) as f64;
        // 容器刚启动时 Docker 可能还没填 `precpu_stats`，此时 pre_* 全是 0。
        // 若照公式算，delta 会等于累计值本身，得出「1000% CPU」这种荒谬数字。
        // 所以缺少前值时明确报 0（前端显示「—」），而不是编一个看起来精确的错数。
        let has_previous = pre_total > 0 && pre_sys > 0;
        let cpu_pct = if has_previous && sys_delta > 0.0 && cpu_delta > 0.0 {
            (cpu_delta / sys_delta * online as f64 * 100.0).clamp(0.0, 100.0 * online as f64)
        } else {
            0.0
        };

        let mem = v.get("memory_stats");
        let mem_used = mem
            .and_then(|m| m.get("usage"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        // `usage` 含页缓存，直接显示会虚高；Docker CLI 也是减去 cache 的。
        let cache = mem
            .and_then(|m| m.get("stats"))
            .and_then(|s| s.get("inactive_file").or_else(|| s.get("cache")))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mem_used = mem_used.saturating_sub(cache);
        let mem_limit = mem
            .and_then(|m| m.get("limit"))
            .and_then(Value::as_u64)
            .unwrap_or(0);

        let mut rx = 0u64;
        let mut tx = 0u64;
        if let Some(nets) = v.get("networks").and_then(Value::as_object) {
            for (_, n) in nets {
                rx += n.get("rx_bytes").and_then(Value::as_u64).unwrap_or(0);
                tx += n.get("tx_bytes").and_then(Value::as_u64).unwrap_or(0);
            }
        }
        let mut br = 0u64;
        let mut bw = 0u64;
        if let Some(entries) = v
            .get("blkio_stats")
            .and_then(|b| b.get("io_service_bytes_recursive"))
            .and_then(Value::as_array)
        {
            for e in entries {
                let op = e.get("op").and_then(Value::as_str).unwrap_or("");
                let val = e.get("value").and_then(Value::as_u64).unwrap_or(0);
                match op.to_ascii_lowercase().as_str() {
                    "read" => br += val,
                    "write" => bw += val,
                    _ => {}
                }
            }
        }
        let pids = v
            .get("pids_stats")
            .and_then(|p| p.get("current"))
            .and_then(Value::as_u64)
            .unwrap_or(0);

        Self {
            cpu_pct,
            mem_used_bytes: mem_used,
            mem_limit_bytes: mem_limit,
            mem_pct: if mem_limit > 0 {
                mem_used as f64 / mem_limit as f64 * 100.0
            } else {
                0.0
            },
            net_rx_bytes: rx,
            net_tx_bytes: tx,
            block_read_bytes: br,
            block_write_bytes: bw,
            pids,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiplexed_frames_are_decoded_and_interleaved_in_order() {
        let mut raw = Vec::new();
        let mut frame = |stream: u8, payload: &str| {
            raw.push(stream);
            raw.extend_from_slice(&[0, 0, 0]);
            raw.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            raw.extend_from_slice(payload.as_bytes());
        };
        frame(1, "hello ");
        frame(2, "oops\n");
        frame(1, "world\n");
        assert_eq!(decode_multiplexed(&raw), "hello oops\nworld\n");
    }

    #[test]
    fn plain_text_logs_from_tty_containers_are_passed_through() {
        // TTY 容器不走复用流，日志就是裸文本
        let raw = b"line one\nline two\n";
        assert_eq!(decode_multiplexed(raw), "line one\nline two\n");
    }

    #[test]
    fn truncated_frame_falls_back_to_text_instead_of_dropping_data() {
        // 声称 100 字节但只有 3 字节 → 不能按帧解析，宁可原样返回
        let mut raw = vec![1u8, 0, 0, 0];
        raw.extend_from_slice(&100u32.to_be_bytes());
        raw.extend_from_slice(b"abc");
        let out = decode_multiplexed(&raw);
        assert!(out.contains("abc"), "数据不能丢：{out:?}");
    }

    #[test]
    fn empty_log_is_empty_string() {
        assert_eq!(decode_multiplexed(b""), "");
    }

    #[test]
    fn path_encoding_only_leaves_safe_characters() {
        assert_eq!(enc("web-1"), "web-1");
        assert_eq!(enc("my_container.1"), "my_container.1");
        assert_eq!(enc("../etc"), "..%2Fetc");
        assert_eq!(enc("a b"), "a%20b");
    }

    #[test]
    fn unsupported_actions_are_refused_before_touching_the_socket() {
        // 用一个不存在的 socket：如果能过校验，错误会是 Unavailable；
        // 被拒绝时必须是 Failed(400)，证明没有真的去连。
        let d = Docker::new("/nonexistent/docker.sock");
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(d.action("abc", "exec"))
            .unwrap_err();
        match err {
            DockerError::Failed { status, .. } => assert_eq!(status, 400),
            other => panic!("应当被允许清单挡下，实际 {other}"),
        }
    }

    #[test]
    fn allowed_actions_are_a_small_explicit_set() {
        // 这条测试是护栏：有人想加 destroy/exec 之类的动作时，必须改这里并解释。
        assert_eq!(
            ALLOWED_ACTIONS,
            &["start", "stop", "restart", "pause", "unpause"]
        );
    }

    #[test]
    fn stats_math_matches_docker_cli_conventions() {
        let raw = serde_json::json!({
            "cpu_stats": { "cpu_usage": { "total_usage": 2_000_000_000u64 }, "system_cpu_usage": 20_000_000_000u64, "online_cpus": 2 },
            "precpu_stats": { "cpu_usage": { "total_usage": 1_000_000_000u64 }, "system_cpu_usage": 10_000_000_000u64 },
            "memory_stats": {
                "usage": 200 * 1024 * 1024,
                "limit": 1024 * 1024 * 1024,
                "stats": { "inactive_file": 50 * 1024 * 1024 }
            },
            "networks": { "eth0": { "rx_bytes": 1000, "tx_bytes": 2000 } },
            "blkio_stats": { "io_service_bytes_recursive": [
                { "op": "read", "value": 4096 }, { "op": "Write", "value": 8192 }
            ]},
            "pids_stats": { "current": 7 }
        });
        let s = ContainerStats::from_api(&raw);
        // cpu_delta=1e9, sys_delta=1e10 → 10% × 2 核 = 20%
        assert!((s.cpu_pct - 20.0).abs() < 0.001, "实际 {}", s.cpu_pct);
        // 内存要减掉页缓存：200MB - 50MB = 150MB
        assert_eq!(s.mem_used_bytes, 150 * 1024 * 1024);
        assert!((s.mem_pct - (150.0 / 1024.0 * 100.0)).abs() < 0.001);
        assert_eq!(s.net_rx_bytes, 1000);
        assert_eq!(s.net_tx_bytes, 2000);
        assert_eq!(s.block_read_bytes, 4096);
        assert_eq!(s.block_write_bytes, 8192, "op 大小写不应影响统计");
        assert_eq!(s.pids, 7);
    }

    #[test]
    fn stats_without_previous_sample_report_zero_cpu() {
        let raw = serde_json::json!({
            "cpu_stats": { "cpu_usage": { "total_usage": 5 }, "system_cpu_usage": 10 },
            "memory_stats": { "usage": 1024, "limit": 2048 },
        });
        let s = ContainerStats::from_api(&raw);
        assert_eq!(s.cpu_pct, 0.0, "没有前值就算不出速率，报 0 而不是瞎编");
        assert!(s.mem_pct > 0.0);
    }

    #[test]
    fn container_is_parsed_with_compose_labels() {
        let raw = serde_json::json!({
            "Id": "abc123def456",
            "Names": ["/myproj-web-1"],
            "Image": "nginx:1.27",
            "ImageID": "sha256:0123456789abcdef",
            "State": "running",
            "Status": "Up 2 hours",
            "Created": 1_700_000_000i64,
            "Ports": [
                { "IP": "0.0.0.0", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp" },
                { "PrivatePort": 443, "Type": "tcp" }
            ],
            "Labels": {
                "com.docker.compose.project": "myproj",
                "com.docker.compose.service": "web"
            }
        });
        let c = Container::from_api(&raw);
        assert_eq!(c.name, "myproj-web-1", "前导 / 应被去掉");
        assert_eq!(c.image_id, "0123456789ab", "sha256: 前缀应去掉并截断");
        assert_eq!(c.compose_project.as_deref(), Some("myproj"));
        assert_eq!(c.compose_service.as_deref(), Some("web"));
        assert_eq!(c.ports.len(), 2);
        assert_eq!(c.ports[0].public, Some(8080));
        assert_eq!(c.ports[1].public, None, "未发布的端口没有 public");
    }

    #[test]
    fn unavailable_socket_reports_unavailable_not_protocol_error() {
        let d = Docker::new("/nonexistent/docker.sock");
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(d.probe())
            .unwrap_err();
        assert!(
            matches!(err, DockerError::Unavailable(_)),
            "连不上 socket 应是 Unavailable（面板据此显示「未安装」），实际 {err}"
        );
    }
}
