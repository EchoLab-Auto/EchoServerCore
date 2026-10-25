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

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// 普通请求（读状态、日志、资源）的超时。
///
/// 5 秒对这些操作足够；超时说明 daemon 卡住了，早点告诉使用者比挂着强。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// 启停类请求的超时。
///
/// **必须显著大于 `STOP_GRACE_SECS`**，否则会出现最难排查的一类错误：
/// `docker stop?t=10` 在 daemon 侧要等容器最多 10 秒才 SIGKILL，而客户端
/// 5 秒就断了 —— 于是面板报「超时失败」，而容器实际上正在被停掉。
/// 使用者看到失败会重试或改用 CLI，两个操作叠在一起，状态就乱了。
///
/// 这不是理论：第一次真实部署时 stop 就是这样报的 504。
const ACTION_TIMEOUT: Duration = Duration::from_secs(30);

/// `docker stop` / `restart` 的 `t=` 秒数：给容器多久优雅退出，之后 SIGKILL。
///
/// 10 秒是 docker CLI 的默认值。之所以不缩短：有些数据库把 SIGTERM 当
/// 「保存并退出」，给太少时间等于让它丢数据。
const STOP_GRACE_SECS: u32 = 10;

/// 拉镜像的超时。大镜像（GB 级）在慢链路上要很久，给足 10 分钟。
///
/// 拉取**不**同步阻塞在前端请求上：`POST /api/docker/images/pull` 立刻返回
/// 一个 job id，真正的等待发生在后台任务里，前端轮询进度。所以这个超时
/// 只是兜底，不会让人干等。
const PULL_TIMEOUT: Duration = Duration::from_secs(600);

