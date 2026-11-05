//! 面板的 HTTP 访问控制：**Basic 认证 + 来源白名单 + 认证失败限速**。
//!
//! 抽成独立 crate 的理由只有一条：这套逻辑现在有两个使用者（隧道面板、服务器
//! 面板），而它属于「错一次就全盘皆输」的那类代码。两份实现意味着两处独立假定
//! ——改了一处忘了另一处，漏洞就留在那儿，而且不会有任何测试报错。
//!
//! 三道闸按顺序生效，任何一道拒绝都不再往下走：
//!
//! ```text
//! 请求 → 来源白名单(403) → 失败限速(429) → Basic 认证(401) → 到后端
//! ```
//!
//! 几个刻意的取舍：
//!
//! * **来源地址只认 socket**，绝不看 `X-Forwarded-For`。那个头谁都能写，
//!   拿它做访问控制等于没锁。放在反代后面时，白名单要写反代自己的地址。
//! * **限速数的是失败次数**，成功登录会清零——正常刷新页面不该被挡。
//! * **白名单解析失败会报错**，而不是「当没配」。后者会静默把管理面开给所有
//!   人，而配置文件看起来还像锁着。
//! * **口令比较是常量时间**的。收益或许有限，但没有理由给旁路留计时差。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

// ─────────────────────────────────────────────────────────────────────────────
// 配置
// ─────────────────────────────────────────────────────────────────────────────

/// 访问控制相关的配置项。两个面板各自从自己的配置格式里填这份结构。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    /// `user:password`。`None` 表示不做认证——**只允许**在回环地址上这么干。
    pub auth: Option<String>,
    /// 允许的来源（IPv4 CIDR 或任意族的精确 IP）。空 = 不限。
    pub allow_cidrs: Vec<String>,
    /// 同一来源在窗口内允许的认证失败次数；`0` = 不限制。
    pub max_auth_failures: u32,
    /// 失败计数的窗口长度（秒）。
    pub auth_fail_window_secs: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            auth: None,
            allow_cidrs: Vec::new(),
            max_auth_failures: 8,
            auth_fail_window_secs: 60,
        }
    }
}

