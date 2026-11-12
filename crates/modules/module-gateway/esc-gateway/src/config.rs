//! 网关配置：**一条 route = 一个公网端口 → 一个上游**。
//!
//! 配置是静态的，改完要重启。这不是懒得做热加载——认证拓扑必须能在重启这个
//! 动作里被完整替换，否则「先改配置再打」这类事故没有干净的收尾点。

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use esc_panel_auth::AllowRule;

/// 一整个网关配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    /// 登录页标题。
    #[serde(default = "default_title")]
    pub title: String,

    /// 会话存活时间（小时）。
    #[serde(default = "default_session_ttl")]
    pub session_ttl_hours: u64,

    /// cookie 名。多个网关实例共存时可以区分。
    #[serde(default = "default_cookie")]
    pub cookie_name: String,

    /// 会话表上限。达到后按「最久未使用」淘汰，防止被登录请求撑爆内存。
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,

    /// 是否信任 `X-Forwarded-For`。
    ///
    /// 默认 `false`，且**强烈建议保持 `false`**。只有当你确定网关前面还有一层
    /// 自己控制的反代、并且那层会覆盖（而非追加）这个头时才打开。
    #[serde(default)]
    pub trust_forwarded_for: bool,

    /// 是否给 cookie 加 `Secure`。有 TLS 终结时打开。
    #[serde(default)]
    pub secure_cookie: bool,

    #[serde(default)]
    pub rate_limit: RateLimitConfig,

    /// 账号库文件（管理面板管理的账号）。
    ///
    /// 不设则默认取**配置文件同目录**的 `gateway-users.toml`
    /// （`/etc/esc/gateway.toml` → `/etc/esc/gateway-users.toml`）。
    /// 目录不可写时面板自动降级为只读，并把这个原因显示在页面上。
    ///
    /// 同名账号以账号库为准——这样配置文件永远是一条可用的救援通道。
    #[serde(default)]
    pub users_file: Option<PathBuf>,

    /// 路由库文件（管理面板管理的端口 → 上游）。
    ///
    /// 不设则默认取配置文件同目录的 `gateway-routes.toml`。
    /// 同名路由以路由库为准——配置文件因此永远是救援通道：路由库坏了，
    /// 里面的端口照样有人听，包括管理面板自己那个。
    #[serde(default)]
    pub routes_file: Option<PathBuf>,

    /// 管理面板（含登录页）的**独立监听端口**。
    ///
    /// 不设时管理面板挂在每个 route 端口的 `/__admin/` 上——方便，但管理面和
    /// 业务面共用同一个公网端口，只有一道来源白名单隔着。
    ///
    /// 设了之后：
    /// * 管理面板**只**在这个端口上提供；
    /// * 其他端口上的 `/__admin/` 变成一页「管理面板在别处」的说明；
    /// * cookie 是 host 级的（浏览器不按端口隔离），所以在这个端口登录一次，
    ///   打开被代理的面板不用再登录。
    ///
    /// 举个典型的用法：`admin_listen = "127.0.0.1:2226"`，外面用 SSH 隧道进来。
    #[serde(default)]
    pub admin_listen: Option<String>,

    /// 允许访问管理面板的来源白名单（空 = 不限）。和账号认证是**与**关系。
    ///
    /// 网关本身就在公网上，管理面板挂在同一个端口上。想「只从内网/VPN
    /// 进去改账号」就填这里，不必为此多占一个端口。
    #[serde(default)]
    pub admin_allowed_cidrs: Vec<String>,

    /// 用户表。至少一个，且每个都必须有 `hash`。
    #[serde(default)]
    pub user: Vec<UserConfig>,

    /// 路由表。至少一个。
    #[serde(default)]
    pub route: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// 同一来源允许的登录失败次数（窗口内）。
    #[serde(default = "default_max_failures")]
    pub max_failures: u32,
    /// 失败计数窗口。
    #[serde(default = "default_fail_window")]
    pub fail_window_secs: u64,
    /// **全局**失败上限（跨所有来源）。防止攻击者换 IP 扫。
    ///
    /// 只按 IP 限速在分布式尝试下是无效的：对方有 1000 个 IP，
    /// 每 IP 只试 8 次就是 8000 次。所以再加一道全局闸。
    #[serde(default = "default_global_failures")]
    pub max_failures_global: u32,
    /// 全局窗口。
    #[serde(default = "default_global_window")]
    pub global_window_secs: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_failures: default_max_failures(),
            fail_window_secs: default_fail_window(),
            max_failures_global: default_global_failures(),
            global_window_secs: default_global_window(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    pub name: String,
    /// argon2id 哈希（`$argon2id$...`）。**不接受明文**——配置里出现明文口令
    /// 等于把口令写进了备份、日志与 git 历史。
    pub hash: String,
    /// 允许访问的 route 名。`["*"]` = 全部。
    #[serde(default)]
    pub routes: Vec<String>,
    /// 是否允许进管理面板（`/__admin/`）。
    ///
    /// 刻意默认 `false`：能改账号 = 能给自己发权限，这个开关不该靠「碰巧
    /// 复制了别人的配置」打开。
    #[serde(default)]
    pub admin: bool,
}

