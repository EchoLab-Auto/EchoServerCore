//! 会话认证：登录、cookie、会话表、限速。
//!
//! 这里的设计选择都偏向「少一个可被滥用的面」：
//!
//! * **会话 id 存在内存里**，进程重启即全部失效。换成签名 cookie 虽然能跨重启
//!   保留登录态，但那就多了一个「签名密钥泄露 = 永久后门」的风险点，而运维
//!   重启一次网关重登一次完全不是负担。
//! * **cookie 用 `SameSite=Strict`**，于是 CSRF 基本不可能发生；再加一道
//!   `Origin`/`Host` 一致性校验作为纵深防御（成本几乎为零）。
//! * **登录成功后重新生成 session id**，防会话固定。
//! * **口令用 argon2id 哈希比对**（见 `config::verify_password`）。
//! * **两层限速**：按来源 + 全局。只有按来源时，对方有 1000 个 IP 就能试 8000 次。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use rand::RngCore;

use crate::config::{RateLimitConfig, RuntimeConfig, UserRuntime};

/// 一次已登录的会话。
#[derive(Debug, Clone)]
pub struct Session {
    pub user: String,
    pub created: Instant,
    pub last_seen: Instant,
    /// 该用户在登录那一刻的授权范围。
    ///
    /// 刻意在登录时就固化：如果管理员在会话期间改了配置并重启，会话已经失效
    /// （在内存里），所以不存在「权限变更后旧会话仍在用旧权限」的窗口。
    ///
    /// 管理面板改账号时是**另一条路**：那时候进程不重启，所以由面板显式调用
    /// [`AuthState::revoke_user`] 把该账号的会话全部作废。不这么做的话，
    /// 「删掉一个被盗账号」只会在 TTL 到期后才真正生效。
    pub routes: Vec<String>,
    pub all_routes: bool,
    /// 是否可进管理面板（登录时固化，同上由 revoke_user 兜底）。
    pub admin: bool,
    /// 登录来源 IP，供管理面板显示与排查。
    pub source: Option<IpAddr>,
    /// 一次性提示（成功/失败横幅），`bool` = 是否成功。
    ///
    /// 放在会话里而不是 URL 查询串上：查询串是**别人能构造**的，于是
    /// `/__admin/?err=你的账号已被停用，请访问 …` 就能在我们自己的管理面板里
    /// 显示一条看似权威的红条。转义能挡住 XSS，挡不住这种钓鱼。
    pub flash: Option<(bool, String)>,
}

impl Session {
    pub fn can_access(&self, route: &str) -> bool {
        self.all_routes || self.routes.iter().any(|r| r == route)
    }
}

/// 会话表 + 两层登录失败限速。
#[derive(Debug)]
pub struct AuthState {
    inner: Mutex<Inner>,
    ttl: Duration,
    max_sessions: usize,
    max_failures: u32,
    fail_window: Duration,
    max_failures_global: u32,
    global_window: Duration,
}

#[derive(Debug, Default)]
struct Inner {
    sessions: HashMap<String, Session>,
    /// 按来源的失败计数。
    fails: HashMap<IpAddr, (u32, Instant)>,
    /// 全局失败计数（跨来源）。用 `Option<Instant>` 表示"从什么时候开始算"。
    global: Option<(u32, Instant)>,
    /// 单调递增的计数器，用于 LRU 淘汰时挑「最久未使用」。
    tick: u64,
    session_tick: HashMap<String, u64>,
}

/// 一次认证判定的结果。
#[derive(Debug)]
pub enum Denied {
    /// 没登录 / 会话过期。浏览器该跳登录页，API 该拿 401。
    Unauthenticated,
    /// 登录了，但这个用户没有该 route 的权限。
    Forbidden,
    /// 失败太多次。
    RateLimited { retry_after_secs: u64 },
}

impl Denied {
    fn status(&self) -> StatusCode {
        match self {
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
        }
    }
}