impl AuthConfig {
    /// 从 `serde_json::Value` 里读（两个面板的配置都是这样传进来的）。
    ///
    /// 缺失的字段取默认值；`auth` 里没有冒号的字符串会被**当成没配**（而不是
    /// 一个空口令的账号）——反正后面 `validate` 会拦住非回环的无认证启动。
    pub fn from_json(v: &serde_json::Value) -> Self {
        let d = Self::default();
        Self {
            auth: v
                .get("auth")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty() && s.contains(':'))
                .map(str::to_string),
            allow_cidrs: v
                .get("allow_cidrs")
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or(d.allow_cidrs),
            max_auth_failures: v
                .get("max_auth_failures")
                .and_then(serde_json::Value::as_u64)
                .map(|n| n.min(u32::MAX as u64) as u32)
                .unwrap_or(d.max_auth_failures),
            auth_fail_window_secs: v
                .get("auth_fail_window_secs")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(d.auth_fail_window_secs),
        }
    }

    /// 校验配置自洽性。返回 `Err` 会让调用方拒绝启动。
    pub fn validate(&self) -> Result<(), String> {
        for rule in &self.allow_cidrs {
            AllowRule::parse(rule)?;
        }
        if self.auth_fail_window_secs == 0 {
            return Err("auth_fail_window_secs 必须大于 0".to_string());
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 来源白名单
// ─────────────────────────────────────────────────────────────────────────────

/// 一条来源白名单规则。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowRule {
    /// IPv4 网段：`base` 已按 `mask` 归零。
    V4 { base: u32, mask: u32 },
    /// 精确地址。
    Exact(IpAddr),
}

impl AllowRule {
    /// 解析 `1.2.3.4` / `1.2.3.0/24` / `::1`。
    ///
    /// 故意只支持 IPv4 CIDR：IPv6 前缀匹配要考虑的作用域（链路本地、唯一本地、
    /// 内嵌 IPv4……）太多，写个半吊子的实现比不支持更危险。IPv6 请写完整地址
    /// 做精确匹配。
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("allow_cidrs 里有空字符串".to_string());
        }
        if let Some((addr, len)) = text.rsplit_once('/') {
            let addr: Ipv4Addr = addr
                .trim()
                .parse()
                .map_err(|_| format!("allow_cidrs 里的 {text:?} 无法解析（只支持 IPv4 CIDR）"))?;
            let len: u32 = len
                .trim()
                .parse()
                .map_err(|_| format!("allow_cidrs 里的 {text:?} 前缀长度不是数字"))?;
            if len > 32 {
                return Err(format!("allow_cidrs 里的 {text:?} 前缀长度超过 32"));
            }
            let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
            return Ok(Self::V4 {
                base: u32::from(addr) & mask,
                mask,
            });
        }
        let addr: IpAddr = text
            .parse()
            .map_err(|_| format!("allow_cidrs 里的 {text:?} 不是合法 IP"))?;
        Ok(Self::Exact(addr))
    }

    pub fn matches(&self, ip: IpAddr) -> bool {
        match (self, ip) {
            (Self::V4 { base, mask }, IpAddr::V4(v4)) => (u32::from(v4) & mask) == *base,
            (Self::Exact(want), got) => *want == got,
            _ => false,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 认证失败限速
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct FailEntry {
    count: u32,
    since: Instant,
}

/// 每个来源的认证失败计数。
///
/// 固定窗口（从首次失败起算）足够应对扫描器，实现也简单到不用怀疑：
/// 公网上的管理面每天会被试探成百上千次，真正的对手在这里撞不过去；
/// 而多花心思做滑动窗口并不会让安全性提高到值得多一份状态机代码。
#[derive(Debug)]
pub struct AuthGuard {
    max_failures: u32,
    window: Duration,
    state: Mutex<HashMap<IpAddr, FailEntry>>,
}

/// 计数表的上限：公网面板天天被扫，不设上限等于送对面一个内存放大招。
const MAX_TRACKED_SOURCES: usize = 4096;

impl AuthGuard {
    pub fn new(max_failures: u32, window: Duration) -> Self {
        Self {
            max_failures,
            window: window.max(Duration::from_secs(1)),
            state: Mutex::new(HashMap::new()),
        }
    }

    /// 是否已被限速；返回建议的重试等待时间。
    pub fn blocked_for(&self, ip: IpAddr) -> Option<Duration> {
        if self.max_failures == 0 {
            return None;
        }
        let map = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.get(&ip)?;
        let elapsed = entry.since.elapsed();
        if entry.count >= self.max_failures && elapsed < self.window {
            Some(self.window - elapsed)
        } else {
            None
        }
    }

    /// 记一次失败。
    pub fn record_failure(&self, ip: IpAddr) {
        if self.max_failures == 0 {
            return;
        }
        let mut map = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= MAX_TRACKED_SOURCES {
            let window = self.window;
            map.retain(|_, e| e.since.elapsed() < window);
            if map.len() >= MAX_TRACKED_SOURCES {
                map.clear();
            }
        }
        let now = Instant::now();
        let entry = map.entry(ip).or_insert(FailEntry {
            count: 0,
            since: now,
        });
        if entry.since.elapsed() >= self.window {
            *entry = FailEntry { count: 1, since: now };
        } else {
            entry.count = entry.count.saturating_add(1);
        }
    }

    /// 认证成功后清掉该来源的计数——避免正常用户手滑几次后被越攒越久。
    pub fn clear(&self, ip: IpAddr) {
        let mut map = self.state.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&ip);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 三道闸的合成
// ─────────────────────────────────────────────────────────────────────────────

/// 一道请求要过的全部检查。
#[derive(Debug)]
pub struct AccessControl {
    auth: Option<String>,
    allow: Vec<AllowRule>,
    guard: AuthGuard,
}

/// `AccessControl::check` 的拒绝结果。
///
/// 内层是 `Box<Response>`：`Response` 有 128 字节，直接放进 `Err` 会让整个
/// `Result` 变大，而这条路径是**罕见**的（绝大多数请求都走 `Ok`），
/// 为一个冷路径让热路径承担拷贝成本不划算。
#[derive(Debug)]
pub struct Denied(pub Box<Response>);

impl AccessControl {
    pub fn new(cfg: &AuthConfig) -> Self {
        Self {
            auth: cfg.auth.clone(),
            // 注意：这里用 `filter_map(...ok())` 是安全的，因为 `AuthConfig::validate`
            // 已经保证每条规则都能解析（调用方必须在启动前调它）。
            allow: cfg
                .allow_cidrs
                .iter()
                .filter_map(|r| AllowRule::parse(r).ok())
                .collect(),
            guard: AuthGuard::new(
                cfg.max_auth_failures,
                Duration::from_secs(cfg.auth_fail_window_secs.max(1)),
            ),
        }
    }

    /// 白名单是否为空（不限来源）。启动时用它决定要不要打警告。
    pub fn allows_any_source(&self) -> bool {
        self.allow.is_empty()
    }

    pub fn has_auth(&self) -> bool {
        self.auth.is_some()
    }

    /// 过闸。`Ok(())` = 放行。
    ///
    /// `peer` 必须来自 accept 时的 socket（axum 的
    /// `into_make_service_with_connect_info`），不要自己从请求头里推。
    /// 传 `None` 时白名单**视为不放行**（fail-closed）：宁可挡住也不要在拿不到
    /// 来源地址的情况下静默放行。
    pub fn check(&self, headers: &HeaderMap, peer: Option<IpAddr>) -> Result<(), Denied> {
        // ① 来源白名单
        if !self.allow.is_empty() {
            let allowed = peer.map(|ip| self.allow.iter().any(|r| r.matches(ip)));
            match allowed {
                Some(true) => {}
                Some(false) => {
                    let ip = peer.map(|i| i.to_string()).unwrap_or_else(|| "-".into());
                    tracing::warn!(%ip, "面板拒绝访问：来源不在白名单内");
                    return Err(Denied(Box::new(forbidden())));
                }
                None => {
                    tracing::warn!("无法确定来源地址，白名单生效时不予放行");
                    return Err(Denied(Box::new(forbidden())));
                }
            }
        }

        // ② 失败限速
        if let Some(ip) = peer {
            if let Some(wait) = self.guard.blocked_for(ip) {
                let secs = wait.as_secs().max(1);
                tracing::warn!(%ip, retry_after_secs = secs, "认证失败次数过多，已限速");
                return Err(Denied(Box::new(too_many_requests(secs))));
            }
        }

        // ③ Basic 认证
        let Some(expected) = self.auth.as_deref() else {
            return Ok(());
        };
        let supplied = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Basic "))
            .and_then(|b64| {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.decode(b64).ok()
            })
            .and_then(|bytes| String::from_utf8(bytes).ok());

        if supplied
            .as_deref()
            .map(|s| ct_eq(s.as_bytes(), expected.as_bytes()))
            .unwrap_or(false)
        {
            if let Some(ip) = peer {
                self.guard.clear(ip);
            }
            Ok(())
        } else {
            if let Some(ip) = peer {
                self.guard.record_failure(ip);
            }
            Err(Denied(Box::new(unauthorized())))
        }
    }
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "来源地址不在白名单内").into_response()
}

fn too_many_requests(retry_after_secs: u64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, retry_after_secs.to_string())],
        "认证失败次数过多，请稍后再试",
    )
        .into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            "Basic realm=\"esc panel\", charset=\"UTF-8\"",
        )],
        "需要认证",
    )
        .into_response()
}