/// 一条路由的**配置形态**。既可反序列化（gateway.toml / 路由库），
/// 也可序列化（面板改完写回路由库）。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub name: String,
    /// 对外监听地址，如 `0.0.0.0:2223`。
    pub listen: String,
    /// 上游地址，如 `127.0.0.1:18787`。
    pub upstream: String,
    /// 只在 `Host` 命中这个域名时把请求转给本路由。`None` = 这个端口上的默认路由。
    ///
    /// # 为什么需要它
    ///
    /// 没有它时**端口就是路由的身份**（`RouteRegistry::at` 按 `local_addr` 查），
    /// 于是每接一个服务就要多占一个监听端口。cloud2 上就是这样堆出
    /// 18784（网关门）→ 18788（面板本尊）两份地址，而 18784 的全部作用只是
    /// 「在这里拦一道认证」。
    ///
    /// 有了它，一个监听地址可以服务多个上游：
    ///
    /// ```toml
    /// [[route]]
    /// name = "panel"; host = "panel.example.com"; listen = "0.0.0.0:8080"
    /// upstream = "127.0.0.1:18788"
    /// [[route]]
    /// name = "cad";   host = "cad.example.com";   listen = "0.0.0.0:8080"
    /// upstream = "127.0.0.1:2228"
    /// ```
    ///
    /// # 匹配规则
    ///
    /// 大小写不敏感的全等，**端口被剥掉**（`Host: a.com:8080` 与 `a.com` 同一条），
    /// 不做通配。理由与 TLS 的 SNI 选择一样：通配会让「哪个请求去了哪个上游」
    /// 变成一件要推理的事，而这里通常只有两三个名字。
    ///
    /// 同一端口上最多只能有**一条**不带 `host` 的路由（它就是兜底）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// 额外的来源白名单（和用户认证是**与**关系）。空 = 不限。
    #[serde(default)]
    pub allowed_cidrs: Vec<String>,
    /// 转发前从请求路径里剥掉的前缀（上游按 `/` 认路径时用）。
    #[serde(default)]
    pub strip_prefix: Option<String>,
    /// 请求体上限（字节）。0 = 用全局默认。
    #[serde(default)]
    pub max_body_bytes: Option<u64>,
}

impl RouteConfig {
    /// 校验单条路由并转成运行时形态。
    pub fn to_runtime(&self) -> Result<RouteRuntime, String> {
        if !valid_name(&self.name) {
            return Err(format!(
                "route 名 {:?} 不合法：只能用字母、数字、`.` `-` `_`，长度 1..=64",
                self.name
            ));
        }
        let addr: SocketAddr = self.listen.parse().map_err(|_| {
            format!(
                "route {:?} 的 listen 不是合法地址: {:?}（需要 host:port，如 0.0.0.0:2224）",
                self.name, self.listen
            )
        })?;
        let up: SocketAddr = self.upstream.parse().map_err(|_| {
            format!(
                "route {:?} 的 upstream 不是合法地址: {:?}（需要 host:port，如 127.0.0.1:18788）",
                self.name, self.upstream
            )
        })?;
        // 上游指回自己 = 无限循环。这类错误一旦上线就是 CPU 100% 且难以定位。
        if up == addr {
            return Err(format!(
                "route {:?} 的 upstream 与 listen 相同（{up}），会形成转发环路",
                self.name
            ));
        }
        // `host` 是「只在 Host 命中时才走本路由」的条件。空串当没写——面板的输入框
        // 清空之后发上来的是 `""`，那不是「匹配空域名」，而是「不限定」。
        let host = self
            .host
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(|h| normalize_host(h))
            .transpose()
            .map_err(|e| format!("route {:?} 的 host: {e}", self.name))?;

        if let Some(p) = &self.strip_prefix {
            if !p.starts_with('/') {
                return Err(format!(
                    "route {:?} 的 strip_prefix 必须以 / 开头",
                    self.name
                ));
            }
            if p.contains(['?', '#']) || p.chars().any(|c| c.is_control()) {
                return Err(format!(
                    "route {:?} 的 strip_prefix 不能包含 ? # 或控制字符",
                    self.name
                ));
            }
        }
        if self.max_body_bytes == Some(0) {
            return Err(format!(
                "route {:?} 的 max_body_bytes 不能是 0（留空表示默认 10 MB）",
                self.name
            ));
        }
        if let Some(n) = self.max_body_bytes {
            if n > 1024 * 1024 * 1024 {
                return Err(format!(
                    "route {:?} 的 max_body_bytes 过大（上限 1 GiB）",
                    self.name
                ));
            }
        }
        let mut allow = Vec::new();
        for c in &self.allowed_cidrs {
            allow.push(
                AllowRule::parse(c)
                    .map_err(|e| format!("route {:?} 的 allowed_cidrs: {e}", self.name))?,
            );
        }
        Ok(RouteRuntime {
            name: self.name.clone(),
            listen: addr,
            upstream: up,
            host,
            allow,
            allowed_cidrs_raw: self.allowed_cidrs.clone(),
            strip_prefix: self.strip_prefix.clone(),
            max_body_bytes: self.max_body_bytes.unwrap_or(DEFAULT_MAX_BODY_BYTES),
        })
    }
}

