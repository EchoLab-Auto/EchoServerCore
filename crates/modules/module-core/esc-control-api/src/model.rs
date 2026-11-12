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

// ─────────────────────────────────────────────────────────────────────────────
// TLS 终结（`esc.tls`）
// ─────────────────────────────────────────────────────────────────────────────

/// TLS 终结层的运行时状态。
///
/// # 为什么要把它做成控制面的一部分
///
/// 这一层从前是 nginx（一个 Docker 容器），面板看不到它 —— 于是「2222 这个端口
/// 是谁在听、转发到哪、要不要登录」这个问题只能在三个页面之间来回切才拼得出来。
/// 收进 `esc-host` 之后它和隧道在同一个进程里，这条链路终于能在一屏内讲完：
///
/// ```text
/// :2222  ──TLS──▶  127.0.0.1:18790  ──隧道──▶  4090 的 127.0.0.1:8080
///  └─ site      ┘   └──── mapping.listen ────┘
/// ```
///
/// 面板靠 `site.target == mapping.listen` 把两段接起来。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsStatus {
    #[serde(default)]
    pub sites: Vec<TlsSite>,
    #[serde(default)]
    pub certs: Vec<TlsCertInfo>,
    /// SNI 没命中时用哪张证书（按 `TlsCertInfo::name`）。
    #[serde(default)]
    pub default_cert: Option<String>,
    /// 是否允许 TLS 1.2。
    ///
    /// 与 [`TlsDefaultsView::tls12`] 是同一个值，保留它是为了不破坏老面板的
    /// 反序列化。新代码请读 `defaults.tls12`。
    #[serde(default)]
    pub tls12: bool,
    #[serde(default)]
    pub counters: TlsCounters,
    /// 面板编辑用的**原始配置**：形状与 `tls.config.apply` 的入参**完全一致**。
    ///
    /// 为什么不给一堆 `defaults.*` 平铺字段、让面板自己拼：上面的展示字段已经
    /// 合并过默认值、把 `upstream`/`redirect` 拼成了 `target`，从它们反推会把
    /// 「继承默认」写成「显式指定」（同一个数字，语义变了——改默认值不再影响
    /// 这个站点）。另外**私钥路径不在展示字段里**，不给原始配置就没法编辑证书。
    ///
    /// 面板的用法是：`structuredClone(status.config)` → 改 → 原样 POST 回去。
    #[serde(default)]
    pub config: TlsConfigInput,
}

/// 一个 TLS 终结站点（一个监听端口）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsSite {
    pub name: String,
    /// 实际在听的地址，如 `0.0.0.0:2222`。
    pub listen: String,
    /// `proxy` = 转发到本机端口；`redirect` = 301。
    ///
    /// 用字符串而不是枚举：这是跨进程/跨版本的契约，加一个动作不该让老面板
    /// 反序列化失败。
    pub kind: String,
    /// `kind = proxy` 时是上游地址（`127.0.0.1:18790`）；`redirect` 时是模板。
    pub target: String,
    #[serde(default)]
    pub websocket: bool,
    /// 是否把查询串写进访问日志。**面板要把它显示出来** —— 承载令牌的端口
    /// 靠它保证凭据不落盘，这是一个应该被看见的设置，而不是藏在配置里。
    #[serde(default)]
    pub log_query: bool,
    #[serde(default)]
    pub read_timeout_secs: u64,
    #[serde(default)]
    pub max_body_bytes: u64,
    #[serde(default)]
    pub hsts: bool,
}

/// 一张已加载的证书。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsCertInfo {
    /// 引用名（`[[tls.config.cert]].name`）。
    pub name: String,
    /// 证书文件路径。面板显示它，是因为**续期后要重新复制到这里再重启** ——
    /// 这条运维路径只有看见路径才想得起来。
    pub cert: String,
    /// 服务的 SNI 名字（小写）。空 = 只作为默认证书用。
    #[serde(default)]
    pub sni: Vec<String>,
    /// 是不是当前的默认证书。
    #[serde(default)]
    pub default: bool,
}

/// 累计计数。全部单调递增，面板可据此算速率。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TlsCounters {
    /// 完成 TLS 握手并进入 HTTP 的请求数。
    #[serde(default)]
    pub requests_total: u64,
    /// 握手失败次数。端口扫描器会造成零星几条，持续增长才是问题。
    #[serde(default)]
    pub handshake_failures: u64,
    /// 连不上 / 超时 / 上游协议错误。
    #[serde(default)]
    pub upstream_errors: u64,
    /// 因请求体超限被拒的次数。
    #[serde(default)]
    pub body_too_large: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// TLS 终结：写请求（面板编辑证书与站点）
// ─────────────────────────────────────────────────────────────────────────────
//
// 与上面的 `TlsSite` / `TlsCertInfo` **分开定义**，而不是复用：
//
// * 读结构里是**派生字段**（`kind`、`target` 已经拼成字符串，`max_body_bytes` 是
//   合并默认后的生效值）；写结构要的是**原始字段**（`upstream` / `redirect` 两个
//   Option，`max_body_bytes` 是可继承的 Option）。混用会让"把 status 改一改发
//   回来"变成一件要推理的事。
// * 读结构承诺稳定，写结构随"允许改什么"变化。分开才好各自演化。
//
// 这里**不加** `deny_unknown_fields`：面板很可能把 status 里的一条原样带回来
// （多出 `kind` / `default` 之类），宽容忽略比报 "unknown field `kind`" 友好。
// 真正的字段校验在插件侧——那里面对的是 TOML，拼错必须报错。