/// 创建一个容器的超时。create 本身是毫秒级的，留足余量是因为调用方可能
/// 先拉镜像再创建，两步共用一份耐心。
const CREATE_TIMEOUT: Duration = Duration::from_secs(660);

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
    ///
    /// 注意：`/version` 的响应里**没有** `RegistryConfig`。要拿镜像加速器
    /// 列表得用 `info()`——`/info` 里既有版本也有 `RegistryConfig.Mirrors`。
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
            "stop" | "restart" => {
                format!("/containers/{}/{action}?t={STOP_GRACE_SECS}", enc(id))
            }
            _ => format!("/containers/{}/{action}", enc(id)),
        };
        let (status, body) = self
            .request_with_timeout("POST", &path, None, ACTION_TIMEOUT)
            .await?;
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

    // ── 详情 / 创建 / 删除 / 改名 / 重启策略 / 镜像 ──────────────────────

    /// 校验容器 id / 名字。
    ///
    /// 空 id 会拼出 `/containers//json` 这种路径，Docker 回 **301 重定向**，
    /// 而��们的客户端不跟随重定向，于是错误显示成 `Docker 返回 301`——
    /// 完全看不出真正的原因是「调用方给了空 id」。在这里挡掉它。
    fn check_id(id: &str) -> Result<(), DockerError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(DockerError::Failed {
                status: 400,
                message: "容器标识为空".into(),
            });
        }
        if id.len() > 128 {
            return Err(DockerError::Failed {
                status: 400,
                message: "容器标识过长".into(),
            });
        }
        Ok(())
    }

    /// 容器详情：把 Docker 那份巨大的 inspect JSON 收敛成面板要用的形状。
    ///
    /// 直接把原始 JSON 丢给前端也能用，但那个结构有上千字段、层级极深，
    /// 前端要为「重启策略叫什么」「挂载源在哪」写一堆防御性取值。在这里
    /// 收敛成固定形状，前端就只是渲染——也顺带成了 API 契约的一部分。
    pub async fn detail(&self, id: &str) -> Result<ContainerDetail, DockerError> {
        let raw = self.container(id).await?;
        Ok(ContainerDetail::from_inspect(&raw))
    }

    /// 镜像是否存在（创建容器前用来决定要不要先拉）。
    pub async fn image_exists(&self, image: &str) -> bool {
        let (name, tag) = split_image(image);
        let path = format!("/images/{}/json", enc(&format!("{name}:{tag}")));
        self.get_json(&path).await.is_ok()
    }

    /// 拉镜像。返回一份人可读的总结（成功时有 digest，失败时有原因）。
    ///
    /// Docker 的 `/images/create` 返回的是 **NDJSON 流**：一堆进度行，最后
    /// 要么是 `Digest: sha256:...`，要么是一个 `errorDetail`。所以必须读完
    /// 整个 body 才能判断成败——中途的 200 状态码不代表拉成功。
    pub async fn pull_image(&self, image: &str) -> Result<String, DockerError> {
        let (name, tag) = split_image(image);
        let path = format!("/images/create?fromImage={}&tag={}", enc(&name), enc(&tag));
        let (status, body) = self
            .request_with_timeout("POST", &path, None, PULL_TIMEOUT)
            .await?;
        if !(200..300).contains(&status) {
            return Err(DockerError::Failed {
                status,
                message: error_message(&body),
            });
        }
        // 逐行看 NDJSON：任何一行带 errorDetail 就是失败
        let text = String::from_utf8_lossy(&body);
        let mut digest = String::new();
        let mut last_status = String::new();
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(err) = v
                .get("errorDetail")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .or_else(|| v.get("error").and_then(Value::as_str))
            {
                return Err(DockerError::Failed {
                    status,
                    message: err.to_string(),
                });
            }
            if let Some(msg) = v.get("status").and_then(Value::as_str) {
                last_status = msg.to_string();
                if msg.starts_with("Digest:") {
                    digest = msg.trim_start_matches("Digest:").trim().to_string();
                }
            }
        }
        if last_status.is_empty() {
            return Err(DockerError::Protocol("拉取响应里没有可识别的状态行".into()));
        }
        Ok(if digest.is_empty() {
            format!("{image} 已拉取（{last_status}）")
        } else {
            format!("{image} 已拉取（{last_status}，digest {digest}）")
        })
    }

    /// 删除镜像。`force` 会连「被容器引用」的镜像也删（容器会变成悬空）。
    pub async fn remove_image(&self, image: &str, force: bool) -> Result<(), DockerError> {
        let path = format!(
            "/images/{}?force={}&noprune=0",
            enc(image),
            if force { 1 } else { 0 }
        );
        let (status, body) = self.request("DELETE", &path, None).await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(DockerError::Failed {
                status,
                message: error_message(&body),
            })
        }
    }

    /// 删除容器。`force` 会先 SIGKILL 再删（不 force 时运行中的容器会 409）。
    pub async fn remove_container(&self, id: &str, force: bool) -> Result<(), DockerError> {
        Self::check_id(id)?;
        let path = format!(
            "/containers/{}?force={}&v=1",
            enc(id),
            if force { 1 } else { 0 }
        );
        let (status, body) = self.request("DELETE", &path, None).await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(DockerError::Failed {
                status,
                message: error_message(&body),
            })
        }
    }

    /// 改名。Docker 要求目标名在全局唯一，撞名会 409。
    pub async fn rename(&self, id: &str, name: &str) -> Result<(), DockerError> {
        Self::check_id(id)?;
        let path = format!("/containers/{}/rename?name={}", enc(id), enc(name));
        let (status, body) = self.request("POST", &path, None).await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(DockerError::Failed {
                status,
                message: error_message(&body),
            })
        }
    }

    /// 改重启策略。运维里最常见的需求：把 `no` 改成 `unless-stopped`，
    /// 免得机器重启后容器不再自启——而这通常只有在出过事之后才会被想起来。
    pub async fn set_restart_policy(&self, id: &str, policy: &str) -> Result<(), DockerError> {
        Self::check_id(id)?;
        let parsed = RestartPolicy::parse(policy).map_err(|m| DockerError::Failed {
            status: 400,
            message: m,
        })?;
        let body = json!({ "RestartPolicy": parsed.to_api() }).to_string();
        let path = format!("/containers/{}/update", enc(id));
        let (status, resp) = self
            .request("POST", &path, Some(body.as_bytes()))
            .await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(DockerError::Failed {
                status,
                message: error_message(&resp),
            })
        }
    }

    /// 创建容器（可选自动启动）。
    ///
    /// **不 pull 镜像**——是否先拉由调用方决定（`create` 的编排在 api 层），
    /// 这样「镜像不存在」能被明确报成 404 而不是被一次隐蔽的拉取掩盖。
    pub async fn create_container(&self, spec: &CreateSpec) -> Result<String, DockerError> {
        let body: Value = spec.to_create_body()?;
        let mut path = String::from("/containers/create");
        if let Some(name) = spec.name.as_deref().filter(|n| !n.trim().is_empty()) {
            path.push_str(&format!("?name={}", enc(name.trim())));
        }
        let text = body.to_string();
        let (status, resp) = self
            .request_with_timeout("POST", &path, Some(text.as_bytes()), CREATE_TIMEOUT)
            .await?;
        if !(200..300).contains(&status) {
            return Err(DockerError::Failed {
                status,
                message: error_message(&resp),
            });
        }
        let v: Value = serde_json::from_slice(&resp)
            .map_err(|e| DockerError::Protocol(format!("create 响应不是 JSON: {e}")))?;
        let id = v
            .get("Id")
            .and_then(Value::as_str)
            .ok_or_else(|| DockerError::Protocol("create 响应里没有 Id".into()))?
            .to_string();
        if spec.auto_start {
            // 创建成功但启动失败时，容器已经存在了——把 id 带进错误里，
            // 否则使用者看到一个「失败」却不知道留下了什么。
            self.action(&id, "start").await.map_err(|e| DockerError::Failed {
                status: 500,
                message: format!("容器已创建（id {id}）但启动失败：{e}"),
            })?;
        }
        Ok(id)
    }

    /// 带 `since` 的日志：只取某个时间点之后的新行。
    ///
    /// 前端日志面板用它做增量刷新——比每次重拉 tail 省带宽，更重要的是
    /// 不会让已滚上去的内容闪一下再回来。
    pub async fn logs_since(
        &self,
        id: &str,
        tail: u32,
        since: Option<&str>,
    ) -> Result<String, DockerError> {
        let mut path = format!(
            "/containers/{}/logs?stdout=1&stderr=1&timestamps=1&tail={}",
            enc(id),
            tail.clamp(1, 5000)
        );
        if let Some(s) = since.filter(|s| !s.is_empty()) {
            // since 必须是 unix 秒。
            //
            // Docker 文档说也接受 RFC3339，但实测（29.8.x）非法格式会被**静默
            // 忽略**，于是「只要新日志」变成「返回全部日志」。前端的增量刷新
            // 就会越滚越长、内容重复。所以这里宁可 400 也不要静默降级。
            let secs: f64 = s.trim().parse().map_err(|_| DockerError::Failed {
                status: 400,
                message: format!("since 需要 unix 秒（数字），收到 {s:?}"),
            })?;
            if !secs.is_finite() || secs < 0.0 {
                return Err(DockerError::Failed {
                    status: 400,
                    message: format!("since 必须是非负有限数，收到 {s:?}"),
                });
            }
            path.push_str(&format!("&since={secs:.3}"));
        }
        let (status, body) = self.request("GET", &path, None).await?;
        if !(200..300).contains(&status) {
            return Err(DockerError::Failed {
                status,
                message: error_message(&body),
            });
        }
        Ok(decode_multiplexed(&body))
    }

    /// 网络列表（创建容器时选）。
    pub async fn networks(&self) -> Result<Vec<String>, DockerError> {
        let raw: Value = self.get_json("/networks").await?;
        Ok(raw
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|n| n.get("Name").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
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
        self.request_with_timeout(method, path, body, REQUEST_TIMEOUT)
            .await
    }

    async fn request_with_timeout(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        limit: Duration,
    ) -> Result<(u16, Vec<u8>), DockerError> {
        let fut = self.request_inner(method, path, body);
        match tokio::time::timeout(limit, fut).await {
            Ok(r) => r,
            Err(_) => Err(DockerError::Failed {
                status: 504,
                message: format!(
                    "请求 {method} {path} 超过 {}s 未返回（daemon 可能卡住了；\
                     注意操作可能仍在服务端进行中）",
                    limit.as_secs()
                ),
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

// ─────────────────────────────────────────────────────────────────────────────
// 创建容器
// ─────────────────────────────────────────────────────────────────────────────

/// 创建一个容器的参数（HTTP 层直接反序列化这个）。
///
/// 刻意只覆盖「日常 90%」的字段。Docker 的 create API 有上百个参数，把
/// `Privileged` / `PidMode` / `CapAdd` 之类暴露到 Web 表单上，除了让界面
/// 变得不可用，还会把「一条命令就能拿到宿主机 root」包装成一个下拉框。
/// 需要那些能力时用 `docker run`——它至少有 shell history 留痕。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateSpec {
    /// `nginx:1.27` / `alpine`（无 tag 视为 latest）。
    pub image: String,
    #[serde(default)]
    pub name: Option<String>,
    /// 覆盖镜像的 CMD。空 = 用镜像默认。前端按空格拆或者一行一个都行。
    #[serde(default)]
    pub cmd: Vec<String>,
    /// `KEY=VALUE` 列表。
    #[serde(default)]
    pub env: Vec<String>,
    /// `8080:80` / `127.0.0.1:8080:80` / `8080:80/udp`
    #[serde(default)]
    pub ports: Vec<String>,
    /// `/host:/container[:ro]`
    #[serde(default)]
    pub binds: Vec<String>,
    /// `no` | `always` | `unless-stopped` | `on-failure[:N]`
    #[serde(default = "default_restart")]
    pub restart: String,
    #[serde(default)]
    pub network: Option<String>,
    /// 创建后立即启动。
    #[serde(default = "default_true")]
    pub auto_start: bool,
}

fn default_restart() -> String {
    "unless-stopped".to_string()
}

fn default_true() -> bool {
    true
}

impl CreateSpec {
    pub fn validate(&self) -> Result<(), String> {
        let image = self.image.trim();
        if image.is_empty() {
            return Err("image 不能为空".into());
        }
        // 镜像名只允许 [a-z0-9] 与 . / : - _ @，别的都是拼错了或者想注入
        if !image
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | ':' | '-' | '_' | '@'))
        {
            return Err(format!("image {image:?} 含非法字符"));
        }
        if let Some(name) = self.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            {
                return Err(format!("容器名 {name:?} 只允许字母数字与 . _ -"));
            }
        }
        for p in &self.ports {
            PortSpec::parse(p)?;
        }
        for e in &self.env {
            if !e.contains('=') {
                return Err(format!("环境变量 {e:?} 需要 KEY=VALUE 形式"));
            }
        }
        RestartPolicy::parse(&self.restart)?;
        Ok(())
    }

    fn to_create_body(&self) -> Result<Value, DockerError> {
        self.validate().map_err(|m| DockerError::Failed {
            status: 400,
            message: m,
        })?;
        let ports: Vec<PortSpec> = self
            .ports
            .iter()
            .filter_map(|p| PortSpec::parse(p).ok())
            .collect();

        // Docker 要求映射的端口同时出现在 ExposedPorts 里；只给 PortBindings
        // 在某些版本上不生效（表现为端口映射"配置了但连不上"）。所以这里
        // 从 PortBindings 反推 ExposedPorts，不让使用者填两遍。
        let mut exposed = serde_json::Map::new();
        let mut bindings = serde_json::Map::new();
        for p in &ports {
            let key = format!("{}/{}", p.container, p.proto);
            exposed.insert(key.clone(), json!({}));
            let arr = bindings.entry(key).or_insert_with(|| json!([]));
            if let Some(arr) = arr.as_array_mut() {
                arr.push(json!({
                    "HostIp": p.host_ip,
                    "HostPort": p.host_port.map(|h| h.to_string()).unwrap_or_default(),
                }));
            }
        }

        let mut host_config = json!({
            "PortBindings": bindings,
            "Binds": self.binds,
            "RestartPolicy": RestartPolicy::parse(&self.restart)
                .map_err(|m| DockerError::Failed { status: 400, message: m })?
                .to_api(),
        });
        if let Some(net) = self.network.as_deref().filter(|n| !n.trim().is_empty()) {
            host_config["NetworkMode"] = json!(net.trim());
        }

        let mut body = json!({
            "Image": self.image.trim(),
            "Env": self.env,
            "ExposedPorts": exposed,
            "HostConfig": host_config,
            "Labels": { "esc.server-panel": "1" },
        });
        if !self.cmd.is_empty() {
            body["Cmd"] = json!(self.cmd);
        }
        Ok(body)
    }
}

/// 一条端口映射。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSpec {
    pub host_ip: String,
    pub host_port: Option<u16>,
    pub container: u16,
    pub proto: String,
}