/// `Host: a.com:8080` → `a.com`。`[::1]:8080` → `[::1]`。
fn strip_port(host: &str) -> String {
    if let Some(rest) = host.strip_prefix('[') {
        // IPv6 字面量：端口在 `]` 之后。
        return match rest.split_once(']') {
            Some((inner, _)) => format!("[{inner}]"),
            None => host.to_string(),
        };
    }
    match host.rsplit_once(':') {
        Some((head, tail)) if tail.chars().all(|c| c.is_ascii_digit()) => head.to_string(),
        _ => host.to_string(),
    }
}

/// 把配置里的 host 归一化，顺便校验。
///
/// 归一化到「小写 + 无端口」：`Example.COM:8080` 与 `example.com` 是同一条路由。
/// 不做通配（`*.example.com`）——理由见 [`RouteConfig::host`]。
fn normalize_host(raw: &str) -> Result<String, String> {
    let h = raw.trim().to_ascii_lowercase();
    // 允许顺手写成 host:port（从浏览器地址栏复制来的就是那样），端口直接丢掉。
    let h = match h.rsplit_once(':') {
        // 方括号形式 `[::1]:8080` 先放过——IPv6 字面量当 Host 匹配没有实际用途，
        // 但也不该因为一个冒号就报错。
        Some((head, tail)) if !head.ends_with(']') || tail.chars().all(|c| c.is_ascii_digit()) => {
            if tail.chars().all(|c| c.is_ascii_digit()) && !tail.is_empty() {
                head.to_string()
            } else {
                h.clone()
            }
        }
        _ => h.clone(),
    };
    if h.is_empty() {
        return Err("不能是空串（要去掉这一项就整行删掉 host）".into());
    }
    if h.len() > 253 {
        return Err(format!("{raw:?} 太长（域名上限 253 字符）"));
    }
    if h.contains('/') || h.contains('?') || h.contains('#') || h.contains(' ') {
        return Err(format!(
            "{raw:?} 里有 / ? # 或空格——这里只填域名（如 panel.example.com），不要带路径"
        ));
    }
    if h.contains('*') {
        return Err(format!(
            "{raw:?} 里有通配符。这里只做**精确匹配**：真要泛域名，在 DNS 里加 \
             *.example.com，路由这边仍然写具体的名字"
        ));
    }
    if !h
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' || c == ':')
    {
        return Err(format!(
            "{raw:?} 含非法字符：只允许字母、数字、`.` `-` `_`（值必须是 ASCII/IDN 的 punycode 形式）"
        ));
    }
    Ok(h)
}

/// 两个监听地址会不会抢同一个端口。
///
/// 只看 `SocketAddr` 是否相等是不够的：`0.0.0.0:2224` 和 `127.0.0.1:2224`
/// 在 bind 的时候是实打实的冲突（后者会 EADDRINUSE），但在结构体上不相等。
pub fn listeners_conflict(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() == b.port() && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
}

/// 两条路由会不会抢同一个请求。
///
/// 抢同一个**端口**本身不算冲突——那正是 host 分流的意义。真正冲突的是
/// 「同一个端口 + 会命中的 Host 集合有交集」：
///
/// * 两条都没有 host → 冲突（一个端口只能有一个兜底）；
/// * 一条有 host、一条没有 → 冲突吗？**不冲突**：带 host 的先命中，剩下落兜底。
///   等等——这也不对。兜底会吃掉所有 host，等于把"按域名分流"变得不可预测。
///   （见下面 `validate_route_list` 的实际判定，这里只放判据。）
pub fn routes_overlap(a: &RouteRuntime, b: &RouteRuntime) -> bool {
    if !listeners_conflict(a.listen, b.listen) {
        return false;
    }
    match (&a.host, &b.host) {
        (None, None) => true,
        (Some(x), Some(y)) => x == y,
        // 一条兜底 + 一条带 host：**允许**。兜底是显式写下来的意图（"其余的给我"），
        // 而带 host 的那条在查表时优先命中。所以这不是"抢占"，是分层：
        //
        //     panel.example.com → panel 这条（精确命中优先）
        //     其它任何 Host      → 兜底那条
        //
        // 反过来禁掉会让人没法配「按域名分流 + 留一个默认站」这种最常见的形态。
        _ => false,
    }
}

