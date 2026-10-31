//! 控制面的数据模型。
//!
//! 这里只描述「面板需要看到/编辑什么」，不包含任何隧道实现细节：
//! 没有 yamux、没有 rustls、没有 `HubConfig`。图形界面与穿透内核因此可以
//! 各自独立演化——只要这份契约不变，两边可以分别升级。
//!
//! 约定：所有结构体字段都带 `#[serde(default)]`（或显式默认值），这样
//! 面板、宿主、引擎三者即使版本不同也能互相解码，不会因为多一个字段而
//! 整条链路 fail。

use serde::{Deserialize, Serialize};

/// 引擎承担的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// 公网侧：接受 spoke 连接并为映射开公网端口。
    Hub,
    /// 内网侧：主动连出，把流量转给本地服务。
    Spoke,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Hub => "hub",
            Role::Spoke => "spoke",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 映射
// ─────────────────────────────────────────────────────────────────────────────

/// 一条端口映射的**持久化**形态。
///
/// 同一条记录同时描述两端：hub 只关心 `listen` / `spoke`，spoke 只关心
/// `local`。把两端放在一条记录里，面板才能一眼看出「公网 2222 → 本机 8080」
/// 这条完整链路，也能据此生成任意一侧的配置文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    /// 稳定 id（不随重命名变化），面板用它做行标识。
    #[serde(default)]
    pub id: String,
    /// 服务名，hub 与 spoke 的匹配键。必须非空、唯一。
    #[serde(default)]
    pub name: String,
    /// 关掉的映射保留记录但不生效（不占端口）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// hub 侧公网监听，如 `0.0.0.0:2222`。spoke 角色下可留空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// hub 侧把该服务钉死到某个 spoke id。留空表示任意 spoke 均可提供。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoke: Option<String>,
    /// spoke 侧本地转发目标，如 `127.0.0.1:8080`。hub 角色下可留空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
    /// 自由备注，只用于人看。
    #[serde(default)]
    pub note: String,
}

fn default_true() -> bool {
    true
}

impl Mapping {
    pub fn new_id() -> String {
        // 不引入 uuid 依赖：纳秒时间戳 + 进程内计数器足够做行标识。
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        format!("m{:x}{:02x}", nanos, seq & 0xff)
    }
}

/// 创建 / 更新映射的入参。
///
/// `id` 为空表示新建；非空表示更新同 id 的记录。语义上是一次 upsert，
/// 这样面板的「保存」按钮对新建和编辑是同一条路径。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingUpsert {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub listen: Option<String>,
    #[serde(default)]
    pub spoke: Option<String>,
    #[serde(default)]
    pub local: Option<String>,
    #[serde(default)]
    pub note: String,
}

/// 单条映射当前是否真的在服务。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MappingState {
    /// 已生效：端口已绑定（hub）或已声明给 hub（spoke）。
    Active,
    /// 配置为启用但尚未生效，例如端口刚被释放、等待下一步。
    Pending,
    /// 明确关闭。
    Disabled,
    /// 生效失败，`MappingView::error` 里有原因（端口被占用等）。
    Error,
}

/// 一条映射的**运行态**视图：持久化字段 + 引擎观察到的实时信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MappingView {
    #[serde(flatten)]
    pub mapping: Mapping,
    pub state: MappingState,
    /// `state = error` 时的原因，否则为 `None`。
    #[serde(default)]
    pub error: Option<String>,
    /// 实际绑定到的地址（hub 角色，`listen` 可能写成 `:0` 由系统分配）。
    #[serde(default)]
    pub bound: Option<String>,
    /// 当前能提供该服务的 spoke id（hub 角色）；spoke 角色下为自己的 id。
    #[serde(default)]
    pub providers: Vec<String>,
    #[serde(default)]
    pub stats: MappingStats,
}

/// 一个映射的累计计数。全部单调递增，方便面板算速率。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct MappingStats {
    /// 累计接受的连接数。
    #[serde(default)]
    pub connections_total: u64,
    /// 当前仍在传输的连接数。
    #[serde(default)]
    pub connections_active: u64,
    /// 客户端 → 目标 的字节数。
    #[serde(default)]
    pub bytes_up: u64,
    /// 目标 → 客户端 的字节数。
    #[serde(default)]
    pub bytes_down: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// 引擎状态
// ─────────────────────────────────────────────────────────────────────────────

/// hub 侧一条已建立的 spoke 连接。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpokeView {
    pub id: String,
    /// 对端套接字地址，便于排查 NAT / 出口 IP。
    #[serde(default)]
    pub peer: String,
    /// 对端自报的 agent 版本。
    #[serde(default)]
    pub agent: String,
    /// 这条连接被授权的服务名。
    #[serde(default)]
    pub granted: Vec<String>,
    #[serde(default)]
    pub connected_secs: u64,
}

/// 引擎整体状态。面板首页用它画顶部信息条。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStatus {
    pub role: Role,
    /// 引擎是否在跑（宿主随时可以停掉它）。
    pub running: bool,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub uptime_secs: u64,
    /// hub 角色：控制端口监听地址。
    #[serde(default)]
    pub control_listen: Option<String>,
    /// spoke 角色：hub 地址。
    #[serde(default)]
    pub hub_server: Option<String>,
    /// spoke 角色：与 hub 的连接是否在线。
    #[serde(default)]
    pub hub_connected: Option<bool>,
    /// hub 角色：当前所有 spoke 连接。
    #[serde(default)]
    pub spokes: Vec<SpokeView>,
    /// 引擎对操作者的提示（例如「映射已保存，重启后生效」）。
    #[serde(default)]
    pub notices: Vec<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// 其他