impl PortSpec {
    /// 解析 `[ip:]host:container[/proto]`。
    ///
    /// 也接受 `container` 单段（只 expose 不映射到固定端口，让 docker 随机分配）。
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("端口映射不能为空".into());
        }
        let (rest, proto) = match spec.rsplit_once('/') {
            Some((r, p)) => {
                let p = p.to_ascii_lowercase();
                if p != "tcp" && p != "udp" && p != "sctp" {
                    return Err(format!("端口协议 {p:?} 不支持（tcp/udp/sctp）"));
                }
                (r, p)
            }
            None => (spec, "tcp".to_string()),
        };
        let parts: Vec<&str> = rest.split(':').collect();
        let (host_ip, host_port, container) = match parts.as_slice() {
            [c] => ("0.0.0.0".to_string(), None, *c),
            [h, c] => (
                "0.0.0.0".to_string(),
                Some(parse_port_num(h, spec)?),
                *c,
            ),
            [ip, h, c] => (ip.to_string(), Some(parse_port_num(h, spec)?), *c),
            _ => return Err(format!("端口映射 {spec:?} 格式不对（host:container）")),
        };
        let container = parse_port_num(container, spec)?;
        Ok(Self {
            host_ip,
            host_port,
            container,
            proto,
        })
    }
}