/// 校验一组路由（可能是"配置文件 ∪ 路由库"的合并结果）。
pub fn validate_route_list(routes: &[RouteConfig]) -> Result<Vec<RouteRuntime>, String> {
    let mut names = HashSet::new();
    let mut out: Vec<RouteRuntime> = Vec::with_capacity(routes.len());
    for r in routes {
        let rt = r.to_runtime()?;
        if !names.insert(rt.name.clone()) {
            return Err(format!("route 名 {:?} 重复", rt.name));
        }
        if let Some(other) = out.iter().find(|o| routes_overlap(o, &rt)) {
            let why = match (&other.host, &rt.host) {
                (None, None) => format!(
                    "{} 与 {} 都监听 {} 且都没写 host——同一个端口只能有一个兜底",
                    other.name, rt.name, rt.listen
                ),
                _ => format!(
                    "{} 与 {} 都监听 {} 且都要接 Host = {} 的请求",
                    other.name,
                    rt.name,
                    rt.listen,
                    rt.host.as_deref().unwrap_or("?")
                ),
            };
            return Err(format!("{why}。给其中一条换 host，或换一个端口。"));
        }
        out.push(rt);
    }
    Ok(out)
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            title: default_title(),
            session_ttl_hours: default_session_ttl(),
            cookie_name: default_cookie(),
            max_sessions: default_max_sessions(),
            trust_forwarded_for: false,
            secure_cookie: false,
            rate_limit: RateLimitConfig::default(),
            user: Vec::new(),
            route: Vec::new(),
            users_file: None,
            routes_file: None,
            admin_listen: None,
            admin_allowed_cidrs: Vec::new(),
        }
    }
}

fn default_title() -> String {
    "esc 网关".into()
}
fn default_session_ttl() -> u64 {
    8
}
fn default_cookie() -> String {
    "esc_gw".into()
}
fn default_max_sessions() -> usize {
    4096
}
/// 路由库的默认位置：配置文件同目录的 `gateway-routes.toml`。
pub fn default_routes_file(config_path: &Path) -> PathBuf {
    config_path.with_file_name("gateway-routes.toml")
}

/// 账号库的默认位置：配置文件同目录的 `gateway-users.toml`。
pub fn default_users_file(config_path: &Path) -> PathBuf {
    config_path.with_file_name("gateway-users.toml")
}

/// 用户名的合法字符集。
///
/// 严格限制不是为了洁癖：用户名会出现在 URL 路径（`/__admin/users/<名字>`）、
/// 表单值和 HTML 里。三者各自有转义规则，允许任意字符就等于要求三处都不能
/// 出错。限定成 ASCII 标识符后，这三个上下文里都是原样安全的。
pub fn valid_user_name(name: &str) -> bool {
    valid_name(name)
}

/// 账号名与路由名共用同一套字符集：它们都会进 URL 路径、表单值和 HTML。
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn default_max_failures() -> u32 {
    8
}
fn default_fail_window() -> u64 {
    60
}
fn default_global_failures() -> u32 {
    200
}
fn default_global_window() -> u64 {
    300
}

/// 请求体默认上限。面板不需要上传，但网关是通用的——给 10 MB 足够任何
/// 「贴一段配置」的场景，又不至于让人一个请求打满内存。
pub const DEFAULT_MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;