impl AuthState {
    pub fn new(cfg: &RuntimeConfig) -> Self {
        let rl: &RateLimitConfig = &cfg.rate_limit;
        Self {
            inner: Mutex::new(Inner::default()),
            ttl: cfg.session_ttl,
            max_sessions: cfg.max_sessions.max(1),
            max_failures: rl.max_failures,
            fail_window: Duration::from_secs(rl.fail_window_secs.max(1)),
            max_failures_global: rl.max_failures_global,
            global_window: Duration::from_secs(rl.global_window_secs.max(1)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 用 session id 查会话，顺便刷新 LRU 与过期清理。
    pub fn get(&self, sid: &str) -> Option<Session> {
        let now = Instant::now();
        let mut inner = self.lock();
        // 顺带清过期：每次访问时做一点，不需要后台任务。
        inner
            .sessions
            .retain(|_, s| now.duration_since(s.created) < self.ttl);
        let tick = {
            inner.tick += 1;
            inner.tick
        };
        let found = inner.sessions.get_mut(sid).map(|s| {
            s.last_seen = now;
            s.clone()
        });
        if found.is_some() {
            inner.session_tick.insert(sid.to_string(), tick);
        }
        found
    }

    /// 登录成功后建立会话，返回 session id。
    pub fn create(&self, user: &UserRuntime, source: Option<IpAddr>) -> String {
        let mut rng = rand::thread_rng();
        // 32 字节熵。Base64URL 无填充，放进 cookie 不用转义。
        let mut raw = [0u8; 32];
        rng.fill_bytes(&mut raw);
        let sid = b64url(&raw);

        let now = Instant::now();
        let session = Session {
            user: user.name.clone(),
            created: now,
            last_seen: now,
            routes: user.routes.clone(),
            all_routes: user.all_routes,
            admin: user.admin,
            source,
            flash: None,
        };

        let mut inner = self.lock();
        inner
            .sessions
            .retain(|_, s| now.duration_since(s.created) < self.ttl);
        // 达到上限时淘汰最久未使用的那个。不淘汰就等于给对面一个内存放大招：
        // 反复登录就能把会话表堆满。
        if inner.sessions.len() >= self.max_sessions {
            if let Some(victim) = inner
                .sessions
                .keys()
                .min_by_key(|k| inner.session_tick.get(*k).copied().unwrap_or(0))
                .cloned()
            {
                inner.sessions.remove(&victim);
                inner.session_tick.remove(&victim);
            }
        }
        inner.tick += 1;
        let tick = inner.tick;
        inner.sessions.insert(sid.clone(), session);
        inner.session_tick.insert(sid.clone(), tick);
        sid
    }

    pub fn logout(&self, sid: &str) {
        self.lock().sessions.remove(sid);
    }

    /// 作废某账号的全部会话，`keep` 指定的那一个除外。
    ///
    /// 管理面板改口令 / 改授权 / 删账号时必须调它。理由很直接：会话里的权限
    /// 是登录时快照的，不主动清就等于「删了账号，人还能用 8 小时」。
    ///
    /// `keep` 留给「管理员改自己的口令」——把自己也踢掉太蠢了，而真正要防的
    /// 是**别人**拿着旧 cookie，那正是被清掉的那些。
    pub fn revoke_user(&self, user: &str, keep: Option<&str>) -> usize {
        let mut inner = self.lock();
        let victims: Vec<String> = inner
            .sessions
            .iter()
            .filter(|(sid, s)| s.user == user && Some(sid.as_str()) != keep)
            .map(|(sid, _)| sid.clone())
            .collect();
        for sid in &victims {
            inner.sessions.remove(sid);
            inner.session_tick.remove(sid);
        }
        victims.len()
    }

    /// 给会话挂一条一次性提示。
    pub fn set_flash(&self, sid: &str, ok: bool, msg: impl Into<String>) {
        let mut inner = self.lock();
        if let Some(s) = inner.sessions.get_mut(sid) {
            s.flash = Some((ok, msg.into()));
        }
    }

    /// 取走一次性提示（取完即清，刷新页面不会重复出现）。
    pub fn take_flash(&self, sid: &str) -> Option<(bool, String)> {
        let mut inner = self.lock();
        inner.sessions.get_mut(sid).and_then(|s| s.flash.take())
    }

    /// 全部在线会话，按建立时间从新到旧。管理面板用。
    pub fn list(&self) -> Vec<(String, Session)> {
        let now = Instant::now();
        let mut inner = self.lock();
        inner
            .sessions
            .retain(|_, s| now.duration_since(s.created) < self.ttl);
        let mut v: Vec<(String, Session)> = inner
            .sessions
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        v.sort_by_key(|(_, s)| std::cmp::Reverse(s.created));
        v
    }

    pub fn session_count(&self) -> usize {
        self.lock().sessions.len()
    }

    // ── 失败限速 ──────────────────────────────────────────────────────────

    /// 这个来源现在是否被限速。
    pub fn blocked_for(&self, ip: Option<IpAddr>) -> Option<Duration> {
        let now = Instant::now();
        let inner = self.lock();

        // 全局闸优先：攻击者换 IP 时按来源的计数各归各的，只有全局能挡住。
        if self.max_failures_global > 0 {
            if let Some((n, since)) = inner.global {
                let el = now.duration_since(since);
                if n >= self.max_failures_global && el < self.global_window {
                    return Some(self.global_window - el);
                }
            }
        }
        let ip = ip?;
        let (n, since) = *inner.fails.get(&ip)?;
        let el = now.duration_since(since);
        if n >= self.max_failures && el < self.fail_window {
            Some(self.fail_window - el)
        } else {
            None
        }
    }

    /// 记一次登录失败。
    pub fn record_failure(&self, ip: Option<IpAddr>) {
        let now = Instant::now();
        let mut inner = self.lock();

        if self.max_failures_global > 0 {
            match inner.global {
                Some((n, since)) if now.duration_since(since) < self.global_window => {
                    inner.global = Some((n.saturating_add(1), since));
                }
                _ => inner.global = Some((1, now)),
            }
        }
        if let Some(ip) = ip {
            // 有界：被大规模扫描时不能无限增长
            if inner.fails.len() >= 4096 {
                let window = self.fail_window;
                inner
                    .fails
                    .retain(|_, (_, since)| now.duration_since(*since) < window);
                if inner.fails.len() >= 4096 {
                    inner.fails.clear();
                }
            }
            let prev = inner.fails.get(&ip).copied();
            match prev {
                Some((n, since)) if now.duration_since(since) < self.fail_window => {
                    inner.fails.insert(ip, (n.saturating_add(1), since));
                }
                _ => {
                    inner.fails.insert(ip, (1, now));
                }
            }
        }
    }

    /// 登录成功后清掉该来源的失败计数。
    ///
    /// **不清全局计数**：全局计数代表「整体上有人在爆破」，一次成功登录
    /// 不该把它归零，否则攻击者只要有一个自己的账号就能反复解锁。
    pub fn clear_source(&self, ip: Option<IpAddr>) {
        let mut inner = self.lock();
        if let Some(ip) = ip {
            inner.fails.remove(&ip);
        }
    }
}

fn b64url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ─────────────────────────────────────────────────────────────────────────────
// cookie
// ─────────────────────────────────────────────────────────────────────────────

/// 从 `Cookie` 头里取指定名字的值。
pub fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    // 手写解析而不是用 cookie crate 的 `Cookie::parse`：后者一次只吃一个
    // name=value 对，还是要自己按分号切。这里只需要「找某个键」。
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((k.trim(), v.trim()))
        })
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// 构造 `Set-Cookie`。
///
/// `SameSite=Strict` 是关键：它让浏览器在任何跨站请求里都不带这个 cookie，
/// 于是 CSRF 从根上不成立（这也是为什么我们没有 CSRF token 却仍然安全）。
pub fn set_cookie(name: &str, value: &str, max_age: Duration, secure: bool) -> HeaderValue {
    let mut s = format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        max_age.as_secs()
    );
    if secure {
        s.push_str("; Secure");
    }
    HeaderValue::from_str(&s).unwrap_or_else(|_| HeaderValue::from_static("esc_gw=; Max-Age=0"))
}