fn parse_port_num(s: &str, whole: &str) -> Result<u16, String> {
    s.trim()
        .parse::<u16>()
        .map_err(|_| format!("端口映射 {whole:?} 里的 {s:?} 不是合法端口号"))
}

/// 重启策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartPolicy {
    No,
    Always,
    UnlessStopped,
    OnFailure(Option<u32>),
}

impl RestartPolicy {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        match s {
            "no" | "" => Ok(Self::No),
            "always" => Ok(Self::Always),
            "unless-stopped" => Ok(Self::UnlessStopped),
            other => {
                if let Some(rest) = other.strip_prefix("on-failure") {
                    // `on-failure` 后面没东西 = 不限次数；写了就必须是数字。
                    // 之前这里用 `.ok()` 吞掉了解析错误，于是 `on-failure:x`
                    // 被当成 `on-failure` 接受——使用者以为设了上限，其实没有。
                    let digits = rest.trim_start_matches(':').trim();
                    if digits.is_empty() {
                        return Ok(Self::OnFailure(None));
                    }
                    let retries = digits.parse::<u32>().map_err(|_| {
                        format!("重启策略 {other:?} 的重试次数 {digits:?} 不是数字")
                    })?;
                    Ok(Self::OnFailure(Some(retries)))
                } else {
                    Err(format!(
                        "重启策略 {other:?} 不支持（no / always / unless-stopped / on-failure[:N]）"
                    ))
                }
            }
        }
    }

    pub fn to_api(&self) -> Value {
        match self {
            Self::No => json!({ "Name": "no" }),
            Self::Always => json!({ "Name": "always" }),
            Self::UnlessStopped => json!({ "Name": "unless-stopped" }),
            Self::OnFailure(n) => json!({
                "Name": "on-failure",
                "MaximumRetryCount": n.unwrap_or(0),
            }),
        }
    }
}