impl GatewayConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("读取 {} 失败: {e}", path.display()))?;
        let mut cfg: Self = toml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("解析 {} 失败: {e}", path.display()))?;
        // 没显式配 users_file 就落到配置文件同目录：这样「不加任何配置也能用
        // 管理面板」成立，而不至于因为漏配一个字段就得到一个只读面板。
        if cfg.users_file.is_none() {
            cfg.users_file = Some(default_users_file(path));
        }
        if cfg.routes_file.is_none() {
            cfg.routes_file = Some(default_routes_file(path));
        }
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("配置无效: {e}"))?;
        Ok(cfg)
    }

    /// 校验配置自洽性。返回 `Err` 会让网关拒绝启动。
    pub fn validate(&self) -> Result<(), String> {
        if self.user.is_empty() {
            return Err("至少要配置一个 [[user]]，否则谁都无法登录".into());
        }
        if self.route.is_empty() {
            return Err("至少要配置一个 [[route]]，否则网关什么都不转发".into());
        }
        if self.session_ttl_hours == 0 {
            return Err("session_ttl_hours 必须大于 0".into());
        }
        if self.cookie_name.is_empty()
            || !self
                .cookie_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return Err("cookie_name 只允许字母数字与 - _".into());
        }

        // ── 用户 ──
        let mut names = HashSet::new();
        for u in &self.user {
            if !names.insert(u.name.as_str()) {
                return Err(format!("用户名 {:?} 重复", u.name));
            }
            if !valid_user_name(&u.name) {
                return Err(format!(
                    "用户名 {:?} 不合法：只能用字母、数字、`.` `-` `_`，长度 1..=64",
                    u.name
                ));
            }
            // 明确拒绝明文：这是最容易犯、后果最重的配置错误。
            if !u.hash.starts_with("$argon2") {
                return Err(format!(
                    "用户 {:?} 的 hash 不是 argon2 格式。请用 `esc-gateway hash-password` 生成，\
                     不要直接写明文口令",
                    u.name
                ));
            }
            if u.routes.is_empty() {
                return Err(format!(
                    "用户 {:?} 的 routes 为空，将无法访问任何页面",
                    u.name
                ));
            }
        }

        for c in &self.admin_allowed_cidrs {
            AllowRule::parse(c).map_err(|e| format!("admin_allowed_cidrs: {e}"))?;
        }

        // 管理端口不能和任何转发端口撞车——否则谁先绑上谁赢，而输的那个
        // 可能是管理面板本身（那就再也进不去了）。
        if let Some(raw) = &self.admin_listen {
            let admin: SocketAddr = raw.parse().map_err(|_| {
                format!("admin_listen 不是合法地址: {raw:?}（需要 host:port，如 127.0.0.1:2226）")
            })?;
            for r in &self.route {
                if let Ok(rt) = r.to_runtime() {
                    if listeners_conflict(rt.listen, admin) {
                        return Err(format!(
                            "admin_listen 与 route {:?} 想监听同一个端口 {}，请改成不同端口",
                            rt.name,
                            admin.port()
                        ));
                    }
                }
            }
        }

        // ── 路由 ──
        let mut rnames = HashSet::new();
        for r in &self.route {
            r.to_runtime()?;
            if !rnames.insert(r.name.as_str()) {
                return Err(format!("route 名 {:?} 重复", r.name));
            }
        }
        // 冲突判定与 `validate_route_list` 共用同一套判据：**同端口并不算冲突**，
        // 除非两条路由会命中的 Host 集合有交集（那才是真抢同一个请求）。
        //
        // 从前这里是 `listeners_conflict` 单判端口，于是「一个监听地址服务多个
        // 上游」这种配置在启动时就被拒了 —— 而那正是省掉多余监听地址的办法。
        let mut seen: Vec<RouteRuntime> = Vec::new();
        for r in &self.route {
            let rt = r.to_runtime()?;
            if let Some(other) = seen.iter().find(|o| routes_overlap(o, &rt)) {
                let why = match (&other.host, &rt.host) {
                    (None, None) => format!(
                        "{} 与 {} 都监听 {} 且都没写 host —— 同一个端口只能有一个兜底",
                        other.name, rt.name, rt.listen
                    ),
                    _ => format!(
                        "{} 与 {} 都监听 {} 且都要接 Host = {} 的请求",
                        other.name,
                        rt.name,
                        rt.listen,
                        rt.host.as_deref().unwrap_or("?")
                    ),
                };
                return Err(format!("{why}。给其中一条换 host，或换一个端口。"));
            }
            seen.push(rt);
        }

        // ── 用户引用的 route 必须存在 ──
        for u in &self.user {
            for want in &u.routes {
                if want == "*" {
                    continue;
                }
                if !rnames.contains(want.as_str()) {
                    return Err(format!(
                        "用户 {:?} 引用了不存在的 route {want:?}（已定义: {:?}）",
                        u.name,
                        rnames.iter().collect::<Vec<_>>()
                    ));
                }
            }
        }

        // ── 限速参数 ──
        let rl = &self.rate_limit;
        if rl.fail_window_secs == 0 || rl.global_window_secs == 0 {
            return Err("限速窗口必须大于 0".into());
        }
        Ok(())
    }

    /// 预先解析好的运行时视图（避免每请求都 parse 字符串）。
    pub fn build(&self) -> Result<RuntimeConfig, String> {
        self.validate()?;
        let runtimes = validate_route_list(&self.route)?;
        let mut routes = HashMap::new();
        let mut listens = Vec::new();
        for rt in runtimes {
            listens.push(rt.listen);
            routes.insert(rt.name.clone(), rt);
        }
        let users = self
            .user
            .iter()
            .map(|u| {
                let all = u.routes.iter().any(|r| r == "*");
                (
                    u.name.clone(),
                    UserRuntime {
                        name: u.name.clone(),
                        hash: u.hash.clone(),
                        all_routes: all,
                        routes: u.routes.iter().filter(|r| *r != "*").cloned().collect(),
                        admin: u.admin,
                    },
                )
            })
            .collect();
        Ok(RuntimeConfig {
            title: self.title.clone(),
            cookie_name: self.cookie_name.clone(),
            session_ttl: std::time::Duration::from_secs(self.session_ttl_hours * 3600),
            max_sessions: self.max_sessions,
            trust_forwarded_for: self.trust_forwarded_for,
            secure_cookie: self.secure_cookie,
            rate_limit: self.rate_limit.clone(),
            users,
            routes,
            listens,
            users_file: self.users_file.clone(),
            routes_file: self.routes_file.clone(),
            admin_listen: self.admin_listen.as_deref().and_then(|s| s.parse().ok()),
            admin_allow: self
                .admin_allowed_cidrs
                .iter()
                .filter_map(|c| AllowRule::parse(c).ok())
                .collect(),
        })
    }
}

/// 一个用户（口令哈希已就绪）。
#[derive(Debug, Clone)]
pub struct UserRuntime {
    pub name: String,
    pub hash: String,
    pub all_routes: bool,
    pub routes: Vec<String>,
    /// 是否能进管理面板。登录时固化进会话（改权限 = 该账号的会话全部作废）。
    pub admin: bool,
}

impl UserRuntime {
    pub fn can_access(&self, route: &str) -> bool {
        self.all_routes || self.routes.iter().any(|r| r == route)
    }
}