/// 一张证书的输入。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsCertInput {
    /// 引用名。`defaults.cert` 与站点按它互相指认。
    pub name: String,
    /// PEM 证书链路径（叶子在前，中间证书在后）。
    pub cert: String,
    /// PEM 私钥路径。
    pub key: String,
    /// 这张证书服务哪些 SNI 名字。空 = 只作默认证书用。
    #[serde(default)]
    pub sni: Vec<String>,
}

/// 一个站点的输入。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsSiteInput {
    pub name: String,
    /// 监听地址。一个或多个（同一后端挂多个端口，例如 443 与 2224）。
    #[serde(default, deserialize_with = "one_or_many")]
    pub listen: Vec<String>,
    /// 明文转发目标，如 `127.0.0.1:18790`。与 `redirect` 二选一。
    ///
    /// **必须是 `IP:端口` 字面量**，不查 DNS —— 主机名（`localhost:8080`）会被拒。
    /// 面板要在提交前自查，就得知道这条；写在这里而不是只在插件源码里。
    #[serde(default)]
    pub upstream: Option<String>,
    /// 301 目标模板，如 `https://{hostname}:2224{uri}`。与 `upstream` 二选一。
    #[serde(default)]
    pub redirect: Option<String>,
    #[serde(default)]
    pub websocket: bool,
    #[serde(default)]
    pub log_query: bool,
    /// 等上游响应的超时，秒。缺省 300。
    ///
    /// **0 会被拒绝**（不是"不限制"、也不是钳到 1 秒）——把它静默钳成 1 秒的结果
    /// 是「偶发 502，只在慢请求上出现」，那是线上最难查的一类症状。
    #[serde(default = "default_read_timeout")]
    pub read_timeout_secs: u64,
    /// 这个端口的请求体上限，覆盖 `defaults.max_body_bytes`。`None` = 继承。
    #[serde(default)]
    pub max_body_bytes: Option<u64>,
    /// 这个端口是否发 HSTS，覆盖 `defaults.hsts`。`None` = 继承。
    #[serde(default)]
    pub hsts: Option<bool>,
}

/// `defaults` 段的输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsDefaultsInput {
    #[serde(default)]
    pub cert: Option<String>,
    #[serde(default = "yes")]
    pub tls12: bool,
    #[serde(default = "default_body_limit")]
    pub max_body_bytes: u64,
    #[serde(default = "default_handshake_timeout")]
    pub handshake_timeout_secs: u64,
    #[serde(default)]
    pub hsts: bool,
}

impl Default for TlsDefaultsInput {
    fn default() -> Self {
        Self {
            cert: None,
            tls12: true,
            max_body_bytes: default_body_limit(),
            handshake_timeout_secs: default_handshake_timeout(),
            hsts: false,
        }
    }
}

/// 整份配置 —— `tls.config.apply` 的入参。
///
/// 一次提交**整份**而不是逐条 upsert，是刻意的：站点之间有约束（端口不能重复、
/// 自环要拒、`defaults.cert` 必须指向存在的证书），逐条提交会让中间态非法，
/// 而合法的中间态又不知该按哪一版校验。整份提交 = 一次校验 = 一次热切换。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsConfigInput {
    #[serde(default)]
    pub defaults: TlsDefaultsInput,
    #[serde(default)]
    pub cert: Vec<TlsCertInput>,
    #[serde(default)]
    pub site: Vec<TlsSiteInput>,
}

/// `tls.config.apply` 的结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsApplyOutcome {
    /// 应用之后的**实际**状态（与 `tls.status` 同形）。
    ///
    /// 回给调用方而不是让它再查一次：中间若有人又改了一次，两次结果会不一致。
    #[serde(default)]
    pub status: TlsStatus,
    /// 这次新绑定（或重绑）的监听地址。
    #[serde(default)]
    pub rebound: Vec<String>,
    /// 这次关掉的监听地址。
    #[serde(default)]
    pub released: Vec<String>,
    /// 完全没动的端口数 —— 面板用它说明"其余 N 个端口未受影响"。
    #[serde(default)]
    pub unchanged: usize,
    /// 落盘路径。`None` = **没有写盘**（没配 `store`，或写盘失败——见 `warn`）。
    ///
    /// 面板必须把它显示出来：不然「保存成功了」和「重启后没了」之间的落差
    /// 要等到重启那天才发现。
    #[serde(default)]
    pub store: Option<String>,
    /// 需要当场告诉用户的坏消息。
    ///
    /// 目前只有一种：配置**已经在运行期生效**，但写盘失败。这时候返回 `Err`
    /// 是错的（用户会以为没生效，再点一次），返回纯成功也是错的（重启就丢）。
    /// 所以带上这句话，让面板原样显示。
    #[serde(default)]
    pub warn: Option<String>,
}

fn yes() -> bool {
    true
}

fn default_body_limit() -> u64 {
    12 * 1024 * 1024
}

fn default_handshake_timeout() -> u64 {
    10
}

fn default_read_timeout() -> u64 {
    300
}

/// `listen = "0.0.0.0:1"` 与 `listen = ["0.0.0.0:1", "0.0.0.0:2"]` 都接受。
///
/// 手写 curl 时人人都会写成字符串；面板发的是数组。两种都收，省一次往返。
fn one_or_many<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
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