/// `nginx` → ("nginx", "latest")；`nginx:1.27` → ("nginx", "1.27")。
///
/// 注意 registry 里带端口的情况（`localhost:5000/nginx:1.27`）：冒号出现在
/// 最后一个 `/` 之前时属于主机名，不是 tag 分隔符。这个细节弄错会把
/// `localhost:5000/nginx` 拆成 `localhost` + `5000/nginx`，然后拉一个
/// 不存在的镜像。
pub fn split_image(image: &str) -> (String, String) {
    let image = image.trim();
    let last_slash = image.rfind('/');
    let last_colon = image.rfind(':');
    match (last_colon, last_slash) {
        (Some(c), Some(sl)) if c < sl => (image.to_string(), "latest".to_string()),
        (Some(c), _) => (
            image[..c].to_string(),
            image[c + 1..].to_string(),
        ),
        (None, _) => (image.to_string(), "latest".to_string()),
    }
}

// ────────────────────────────────────────────────────────────────────────────��
// 容器详情
// ─────────────────────────────────────────────────────────────────────────────

/// 面板要展示的容器详情（从 inspect 的原始 JSON 收敛而来）。
#[derive(Debug, Clone, Serialize)]
pub struct ContainerDetail {
    pub id: String,
    pub name: String,
    pub image: String,
    pub created: String,
    pub state: String,
    pub status: String,
    pub started_at: String,
    pub finished_at: String,
    pub exit_code: i64,
    pub restart_policy: String,
    pub restart_count: i64,
    pub platform: String,
    pub cmd: Vec<String>,
    pub entrypoint: Vec<String>,
    pub working_dir: String,
    pub env: Vec<(String, String)>,
    pub ports: Vec<PortMapping>,
    pub mounts: Vec<MountInfo>,
    pub networks: Vec<(String, String)>,
    pub memory_limit_bytes: i64,
    pub cpu_quota: i64,
    pub privileged: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MountInfo {
    pub kind: String,
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

impl ContainerDetail {
    fn from_inspect(v: &Value) -> Self {
        let cfg = v.get("Config").cloned().unwrap_or(Value::Null);
        let st = v.get("State").cloned().unwrap_or(Value::Null);
        let host = v.get("HostConfig").cloned().unwrap_or(Value::Null);
        let str_list = |node: &Value, key: &str| -> Vec<String> {
            node.get(key)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        let env = str_list(&cfg, "Env")
            .into_iter()
            .map(|kv| match kv.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (kv, String::new()),
            })
            .collect();

        let mut ports: Vec<PortMapping> = cfg
            .get("ExposedPorts")
            .and_then(Value::as_object)
            .map(|o| {
                o.keys()
                    .filter_map(|k| {
                        let (port, proto) = k.split_once('/')?;
                        Some(PortMapping {
                            ip: String::new(),
                            private: port.parse().ok()?,
                            public: None,
                            kind: proto.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        // HostConfig.PortBindings 里有真实的 host 端口，用来补全上面的 public
        if let Some(bindings) = host.get("PortBindings").and_then(Value::as_object) {
            for (key, val) in bindings {
                let Some((port, proto)) = key.split_once('/') else {
                    continue;
                };
                let Ok(private) = port.parse::<u16>() else {
                    continue;
                };
                let first = val.as_array().and_then(|a| a.first());
                let public = first
                    .and_then(|b| b.get("HostPort"))
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<u16>().ok());
                if let Some(existing) = ports
                    .iter_mut()
                    .find(|p| p.private == private && p.kind == proto)
                {
                    existing.public = public;
                } else {
                    ports.push(PortMapping {
                        ip: first
                            .and_then(|b| b.get("HostIp"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        private,
                        public,
                        kind: proto.to_string(),
                    });
                }
            }
        }

        let mounts = v
            .get("Mounts")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|m| MountInfo {
                        kind: m
                            .get("Type")
                            .and_then(Value::as_str)
                            .unwrap_or("bind")
                            .to_string(),
                        source: m
                            .get("Source")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        target: m
                            .get("Destination")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        read_only: m.get("RW").and_then(Value::as_bool).map(|rw| !rw).unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let networks = v
            .get("NetworkSettings")
            .and_then(|n| n.get("Networks"))
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .map(|(name, v)| {
                        (
                            name.clone(),
                            v.get("IPAddress")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        let restart_policy = host
            .get("RestartPolicy")
            .and_then(|r| r.get("Name"))
            .and_then(Value::as_str)
            .unwrap_or("no")
            .to_string();
        let retries = host
            .get("RestartPolicy")
            .and_then(|r| r.get("MaximumRetryCount"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let restart_policy = if restart_policy == "on-failure" && retries > 0 {
            format!("on-failure:{retries}")
        } else {
            restart_policy
        };

        let name = v
            .get("Name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_start_matches('/')
            .to_string();

        Self {
            id: v.get("Id").and_then(Value::as_str).unwrap_or("").to_string(),
            name,
            image: cfg.get("Image").and_then(Value::as_str).unwrap_or("").to_string(),
            created: v.get("Created").and_then(Value::as_str).unwrap_or("").to_string(),
            state: st
                .get("Status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            status: st
                .get("Error")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|e| format!("error: {e}"))
                .unwrap_or_else(|| {
                    if st.get("Running").and_then(Value::as_bool).unwrap_or(false) {
                        "running".to_string()
                    } else {
                        "stopped".to_string()
                    }
                }),
            started_at: st
                .get("StartedAt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            finished_at: st
                .get("FinishedAt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            exit_code: st.get("ExitCode").and_then(Value::as_i64).unwrap_or(0),
            restart_policy,
            restart_count: v.get("RestartCount").and_then(Value::as_i64).unwrap_or(0),
            platform: v
                .get("Platform")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            cmd: str_list(&cfg, "Cmd"),
            entrypoint: str_list(&cfg, "Entrypoint"),
            working_dir: cfg
                .get("WorkingDir")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            env,
            ports,
            mounts,
            networks,
            memory_limit_bytes: host
                .get("Memory")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            cpu_quota: host.get("CpuQuota").and_then(Value::as_i64).unwrap_or(0),
            privileged: host
                .get("Privileged")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 后台任务（拉镜像 / 建容器）
// ─────────────────────────────────────────────────────────────────────────────

/// 一个后台任务的状态。拉镜像可能要几分钟，不能同步等着。
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: String,
    /// 单调递增序号。`started_ms` 精度只到毫秒，同一毫秒内创建的多个任务
    /// 光靠时间排序会得到随机顺序——列表一会儿一个样，看起来像在跳动。
    pub seq: u64,
    pub kind: String,
    /// `running` | `done` | `failed`
    pub state: String,
    pub message: String,
    pub started_ms: i64,
    pub elapsed_ms: i64,
}

#[derive(Debug, Default)]
pub struct Jobs {
    inner: std::sync::Mutex<std::collections::HashMap<String, Job>>,
    counter: std::sync::atomic::AtomicU64,
}

/// 已完成任务保留多久。面板是单人用的，留 10 分钟足够回看，也不会积累。
const JOB_TTL_MS: i64 = 10 * 60 * 1000;

impl Jobs {
    pub fn new() -> Self {
        Self::default()
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// 登记一个新任务，返回 id。
    pub fn start(&self, kind: &str) -> String {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("j{}-{}", Self::now_ms(), n);
        let job = Job {
            id: id.clone(),
            seq: n,
            kind: kind.to_string(),
            state: "running".to_string(),
            message: String::new(),
            started_ms: Self::now_ms(),
            elapsed_ms: 0,
        };
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(id.clone(), job);
        id
    }

    /// 收尾。`result` 为 `Ok(msg)` 或 `Err(msg)`。
    pub fn finish(&self, id: &str, result: Result<String, String>) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = map.get_mut(id) {
            let (state, message) = match result {
                Ok(m) => ("done".to_string(), m),
                Err(m) => ("failed".to_string(), m),
            };
            job.state = state;
            job.message = message;
            job.elapsed_ms = Self::now_ms() - job.started_ms;
        }
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.get(id).map(|j| {
            let mut j = j.clone();
            if j.state == "running" {
                j.elapsed_ms = Self::now_ms() - j.started_ms;
            }
            j
        })
    }

    pub fn list(&self) -> Vec<Job> {
        self.gc();
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Self::now_ms();
        let mut out: Vec<Job> = map
            .values()
            .map(|j| {
                let mut j = j.clone();
                if j.state == "running" {
                    j.elapsed_ms = now - j.started_ms;
                }
                j
            })
            .collect();
        out.sort_by_key(|j| std::cmp::Reverse((j.started_ms, j.seq)));
        out
    }

    /// 丢掉过期的已完成任务。只在 list/get 时顺带做，不需要定时器。
    pub fn gc(&self) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Self::now_ms();
        map.retain(|_, j| j.state == "running" || now - j.started_ms < JOB_TTL_MS);
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
    fn action_timeout_exceeds_the_stop_grace_period() {
        // 这条是护栏。真实事故：REQUEST_TIMEOUT=5s 而 stop 带 t=10，
        // 于是 daemon 还在优雅等容器退出，我们 5 秒就报"超时失败"——
        // 而容器实际上正在被停掉。使用者看到失败会重试，两个操作叠在一起。
        assert!(
            ACTION_TIMEOUT > Duration::from_secs(STOP_GRACE_SECS as u64),
            "启停超时（{}s）必须大于 stop 的优雅期（{}s），否则必然误报超时",
            ACTION_TIMEOUT.as_secs(),
            STOP_GRACE_SECS
        );
        // 留出可观的余量而不只是"大 1 秒"：daemon 在负载下还需要额外时间
        assert!(
            ACTION_TIMEOUT >= Duration::from_secs(STOP_GRACE_SECS as u64 + 10),
            "余量太小，daemon 稍慢就会误报"
        );
    }

    // ── 端口 / 镜像名 / 重启策略 ─────────────────────────────────────────

    #[test]
    fn port_spec_accepts_the_forms_people_actually_type() {
        let a = PortSpec::parse("8080:80").unwrap();
        assert_eq!((a.host_ip.as_str(), a.host_port, a.container, a.proto.as_str()),
                   ("0.0.0.0", Some(8080), 80, "tcp"));

        let b = PortSpec::parse("127.0.0.1:8443:443").unwrap();
        assert_eq!((b.host_ip.as_str(), b.host_port, b.container),
                   ("127.0.0.1", Some(8443), 443));

        let c = PortSpec::parse("53:53/udp").unwrap();
        assert_eq!((c.host_port, c.container, c.proto.as_str()), (Some(53), 53, "udp"));

        // 单段 = 只 expose，让 docker 随机分配 host 端口
        let d = PortSpec::parse("3000").unwrap();
        assert_eq!((d.host_port, d.container), (None, 3000));
    }

    #[test]
    fn port_spec_rejects_garbage_instead_of_guessing() {
        assert!(PortSpec::parse("").is_err());
        assert!(PortSpec::parse("abc:80").is_err());
        assert!(PortSpec::parse("99999:80").is_err(), "端口超过 u16");
        assert!(PortSpec::parse("1:2:3:4").is_err());
        assert!(PortSpec::parse("8080:80/sctp2").is_err());
    }

    #[test]
    fn split_image_does_not_mistake_a_registry_port_for_a_tag() {
        // 这是唯一容易写错的地方：冒号在最后一个 / 之前时属于主机名。
        assert_eq!(split_image("nginx"), ("nginx".into(), "latest".into()));
        assert_eq!(split_image("nginx:1.27"), ("nginx".into(), "1.27".into()));
        assert_eq!(
            split_image("library/nginx:alpine"),
            ("library/nginx".into(), "alpine".into())
        );
        assert_eq!(
            split_image("localhost:5000/nginx"),
            ("localhost:5000/nginx".into(), "latest".into()),
            "registry 端口不是 tag 分隔符"
        );
        assert_eq!(
            split_image("localhost:5000/nginx:1.27"),
            ("localhost:5000/nginx".into(), "1.27".into())
        );
    }

    #[test]
    fn restart_policy_round_trips_and_rejects_typos() {
        assert_eq!(RestartPolicy::parse("no").unwrap(), RestartPolicy::No);
        assert_eq!(RestartPolicy::parse("").unwrap(), RestartPolicy::No);
        assert_eq!(
            RestartPolicy::parse("unless-stopped").unwrap(),
            RestartPolicy::UnlessStopped
        );
        assert_eq!(
            RestartPolicy::parse("on-failure:5").unwrap(),
            RestartPolicy::OnFailure(Some(5))
        );
        assert_eq!(
            RestartPolicy::parse("on-failure").unwrap(),
            RestartPolicy::OnFailure(None)
        );
        for bad in ["alwayss", "unless", "restart", "on-failure:x"] {
            assert!(RestartPolicy::parse(bad).is_err(), "{bad} 应被拒绝");
        }
        // on-failure 的 N 要真的写进 API 请求，否则等于没设上限
        let api = RestartPolicy::parse("on-failure:3").unwrap().to_api();
        assert_eq!(api["Name"], "on-failure");
        assert_eq!(api["MaximumRetryCount"], 3);
    }

    #[test]
    fn create_body_derives_exposed_ports_from_bindings() {
        // 只给 PortBindings 不给 ExposedPorts 时，某些 docker 版本会"配置了
        // 但连不上"。所以必须同时填，且两者要一致。
        let spec = CreateSpec {
            image: "nginx:alpine".into(),
            name: Some("web".into()),
            ports: vec!["8080:80".into(), "8443:443".into()],
            env: vec!["TZ=Asia/Shanghai".into()],
            binds: vec!["/srv/web:/usr/share/nginx/html:ro".into()],
            restart: "unless-stopped".into(),
            ..Default::default()
        };
        let body = spec.to_create_body().unwrap();
        assert_eq!(body["Image"], "nginx:alpine");
        assert_eq!(body["Env"][0], "TZ=Asia/Shanghai");
        assert_eq!(body["ExposedPorts"]["80/tcp"], json!({}));
        assert_eq!(body["ExposedPorts"]["443/tcp"], json!({}));
        assert_eq!(
            body["HostConfig"]["PortBindings"]["80/tcp"][0]["HostPort"],
            "8080"
        );
        assert_eq!(body["HostConfig"]["RestartPolicy"]["Name"], "unless-stopped");
        assert_eq!(body["HostConfig"]["Binds"][0], "/srv/web:/usr/share/nginx/html:ro");
        // 面板创建的容器打个标签，方便以后分辨哪些是从 Web 建出来的。
        // 用 `esc.server-panel` 而不是笼统的 `esc.panel`：项目里有两个面板，
        // 只有这个能创建容器，标签名应当能自证来源。
        assert_eq!(body["Labels"]["esc.server-panel"], "1");
    }

    #[test]
    fn create_spec_validation_catches_the_common_mistakes() {
        let ok = CreateSpec {
            image: "alpine".into(),
            ..Default::default()
        };
        assert!(ok.validate().is_ok());

        // 空镜像
        let mut s = ok.clone();
        s.image = "  ".into();
        assert!(s.validate().is_err());

        // 镜像名里塞 shell 元字符（虽然我们不走 shell，但也不该放进来）
        let mut s = ok.clone();
        s.image = "alpine;rm -rf /".into();
        assert!(s.validate().is_err());

        // 容器名带斜杠（docker 也不允许）
        let mut s = ok.clone();
        s.name = Some("a/b".into());
        assert!(s.validate().is_err());

        // 环境变量缺等号：使用者以为设了，其实是空的
        let mut s = ok.clone();
        s.env = vec!["NODE_ENV".into()];
        assert!(s.validate().is_err());

        // 端口写错
        let mut s = ok.clone();
        s.ports = vec!["8080".into(), "x:y".into()];
        assert!(s.validate().is_err());

        // 重启策略拼错
        let mut s = ok.clone();
        s.restart = "alwayss".into();
        assert!(s.validate().is_err());
    }

    #[test]
    fn detail_flattens_inspect_into_a_stable_shape() {
        let raw = json!({
            "Id": "abc123",
            "Name": "/my-web",
            "Created": "2026-10-25T12:00:00Z",
            "Platform": "linux",
            "RestartCount": 2,
            "State": { "Status": "running", "Running": true, "ExitCode": 0,
                       "StartedAt": "2026-10-25T12:00:01Z", "FinishedAt": "0001-01-01T00:00:00Z" },
            "Config": {
                "Image": "nginx:alpine",
                "Cmd": ["nginx", "-g", "daemon off;"],
                "Entrypoint": ["/docker-entrypoint.sh"],
                "WorkingDir": "/",
                "Env": ["PATH=/usr/bin", "MULTI=a=b"],
                "ExposedPorts": { "80/tcp": {} }
            },
            "HostConfig": {
                "RestartPolicy": { "Name": "on-failure", "MaximumRetryCount": 3 },
                "PortBindings": { "80/tcp": [{ "HostIp": "0.0.0.0", "HostPort": "8080" }] },
                "Memory": 268435456,
                "Privileged": false
            },
            "Mounts": [
                { "Type": "bind", "Source": "/srv/web", "Destination": "/usr/share/nginx/html", "RW": false },
                { "Type": "volume", "Source": "/var/lib/docker/volumes/x/_data", "Destination": "/data", "RW": true }
            ],
            "NetworkSettings": { "Networks": { "bridge": { "IPAddress": "172.17.0.2" } } }
        });
        let d = ContainerDetail::from_inspect(&raw);
        assert_eq!(d.name, "my-web", "���导 / 应被去掉");
        assert_eq!(d.state, "running");
        assert_eq!(d.restart_policy, "on-failure:3", "重试次数应拼进策略名");
        assert_eq!(d.restart_count, 2);
        assert_eq!(d.cmd, vec!["nginx", "-g", "daemon off;"]);
        // 环境变量按第一个 = 切分，值里可以再含 =
        assert_eq!(
            d.env.iter().find(|(k, _)| k == "MULTI").map(|(_, v)| v.as_str()),
            Some("a=b")
        );
        // 端口：ExposedPorts 提供 private，PortBindings 补上 public
        assert_eq!(d.ports.len(), 1);
        assert_eq!(d.ports[0].private, 80);
        assert_eq!(d.ports[0].public, Some(8080));
        // 挂载：RW:false → read_only:true
        assert_eq!(d.mounts.len(), 2);
        assert!(d.mounts[0].read_only);
        assert!(!d.mounts[1].read_only);
        assert_eq!(d.networks[0].0, "bridge");
        assert_eq!(d.networks[0].1, "172.17.0.2");
        assert_eq!(d.memory_limit_bytes, 268435456);
        assert!(!d.privileged);
    }

    #[test]
    fn detail_survives_a_minimal_inspect_payload() {
        // docker 的字段在不同版本间会少，缺字段不能让整个详情 500
        let d = ContainerDetail::from_inspect(&json!({}));
        assert_eq!(d.id, "");
        assert_eq!(d.state, "unknown");
        assert!(d.env.is_empty());
        assert!(d.mounts.is_empty());
        assert_eq!(d.restart_policy, "no");
    }

    // ── 任务表 ───────────────────────────────────────────────────────────

    #[test]
    fn jobs_track_running_then_done() {
        let jobs = Jobs::new();
        let id = jobs.start("pull");
        let j = jobs.get(&id).unwrap();
        assert_eq!(j.state, "running");
        assert_eq!(j.kind, "pull");

        jobs.finish(&id, Ok("nginx 已拉取".into()));
        let j = jobs.get(&id).unwrap();
        assert_eq!(j.state, "done");
        assert!(j.message.contains("nginx"));

        let id2 = jobs.start("pull");
        jobs.finish(&id2, Err("not found".into()));
        assert_eq!(jobs.get(&id2).unwrap().state, "failed");
        // 最新的排前面
        assert_eq!(jobs.list()[0].id, id2);
    }

    #[test]
    fn job_ids_are_unique_even_within_the_same_millisecond() {
        let jobs = Jobs::new();
        let ids: Vec<String> = (0..50).map(|_| jobs.start("x")).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "同一毫秒内也要唯一");
    }

    #[test]
    fn finished_jobs_are_garbage_collected() {
        let jobs = Jobs::new();
        let id = jobs.start("pull");
        jobs.finish(&id, Ok("ok".into()));
        // 手动把它挪到 TTL 之外
        {
            let mut map = jobs.inner.lock().unwrap();
            map.get_mut(&id).unwrap().started_ms -= JOB_TTL_MS + 1000;
        }
        jobs.gc();
        assert!(jobs.get(&id).is_none(), "过期任务应被清掉");
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