/// 一条路由的运行时形态。
#[derive(Debug, Clone)]
pub struct RouteRuntime {
    pub name: String,
    pub listen: SocketAddr,
    pub upstream: SocketAddr,
    /// 归一化后的 host 条件（小写、无端口）。`None` = 这个端口的默认路由。
    pub host: Option<String>,
    pub allow: Vec<AllowRule>,
    /// 解析后的 `allow` 对应的原文。面板要显示、要回填编辑框，靠结构体反推
    /// 字符串既不可靠（规范化之后和用户写的不一样），也没必要。
    pub allowed_cidrs_raw: Vec<String>,
    pub strip_prefix: Option<String>,
    pub max_body_bytes: u64,
}

impl RouteRuntime {
    /// 这个路由是不是这个端口上的兜底（没有 host 条件）。
    pub fn is_default(&self) -> bool {
        self.host.is_none()
    }

    /// `Host` 头是否命中本路由。
    ///
    /// `None`（请求里没有 Host 头，HTTP/1.0 才可能）**不命中**任何带 host 条件的
    /// 路由，但命中兜底路由。不给它一个特例：没有 Host 就没法判断它想去哪，
    /// 猜一个比拒绝更糟。
    pub fn host_matches(&self, host: Option<&str>) -> bool {
        match (&self.host, host) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(want), Some(got)) => strip_port(&got.to_ascii_lowercase()) == *want,
        }
    }

    /// 来源是否被 route 级白名单放行。
    ///
    /// `peer` 为 `None` 时视为**不放行**（fail-closed）：拿不到来源地址就无法
    /// 判断，而白名单的意义就是「只允许这些」。
    pub fn source_allowed(&self, peer: Option<IpAddr>) -> bool {
        if self.allow.is_empty() {
            return true;
        }
        match peer {
            Some(ip) => self.allow.iter().any(|r| r.matches(ip)),
            None => false,
        }
    }
}

/// 校验通过后的运行时配置。
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub title: String,
    pub cookie_name: String,
    pub session_ttl: std::time::Duration,
    pub max_sessions: usize,
    pub trust_forwarded_for: bool,
    pub secure_cookie: bool,
    pub rate_limit: RateLimitConfig,
    pub users: HashMap<String, UserRuntime>,
    pub routes: HashMap<String, RouteRuntime>,
    pub listens: Vec<SocketAddr>,
    /// 账号库路径。`None` = 面板只读（只用配置文件里的账号）。
    pub users_file: Option<PathBuf>,
    /// 路由库路径。`None` = 端口只能在配置文件里改。
    pub routes_file: Option<PathBuf>,
    /// 管理面板的独立监听端口。`None` = 挂在每个 route 端口的 `/__admin/` 上。
    pub admin_listen: Option<SocketAddr>,
    /// 管理面板的来源白名单（空 = 不限）。
    pub admin_allow: Vec<AllowRule>,
}

impl RuntimeConfig {
    pub fn user(&self, name: &str) -> Option<&UserRuntime> {
        self.users.get(name)
    }

    /// 来源是否允许访问管理面板。
    pub fn admin_source_allowed(&self, peer: Option<IpAddr>) -> bool {
        if self.admin_allow.is_empty() {
            return true;
        }
        match peer {
            Some(ip) => self.admin_allow.iter().any(|r| r.matches(ip)),
            None => false,
        }
    }

    /// **配置文件里**的 route 名。合并视图见 `Gateway::route_names`。
    pub fn config_route_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.routes.keys().cloned().collect();
        v.sort();
        v
    }

    /// 按监听地址找 route。
    pub fn route_by_listen(&self, addr: SocketAddr) -> Option<&RouteRuntime> {
        self.routes.values().find(|r| r.listen == addr)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 口令哈希
// ─────────────────────────────────────────────────────────────────────────────

/// 用 argon2id 生成哈希（`esc-gateway hash-password` 用它）。
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    let salt = SaltString::generate(&mut OsRng);
    let hash = argon2_instance()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("哈希失败: {e}"))?;
    Ok(hash.to_string())
}

/// 固定参数的 argon2id 实例。
///
/// 显式构造而不是 `Argon2::default()`：后者的参数跟着库版本/feature 走，
/// 升级时就可能悄悄变化——而认证强度不该是一个隐式决定。
fn argon2_instance() -> argon2::Argon2<'static> {
    argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2_params(),
    )
}

fn argon2_params() -> argon2::Params {
    // OWASP 2024 推荐的最低档：19 MiB / t=2 / p=1。
    // 比库默认（19MiB, t=2）显式写出来，免得上游改默认值时认证强度悄悄变化。
    argon2::Params::new(19456, 2, 1, None).expect("argon2 参数是常量，不会失败")
}