/// 清掉 cookie（登出）。
pub fn clear_cookie(name: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("esc_gw=; Max-Age=0"))
}

// ─────────────────────────────────────────────────────────────────────────────
// 判断「浏览器要跳转」还是「API 要 401」
// ─────────────────────────────────────────────────────────────────────────────

/// 这个请求是不是「浏览器在导航」。
///
/// 这个判断错一次就会让人用着难受：把 fetch 也 302 到登录页，前端的
/// `JSON.parse` 会拿到一坨 HTML 然后报一个完全无关的解析错误。
///
/// 判定规则（按可靠度排序）：
///
/// 1. `Sec-Fetch-Mode: navigate` —— 现代浏览器对导航请求一定会带，最可靠
/// 2. `Accept` 明确要 HTML 且不要 JSON
/// 3. 否则一律当 API
pub fn wants_html(headers: &HeaderMap) -> bool {
    if let Some(mode) = headers.get("sec-fetch-mode").and_then(|v| v.to_str().ok()) {
        return mode.eq_ignore_ascii_case("navigate");
    }
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    accept.contains("text/html") && !accept.contains("application/json")
}

/// `Origin`（或没有 Origin 时的 `Referer` 主机）是否与 `Host` 一致。
///
/// `SameSite=Strict` 已经让 CSRF 基本不可能，这条是纵深防御：万一将来有人
/// 为了嵌入 iframe 把 SameSite 改成 Lax/None，这层还在。
pub fn same_origin(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        // HTTP/1.0 或某些客户端不带 Host。这种情况下没法判断，
        // 放行（要求 Host 会让某些正常客户端不可用），安全性由 SameSite 兜。
        return true;
    };
    let host = host.trim();

    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty());
    let Some(origin) = origin else {
        // 同源 GET 常常没有 Origin 头；非 GET 时浏览器一定会带。
        // 没有 Origin 也不代表跨站（curl 就没有），放行。
        return true;
    };
    if origin == "null" {
        return false;
    }
    // origin 形如 scheme://host[:port]
    let Some(rest) = origin.split_once("://").map(|(_, r)| r) else {
        return false;
    };
    let origin_host = rest.split('/').next().unwrap_or(rest);
    origin_host.eq_ignore_ascii_case(host)
}