// ─────────────────────────────────────────────────────────────────────────────

/// `config.export` 的结果：给定角色生成一份可以直接喂给 `esc hub/spoke`
/// 的静态配置文本。用来把面板里调好的映射落成传统的 TOML 文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedConfig {
    pub role: Role,
    /// 建议的文件名，如 `hub.toml`。
    pub filename: String,
    pub toml: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// 边缘路由（域名前缀 → 本机端口）
// ─────────────────────────────────────────────────────────────────────────────

/// 一条边缘路由。
///
/// `prefix` 是**域名第一段**（`cad`）或**完整域名**（`cad.example.com`），
/// 一律小写。完整域名优先匹配 —— 这样同一个前缀在不同域名下可以指向
/// 不同后端。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeRoute {
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub port: u16,
}

/// 新增/覆盖一条路由的入参。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeRouteUpsert {
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub port: u16,
    /// 编辑前的路由键；改名时与写入新键在同一次原子替换中完成。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_prefix: Option<String>,
}

/// 边缘路由的计数器。
///
/// 只做全局聚合：按前缀计数要在无锁热路径上加锁，收益不抵成本；日志里有
/// `prefix=` 字段，`grep` 就能统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeRouterCounters {
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub proxied: u64,
    #[serde(default)]
    pub unrouted: u64,
    #[serde(default)]
    pub upstream_errors: u64,
    /// 成功升级（101 + 双向搬运已建立）的连接数。
    #[serde(default)]
    pub upgraded: u64,
}

/// 边缘路由的运行状态。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeRouterStatus {
    /// 实际在听的地址（`0.0.0.0:2229`）。
    #[serde(default)]
    pub listens: Vec<String>,
    #[serde(default)]
    pub routes: Vec<EdgeRoute>,
    /// 没有前缀的 Host 兜底到哪个端口（`None` = 直接 421）。
    #[serde(default)]
    pub default_port: Option<u16>,
    /// 运行期改动落盘到哪个状态文件。`None` = 不持久化，重启回到配置文件的基线。
    ///
    /// 面板用它来提示"你刚才那条改动会不会活过重启" —— 这是本地/临时改配置
    /// 与真实部署之间最容易让人困惑的差别，所以放在状态里而不是靠文档。
    #[serde(default)]
    pub store: Option<String>,
    #[serde(default)]
    pub counters: EdgeRouterCounters,
}

/// 引擎侧的错误。用字符串是为了能跨进程边界无损传递。
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
#[error("{message}")]
pub struct ControlError {
    pub message: String,
    /// 机器可读的分类，面板可据此决定提示方式。
    #[serde(default)]
    pub kind: ErrorKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// 未分类。
    #[default]
    Other,
    /// 入参不合法（缺字段、格式错）。
    Invalid,
    /// 名称 / 端口重复。
    Conflict,
    /// 端口被占用、绑定失败。
    Bind,
    /// 目标不存在。
    NotFound,
    /// 引擎未运行或角色不支持该操作。
    Unavailable,
}

impl ErrorKind {
    /// 机器可读的短码。**这是契约的一部分**：宿主用它生成
    /// `HostCall` 的错误码，面板用它决定 HTTP 状态码。两边都调这一个
    /// 函数，所以不会出现「宿主说 notfound、面板只认 not_found」这种漂移。
    pub fn as_code(self) -> &'static str {
        match self {
            ErrorKind::Other => "other",
            ErrorKind::Invalid => "invalid",
            ErrorKind::Conflict => "conflict",
            ErrorKind::Bind => "bind",
            ErrorKind::NotFound => "not_found",
            ErrorKind::Unavailable => "unavailable",
        }
    }

    /// 把 [`Self::as_code`] 的结果解回来；未知码归为 [`ErrorKind::Other`]。
    pub fn from_code(code: &str) -> Self {
        match code {
            "invalid" => ErrorKind::Invalid,
            "conflict" => ErrorKind::Conflict,
            "bind" => ErrorKind::Bind,
            "not_found" => ErrorKind::NotFound,
            "unavailable" => ErrorKind::Unavailable,
            _ => ErrorKind::Other,
        }
    }

    /// 这个错误该对应哪个 HTTP 状态码。
    ///
    /// 放在契约里而不是面板里，是为了让「新增一个错误类别」只需要改一处
    /// （顺带迫使作者想清楚它对外应该长什么样）。
    pub fn http_status(self) -> u16 {
        match self {
            ErrorKind::Invalid => 400,
            ErrorKind::NotFound => 404,
            ErrorKind::Conflict => 409,
            ErrorKind::Bind => 409,
            ErrorKind::Unavailable => 503,
            ErrorKind::Other => 502,
        }
    }
}

impl ControlError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { message: message.into(), kind }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, message)
    }
    pub fn bind(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Bind, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unavailable, message)
    }
    pub fn other(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Other, message)
    }
}