/// 校验口令。**恒定时间**由 argon2 库保证（它比对的是重新计算出的哈希）。
pub fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    argon2_instance()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> GatewayConfig {
        let raw = r#"
[[user]]
name = "admin"
hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$11111111111111111111111111111111"
routes = ["*"]

[[route]]
name = "p1"
listen = "0.0.0.0:2223"
upstream = "127.0.0.1:18787"
"#;
        toml::from_str(raw).unwrap()
    }

    #[test]
    fn minimal_config_is_valid() {
        let c = minimal();
        assert!(c.validate().is_ok(), "{:?}", c.validate());
        let rt = c.build().unwrap();
        assert_eq!(rt.listens.len(), 1);
        assert!(rt.user("admin").unwrap().can_access("anything"));
    }

    #[test]
    fn plaintext_password_is_rejected_loudly() {
        let mut c = minimal();
        c.user[0].hash = "hunter2".into();
        let err = c.validate().unwrap_err();
        assert!(err.contains("argon2"), "错误信息应指明要 argon2: {err}");
        assert!(err.contains("hash-password"), "应告诉用户怎么做: {err}");
    }

    #[test]
    fn duplicate_ports_and_names_are_rejected() {
        let mut c = minimal();
        c.route.push(RouteConfig {
            name: "p2".into(),
            listen: "0.0.0.0:2223".into(), // 同端口
            upstream: "127.0.0.1:18788".into(),
            host: None,
            allowed_cidrs: vec![],
            strip_prefix: None,
            max_body_bytes: None,
        });
        assert!(
            c.validate().unwrap_err().contains("同一个端口"),
            "冲突信息要指出端口"
        );

        let mut c = minimal();
        c.route.push(RouteConfig {
            name: "p1".into(), // 同名
            listen: "0.0.0.0:2224".into(),
            upstream: "127.0.0.1:18788".into(),
            host: None,
            allowed_cidrs: vec![],
            strip_prefix: None,
            max_body_bytes: None,
        });
        assert!(c.validate().unwrap_err().contains("重复"));
    }

    #[test]
    fn upstream_equal_to_listen_is_rejected_as_a_loop() {
        let mut c = minimal();
        c.route[0].upstream = c.route[0].listen.clone();
        let err = c.validate().unwrap_err();
        assert!(err.contains("环路"), "{err}");
    }

    #[test]
    fn unknown_route_reference_is_rejected_with_the_available_names() {
        let mut c = minimal();
        c.user[0].routes = vec!["nope".into()];
        let err = c.validate().unwrap_err();
        assert!(err.contains("nope"));
        assert!(err.contains("p1"), "应列出可用 route: {err}");
    }

    #[test]
    fn empty_users_or_routes_are_rejected() {
        let mut c = minimal();
        c.user.clear();
        assert!(c.validate().unwrap_err().contains("user"));

        let mut c = minimal();
        c.route.clear();
        assert!(c.validate().unwrap_err().contains("route"));
    }

    #[test]
    /// 同一个端口上「两条兜底」必须被拒 —— 那等于两条路由抢同一个请求。
    #[test]
    fn two_default_routes_on_one_port_are_rejected() {
        let mk = |name: &str| RouteConfig {
            name: name.into(),
            listen: "0.0.0.0:8080".into(),
            upstream: "127.0.0.1:9000".into(),
            host: None,
            allowed_cidrs: vec![],
            strip_prefix: None,
            max_body_bytes: None,
        };
        let err = validate_route_list(&[mk("a"), mk("b")]).unwrap_err();
        assert!(err.contains("兜底"), "{err}");
    }

    /// 同一端口 + 同一 host 也不行：两条路由抢同一批请求。
    #[test]
    fn same_host_on_one_port_is_rejected() {
        let mk = |name: &str, host: &str| RouteConfig {
            name: name.into(),
            listen: "0.0.0.0:8080".into(),
            upstream: "127.0.0.1:9000".into(),
            host: Some(host.into()),
            allowed_cidrs: vec![],
            strip_prefix: None,
            max_body_bytes: None,
        };
        let err = validate_route_list(&[mk("a", "x.example.com"), mk("b", "X.Example.com")])
            .unwrap_err();
        assert!(err.contains("Host"), "{err}");
    }

    /// 同一端口：一条带 host、一条兜底 —— 这是最常见的形态，必须允许。
    #[test]
    fn host_plus_default_on_one_port_is_allowed() {
        let a = RouteConfig {
            name: "panel".into(),
            listen: "0.0.0.0:8080".into(),
            upstream: "127.0.0.1:9001".into(),
            host: Some("panel.example.com".into()),
            allowed_cidrs: vec![],
            strip_prefix: None,
            max_body_bytes: None,
        };
        let b = RouteConfig {
            name: "rest".into(),
            listen: "0.0.0.0:8080".into(),
            upstream: "127.0.0.1:9002".into(),
            host: None,
            allowed_cidrs: vec![],
            strip_prefix: None,
            max_body_bytes: None,
        };
        let out = validate_route_list(&[a, b]).expect("一条 host + 一条兜底应当允许");
        assert_eq!(out.len(), 2);
        // 两条都指向同一个端口，但只有一条是兜底
        assert!(out.iter().filter(|r| r.is_default()).count() == 1);
    }

    /// 三条同一端口、三个不同 host：全都要能起来。
    #[test]
    fn many_hosts_on_one_port_are_allowed() {
        let mk = |name: &str, host: &str| RouteConfig {
            name: name.into(),
            listen: "0.0.0.0:8080".into(),
            upstream: "127.0.0.1:9000".into(),
            host: Some(host.into()),
            allowed_cidrs: vec![],
            strip_prefix: None,
            max_body_bytes: None,
        };
        let out = validate_route_list(&[
            mk("panel", "panel.example.com"),
            mk("cad", "cad.example.com"),
            mk("shop", "shop.example.com"),
        ])
        .expect("三个不同 host 应当允许");
        assert_eq!(out.len(), 3);
    }

    /// host 归一化：大小写、端口、空白。
    #[test]
    fn host_is_normalized() {
        let mk = |raw: &str| RouteConfig {
            name: "a".into(),
            listen: "0.0.0.0:8080".into(),
            upstream: "127.0.0.1:9000".into(),
            host: Some(raw.into()),
            allowed_cidrs: vec![],
            strip_prefix: None,
            max_body_bytes: None,
        };
        assert_eq!(mk("Panel.Example.COM").to_runtime().unwrap().host.as_deref(), Some("panel.example.com"));
        assert_eq!(mk(" panel.example.com ").to_runtime().unwrap().host.as_deref(), Some("panel.example.com"));
        // 从浏览器地址栏复制来的那种带端口
        assert_eq!(mk("panel.example.com:8080").to_runtime().unwrap().host.as_deref(), Some("panel.example.com"));
        // 空串 = 不限定（面板清空输入框发上来的就是它）
        assert_eq!(mk("").to_runtime().unwrap().host, None);
    }

    /// host 里写路径 / 通配符要报错，并说清楚为什么不支持。
    #[test]
    fn bad_host_syntax_is_rejected() {
        let mk = |raw: &str| RouteConfig {
            name: "a".into(),
            listen: "0.0.0.0:8080".into(),
            upstream: "127.0.0.1:9000".into(),
            host: Some(raw.into()),
            allowed_cidrs: vec![],
            strip_prefix: None,
            max_body_bytes: None,
        };
        for (raw, want) in [
            ("panel.example.com/api", "路径"),
            ("*.example.com", "通配"),
            ("a b.com", "空格"),
        ] {
            let err = mk(raw).to_runtime().unwrap_err();
            assert!(err.contains(want), "{raw} → {err}");
        }
    }

    fn bad_cidr_and_relative_strip_prefix_are_rejected() {
        let mut c = minimal();
        c.route[0].allowed_cidrs = vec!["10.0.0.0/33".into()];
        assert!(c.validate().is_err());

        let mut c = minimal();
        c.route[0].strip_prefix = Some("api".into()); // 缺前导 /
        assert!(c.validate().unwrap_err().contains("strip_prefix"));
    }

    #[test]
    fn per_user_route_acl_is_enforced() {
        let mut c = minimal();
        c.route.push(RouteConfig {
            name: "p2".into(),
            listen: "0.0.0.0:2224".into(),
            upstream: "127.0.0.1:18788".into(),
            host: None,
            allowed_cidrs: vec![],
            strip_prefix: None,
            max_body_bytes: None,
        });
        c.user[0].routes = vec!["p1".into()];
        let rt = c.build().unwrap();
        let u = rt.user("admin").unwrap();
        assert!(u.can_access("p1"));
        assert!(!u.can_access("p2"), "未授权的 route 必须拒绝");
    }

    #[test]
    fn password_hash_round_trips_and_rejects_wrong_password() {
        let hash = hash_password("s3cret-口令").unwrap();
        assert!(hash.starts_with("$argon2id$"), "应为 argon2id: {hash}");
        assert!(verify_password("s3cret-口令", &hash), "多字节口令应能校验");
        assert!(!verify_password("s3cret-口令 ", &hash));
        assert!(!verify_password("", &hash));
        // 损坏的 hash 不能 panic，只能返回 false
        assert!(!verify_password("x", "not-a-hash"));
        assert!(!verify_password("x", ""));
        // 相同口令两次哈希应不同（salt 随机）
        assert_ne!(hash, hash_password("s3cret-口令").unwrap());
    }

    #[test]
    fn source_acl_fails_closed_without_a_peer() {
        let mut c = minimal();
        c.route[0].allowed_cidrs = vec!["10.0.0.0/8".into()];
        let rt = c.build().unwrap().routes["p1"].clone();
        assert!(rt.source_allowed(Some("10.1.2.3".parse().unwrap())));
        assert!(!rt.source_allowed(Some("192.168.1.1".parse().unwrap())));
        assert!(!rt.source_allowed(None), "拿不到来源时必须拒绝");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        // 拼错的键必须报错，否则「我明明配了」会成为谜案
        let raw = r#"
[[user]]
name = "a"
hash = "$argon2id$x"
routes = ["*"]
typo_field = 1
[[route]]
name = "p"
listen = "0.0.0.0:1"
upstream = "127.0.0.1:2"
"#;
        assert!(toml::from_str::<GatewayConfig>(raw).is_err());
    }
}