impl Denied {
    pub fn status_code(&self) -> StatusCode {
        self.status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RateLimitConfig;

    fn runtime() -> RuntimeConfig {
        RuntimeConfig {
            title: "t".into(),
            cookie_name: "esc_gw".into(),
            session_ttl: Duration::from_secs(3600),
            max_sessions: 4,
            trust_forwarded_for: false,
            secure_cookie: false,
            rate_limit: RateLimitConfig::default(),
            users: HashMap::new(),
            routes: HashMap::new(),
            listens: vec![],
            users_file: None,
            admin_allow: vec![],
        }
    }

    fn user(routes: &[&str]) -> UserRuntime {
        UserRuntime {
            name: "u".into(),
            hash: "$argon2id$x".into(),
            all_routes: routes.contains(&"*"),
            routes: routes
                .iter()
                .filter(|r| **r != "*")
                .map(|r| r.to_string())
                .collect(),
            admin: false,
        }
    }

    #[test]
    fn session_round_trips_and_logout_invalidates() {
        let st = AuthState::new(&runtime());
        let sid = st.create(&user(&["*"]), None);
        let s = st.get(&sid).expect("刚建的会话应能查到");
        assert_eq!(s.user, "u");
        assert!(s.can_access("any"));
        st.logout(&sid);
        assert!(st.get(&sid).is_none(), "登出后必须查不到");
    }

    #[test]
    fn session_ids_are_unguessable_and_unique() {
        let st = AuthState::new(&runtime());
        let a = st.create(&user(&["*"]), None);
        let b = st.create(&user(&["*"]), None);
        assert_ne!(a, b);
        // 32 字节 base64url 无填充 = 43 个字符
        assert_eq!(a.len(), 43, "session id 长度: {}", a.len());
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn sessions_are_capped_and_evict_least_recently_used() {
        let st = AuthState::new(&runtime()); // max_sessions = 4
        let ids: Vec<String> = (0..4).map(|_| st.create(&user(&["*"]), None)).collect();
        assert_eq!(st.session_count(), 4);
        // 碰一下第 1 个，让第 0 个成为最久未使用
        assert!(st.get(&ids[1]).is_some());

        let new = st.create(&user(&["*"]), None);
        assert_eq!(st.session_count(), 4, "不能超过上限");
        assert!(st.get(&new).is_some());
        assert!(st.get(&ids[0]).is_none(), "最久未使用的应被淘汰");
        assert!(st.get(&ids[1]).is_some(), "刚访问过的不该被淘汰");
    }

    #[test]
    fn expired_sessions_are_dropped() {
        let mut rt = runtime();
        rt.session_ttl = Duration::from_millis(30);
        let st = AuthState::new(&rt);
        let sid = st.create(&user(&["*"]), None);
        assert!(st.get(&sid).is_some());
        std::thread::sleep(Duration::from_millis(60));
        assert!(st.get(&sid).is_none(), "过期会话应查不到");
    }

    #[test]
    fn per_user_acl_is_carried_into_the_session() {
        let st = AuthState::new(&runtime());
        let sid = st.create(&user(&["p1"]), None);
        let s = st.get(&sid).unwrap();
        assert!(s.can_access("p1"));
        assert!(!s.can_access("p2"));
    }

    #[test]
    fn source_failures_block_after_the_budget() {
        let st = AuthState::new(&runtime()); // 8 次 / 60s
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        for _ in 0..8 {
            assert!(st.blocked_for(Some(ip)).is_none());
            st.record_failure(Some(ip));
        }
        assert!(st.blocked_for(Some(ip)).is_some(), "8 次后应被挡");
        // 其它来源不受影响
        assert!(st.blocked_for(Some("5.6.7.8".parse().unwrap())).is_none());
        // 成功后清源计数
        st.clear_source(Some(ip));
        assert!(st.blocked_for(Some(ip)).is_none());
    }

    #[test]
    fn global_limit_stops_distributed_guessing() {
        let mut rt = runtime();
        rt.rate_limit.max_failures_global = 5;
        rt.rate_limit.max_failures = 100; // 单 IP 很宽松，只有全局闸能起作用
        let st = AuthState::new(&rt);
        // 每次都换一个 IP，模拟分布式爆破
        for i in 0..5u32 {
            let ip: IpAddr = format!("10.0.0.{i}").parse().unwrap();
            assert!(st.blocked_for(Some(ip)).is_none(), "第 {i} 次不该被挡");
            st.record_failure(Some(ip));
        }
        let fresh: IpAddr = "10.9.9.9".parse().unwrap();
        assert!(
            st.blocked_for(Some(fresh)).is_some(),
            "换新 IP 也必须被全局闸挡住"
        );
    }

    #[test]
    fn successful_login_does_not_reset_the_global_counter() {
        // 否则攻击者只要自己有个账号，就能反复解锁全局闸
        let mut rt = runtime();
        rt.rate_limit.max_failures_global = 2;
        let st = AuthState::new(&rt);
        let ip: IpAddr = "1.1.1.1".parse().unwrap();
        st.record_failure(Some(ip));
        st.record_failure(Some(ip));
        st.clear_source(Some(ip)); // 清源计数（成功登录时会调）
        assert!(
            st.blocked_for(Some("2.2.2.2".parse().unwrap())).is_some(),
            "全局计数不应被成功登录清零"
        );
    }

    #[test]
    fn failure_table_stays_bounded() {
        let st = AuthState::new(&runtime());
        for i in 0..6000u32 {
            let ip: IpAddr = format!("172.16.{}.{}", i / 256, i % 256).parse().unwrap();
            st.record_failure(Some(ip));
        }
        let len = st.lock().fails.len();
        assert!(len <= 4096, "失败计数表应有上限，实际 {len}");
    }

    // ── cookie / 判定 ────────────────────────────────────────────────────

    #[test]
    fn cookie_is_parsed_out_of_a_multi_pair_header() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; esc_gw=abc-123; b=2"),
        );
        assert_eq!(cookie_value(&h, "esc_gw"), Some("abc-123"));
        assert_eq!(cookie_value(&h, "a"), Some("1"));
        assert_eq!(cookie_value(&h, "nope"), None);
    }