/// 常量时间字节比较。长度不同直接返回 false（长度本来就不是秘密）。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn basic(creds: &str) -> HeaderMap {
        use base64::Engine as _;
        let mut h = HeaderMap::new();
        let encoded = base64::engine::general_purpose::STANDARD.encode(creds);
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {encoded}")).unwrap(),
        );
        h
    }

    fn ac(auth: Option<&str>, allow: &[&str], max: u32) -> AccessControl {
        let cfg = AuthConfig {
            auth: auth.map(str::to_string),
            allow_cidrs: allow.iter().map(|s| s.to_string()).collect(),
            max_auth_failures: max,
            auth_fail_window_secs: 60,
        };
        cfg.validate().expect("测试配置应当合法");
        AccessControl::new(&cfg)
    }

    // ── AllowRule ───────────────────────────────────────────────────────────

    #[test]
    fn cidr_matches_only_inside_the_range() {
        let rule = AllowRule::parse("10.1.0.0/16").unwrap();
        assert!(rule.matches(ip("10.1.0.1")));
        assert!(rule.matches(ip("10.1.255.254")));
        assert!(!rule.matches(ip("10.2.0.1")), "相邻网段不该命中");
        assert!(!rule.matches(ip("11.1.0.1")));
    }

    #[test]
    fn host_bits_are_normalised_away() {
        let rule = AllowRule::parse("1.2.3.4/24").unwrap();
        assert!(rule.matches(ip("1.2.3.99")));
        assert!(!rule.matches(ip("1.2.4.1")));
    }

    #[test]
    fn slash_zero_matches_everything_and_slash_32_matches_one() {
        assert!(AllowRule::parse("0.0.0.0/0").unwrap().matches(ip("8.8.8.8")));
        let one = AllowRule::parse("203.0.113.7/32").unwrap();
        assert!(one.matches(ip("203.0.113.7")));
        assert!(!one.matches(ip("203.0.113.8")));
    }

    #[test]
    fn plain_address_is_exact_match_including_ipv6() {
        let v4 = AllowRule::parse("203.0.113.7").unwrap();
        assert!(v4.matches(ip("203.0.113.7")));
        assert!(!v4.matches(ip("203.0.113.8")));

        let v6 = AllowRule::parse("2001:db8::1").unwrap();
        assert!(v6.matches(ip("2001:db8::1")));
        assert!(!v6.matches(ip("2001:db8::2")));
        assert!(!v6.matches(ip("203.0.113.7")), "跨族规则应不匹配而非报错");
    }

    #[test]
    fn malformed_rules_are_rejected_loudly() {
        assert!(AllowRule::parse("10.0.0.0/33").is_err());
        assert!(AllowRule::parse("10.0.0.0/x").is_err());
        assert!(AllowRule::parse("not-an-ip").is_err());
        assert!(
            AllowRule::parse("2001:db8::/32").is_err(),
            "IPv6 前缀暂不支持，应报错而非误判"
        );
        assert!(AllowRule::parse("").is_err());
    }

    #[test]
    fn config_validation_rejects_bad_cidrs_and_zero_window() {
        let mut cfg = AuthConfig {
            auth: Some("admin:pw".into()),
            ..Default::default()
        };
        cfg.allow_cidrs = vec!["10.0.0.0/33".into()];
        assert!(cfg.validate().is_err());

        cfg.allow_cidrs = vec!["10.0.0.0/8".into(), "::1".into()];
        assert!(cfg.validate().is_ok());

        cfg.auth_fail_window_secs = 0;
        assert!(cfg.validate().is_err());
    }

    // ── AccessControl ───────────────────────────────────────────────────────

    #[test]
    fn no_auth_configured_lets_everything_through() {
        let c = ac(None, &[], 8);
        assert!(c.check(&HeaderMap::new(), Some(ip("1.2.3.4"))).is_ok());
    }

    #[test]
    fn missing_or_wrong_credentials_are_401() {
        let c = ac(Some("admin:s3cret"), &[], 8);
        assert_eq!(
            c.check(&HeaderMap::new(), Some(ip("1.2.3.4"))).unwrap_err().0.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            c.check(&basic("admin:nope"), Some(ip("1.2.3.4"))).unwrap_err().0.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            c.check(&basic("other:s3cret"), Some(ip("1.2.3.4"))).unwrap_err().0.status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(c.check(&basic("admin:s3cret"), Some(ip("1.2.3.4"))).is_ok());
    }

    #[test]
    fn allow_list_blocks_unknown_sources_with_403() {
        let c = ac(Some("admin:s3cret"), &["10.0.0.0/8"], 8);
        assert_eq!(
            c.check(&basic("admin:s3cret"), Some(ip("192.168.1.1")))
                .unwrap_err()
                .0
                .status(),
            StatusCode::FORBIDDEN
        );
        assert!(c.check(&basic("admin:s3cret"), Some(ip("10.1.2.3"))).is_ok());
    }

    #[test]
    fn allow_list_is_fail_closed_when_peer_is_unknown() {
        // 拿不到来源地址时不能"当作放行"——否则只要能让 peer 缺失就绕过了白名单。
        let c = ac(Some("admin:s3cret"), &["10.0.0.0/8"], 8);
        assert_eq!(
            c.check(&basic("admin:s3cret"), None).unwrap_err().0.status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn failures_are_capped_and_correct_password_is_blocked_afterwards() {
        let c = ac(Some("admin:s3cret"), &[], 3);
        let who = ip("1.2.3.4");
        for _ in 0..3 {
            assert_eq!(
                c.check(&basic("admin:wrong"), Some(who)).unwrap_err().0.status(),
                StatusCode::UNAUTHORIZED
            );
        }
        // 预算用完：即使口令正确也 429 —— 否则限速可以被"继续试探"绕过
        assert_eq!(
            c.check(&basic("admin:s3cret"), Some(who)).unwrap_err().0.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        // 另一个来源不受影响
        assert!(c.check(&basic("admin:s3cret"), Some(ip("5.6.7.8"))).is_ok());
    }

    #[test]
    fn successful_auth_resets_the_counter() {
        let c = ac(Some("admin:s3cret"), &[], 2);
        let who = ip("1.2.3.4");
        let _ = c.check(&basic("admin:no"), Some(who));
        assert!(c.check(&basic("admin:s3cret"), Some(who)).is_ok());
        // 计数已清零，这一次失败不该直接触发限速
        assert_eq!(
            c.check(&basic("admin:no"), Some(who)).unwrap_err().0.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn rate_limit_expires_after_the_window() {
        let cfg = AuthConfig {
            auth: Some("admin:s3cret".into()),
            max_auth_failures: 1,
            auth_fail_window_secs: 1,
            ..Default::default()
        };
        let c = AccessControl::new(&cfg);
        let who = ip("1.2.3.4");
        let _ = c.check(&basic("admin:no"), Some(who));
        assert_eq!(
            c.check(&basic("admin:s3cret"), Some(who)).unwrap_err().0.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        std::thread::sleep(Duration::from_millis(1100));
        assert!(c.check(&basic("admin:s3cret"), Some(who)).is_ok(), "窗口过期后应恢复");
    }

    #[test]
    fn retry_after_header_is_present_on_429() {
        let c = ac(Some("admin:s3cret"), &[], 1);
        let who = ip("1.2.3.4");
        let _ = c.check(&basic("admin:no"), Some(who));
        let denied = c.check(&basic("admin:no"), Some(who)).unwrap_err();
        assert!(denied.0.headers().contains_key(header::RETRY_AFTER));
    }

    #[test]
    fn constant_time_compare_is_still_correct() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn tracker_table_stays_bounded_under_a_flood() {
        let guard = AuthGuard::new(1, Duration::from_secs(60));
        for i in 0..(MAX_TRACKED_SOURCES + 500) {
            guard.record_failure(IpAddr::V4(Ipv4Addr::from((i as u32) | 0x0a00_0000)));
        }
        let len = guard.state.lock().unwrap().len();
        assert!(len <= MAX_TRACKED_SOURCES, "计数表应有上限，实际 {len}");
    }
}