    #[test]
    fn cookie_is_parsed_when_split_across_two_headers() {
        let mut h = HeaderMap::new();
        h.append(header::COOKIE, HeaderValue::from_static("a=1"));
        h.append(header::COOKIE, HeaderValue::from_static("esc_gw=xyz"));
        assert_eq!(cookie_value(&h, "esc_gw"), Some("xyz"));
    }

    #[test]
    fn set_cookie_has_the_security_flags() {
        let v = set_cookie("esc_gw", "abc", Duration::from_secs(60), false);
        let s = v.to_str().unwrap();
        assert!(s.contains("HttpOnly"), "{s}");
        assert!(s.contains("SameSite=Strict"), "{s}");
        assert!(s.contains("Path=/"), "{s}");
        assert!(s.contains("Max-Age=60"), "{s}");
        assert!(
            !s.contains("Secure"),
            "没开 TLS 时不该加 Secure（否则浏览器不存）"
        );

        let v = set_cookie("esc_gw", "abc", Duration::from_secs(60), true);
        assert!(v.to_str().unwrap().contains("; Secure"));
    }

    #[test]
    fn logout_cookie_expires_immediately() {
        let s = clear_cookie("esc_gw").to_str().unwrap().to_string();
        assert!(s.contains("Max-Age=0"), "{s}");
    }

    #[test]
    fn navigation_vs_api_is_distinguished() {
        // 浏览器导航
        let mut h = HeaderMap::new();
        h.insert("sec-fetch-mode", HeaderValue::from_static("navigate"));
        assert!(wants_html(&h));

        // 现代浏览器的 fetch：Sec-Fetch-Mode 会明确说是 cors/same-origin
        h.insert("sec-fetch-mode", HeaderValue::from_static("cors"));
        h.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
        assert!(
            !wants_html(&h),
            "面板的 fetch 不带 Accept: json，必须按 API 处理"
        );

        // 老浏览器没有 Sec-Fetch-Mode，只能看 Accept
        let mut h = HeaderMap::new();
        h.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/html,application/xhtml+xml"),
        );
        assert!(wants_html(&h));

        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        assert!(!wants_html(&h));

        // 什么都没有 —— 当 API（curl 默认行为）
        assert!(!wants_html(&HeaderMap::new()));
    }

    #[test]
    fn cross_origin_post_is_detected() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("example.com:2223"));
        h.insert(header::ORIGIN, HeaderValue::from_static("https://evil.com"));
        assert!(!same_origin(&h));

        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("example.com:2223"));
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://example.com:2223"),
        );
        assert!(same_origin(&h), "同源应当放行");

        // 没有 Origin（curl / 同源 GET）→ 放行，安全性由 SameSite 兜
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("example.com"));
        assert!(same_origin(&h));

        // Origin: null（sandbox iframe / file://）→ 拒绝
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("example.com"));
        h.insert(header::ORIGIN, HeaderValue::from_static("null"));
        assert!(!same_origin(&h));
    }
}
