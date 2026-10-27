//! 路由（代理目标）的**运行时**管理与热应用。
//!
//! # 和账号库一个套路
//!
//! | | 位置 | 面板能改吗 |
//! |---|---|---|
//! | `[[route]]` | `gateway.toml` | 不能（只读，要改先「接管」） |
//! | 路由库 | `routes_file`，默认 `gateway-routes.toml` | 能 |
//!
//! 同名以路由库为准。配置文件因此永远是救援通道：路由库删了、写坏了、卷没挂
//! 上，`gateway.toml` 里的端口照样有人听——包括管理面板自己所在的那个端口。
//! 这一点比账号更关键：账号库坏了最坏是登不进去（配置文件的账号还能救），
//! 路由库坏了如果连带端口不监听，那就连"救"的入口都没了。
//!
//! # 什么时候生效
//!
//! 一直是"改完立刻生效"，但分两层，因为成本完全不同：
//!
//! | 改的东西 | 生效方式 |
//! |---|---|
//! | `upstream`、`strip_prefix`、`allowed_cidrs`、`max_body_bytes` | **下一个请求**就生效（每请求从注册表读一次） |
//! | `listen`（含新增 / 删除端口） | 现绑 / 现拆监听套接字，几毫秒 |
//!
//! 早先的设计是"路由必须重启才生效"，理由是「认证拓扑要能在重启这个动作里被
//! 完整替换」。这个理由对**审计**成立，对**可用性**不成立：改一个上游地址要
//! 停 10 秒网关，等于所有面板一起断。所以现在改成两阶段热应用——**要么全成，
//! 要么全不动**（见 [`ListenerSet::prepare`]）：先绑新端口，绑不上就整体放弃并
//! 报错；只有全部绑成功了才写盘、换注册表、停旧端口。中间不出现"一半新一半旧"
//! 的授权状态。
//!
//! 代价是失效会话不会因此清空：路由改名后老会话的 ACL 里还是老名字，于是它
//! 拿不到新端口的权限。这是**对的**——改名就该重新授权。

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::config::{RouteConfig, RouteRuntime};
use crate::store::{Audit, AUDIT_KEEP};

/// 路由是从哪来的。和账号一样，面板上必须标出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteSource {
    Config,
    Store,
    StoreOverConfig,
}

impl RouteSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Config => "配置文件（只读）",
            Self::Store => "路由库",
            Self::StoreOverConfig => "路由库（覆盖了配置文件）",
        }
    }
    pub fn editable(self) -> bool {
        !matches!(self, Self::Config)
    }
}

/// 一条生效的路由。
#[derive(Debug, Clone)]
pub struct EffectiveRoute {
    pub spec: RouteConfig,
    pub source: RouteSource,
}

// ─────────────────────────────────────────────────────────────────────────────
// 路由库（磁盘）
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreFile {
    #[serde(default)]
    route: Vec<RouteConfig>,
}

#[derive(Debug)]
struct State {
    routes: BTreeMap<String, RouteConfig>,
    writable: bool,
    note: Option<String>,
    rejected: Vec<String>,
    recent: VecDeque<Audit>,
}

/// 面板管理的路由集合。
#[derive(Debug)]
pub struct RouteStore {
    path: Option<PathBuf>,
    state: Mutex<State>,
}

impl RouteStore {
    /// 载入。和账号库一样：任何问题都**不阻塞启动**，只记进 `note`/`rejected`。
    pub fn load(path: Option<&Path>) -> Self {
        let mut st = State {
            routes: BTreeMap::new(),
            writable: false,
            note: None,
            rejected: Vec::new(),
            recent: VecDeque::new(),
        };

        let Some(path) = path else {
            st.note =
                Some("未配置 routes_file，端口只能在 gateway.toml 里改（面板为只读）。".into());
            return Self {
                path: None,
                state: Mutex::new(st),
            };
        };

        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(raw) => match toml::from_str::<StoreFile>(&raw) {
                    Ok(file) => {
                        for r in file.route {
                            match r.to_runtime() {
                                Ok(_) => {
                                    if st.routes.insert(r.name.clone(), r.clone()).is_some() {
                                        st.rejected
                                            .push(format!("{}: 同名路由重复，后一条生效", r.name));
                                    }
                                }
                                Err(e) => st.rejected.push(e),
                            }
                        }
                    }
                    Err(e) => {
                        st.note = Some(format!(
                            "路由库 {} 解析失败（{e}）——已忽略，当前只用 gateway.toml 里的端口",
                            path.display()
                        ));
                    }
                },
                Err(e) => {
                    st.note = Some(format!(
                        "路由库 {} 读取失败（{e}）——当前只用 gateway.toml 里的端口",
                        path.display()
                    ));
                }
            }
        }

        match probe_writable(path) {
            Ok(()) => st.writable = true,
            Err(e) => {
                if st.note.is_none() {
                    st.note = Some(format!("路由库不可写：{e}"));
                }
            }
        }

        Self {
            path: Some(path.to_path_buf()),
            state: Mutex::new(st),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn writable(&self) -> bool {
        self.lock().writable
    }
    pub fn note(&self) -> Option<String> {
        self.lock().note.clone()
    }
    pub fn rejected(&self) -> Vec<String> {
        self.lock().rejected.clone()
    }
    pub fn count(&self) -> usize {
        self.lock().routes.len()
    }
    pub fn get(&self, name: &str) -> Option<RouteConfig> {
        self.lock().routes.get(name).cloned()
    }
    pub fn all(&self) -> Vec<RouteConfig> {
        self.lock().routes.values().cloned().collect()
    }
    pub fn recent(&self) -> Vec<Audit> {
        self.lock().recent.iter().cloned().collect()
    }

    /// 新建或替换一条路由（**只改内存候选**，写盘由 [`Self::commit`] 负责）。
    ///
    /// 这里刻意不判断"能不能绑上"——那是 [`ListenerSet::prepare`] 的事。
    /// 两阶段的意义就是把"能不能"和"写不写"分开。
    pub fn stage(
        &self,
        spec: RouteConfig,
        creating: bool,
    ) -> Result<(BTreeMap<String, RouteConfig>, String), String> {
        let st = self.lock();
        if !st.writable {
            return Err(format!(
                "路由库是只读的，改动无法保存{}",
                st.note
                    .as_ref()
                    .map(|n| format!("（{n}）"))
                    .unwrap_or_default()
            ));
        }
        if creating && st.routes.contains_key(&spec.name) {
            return Err(format!("路由 {:?} 在路由库里已存在", spec.name));
        }
        if !creating && !st.routes.contains_key(&spec.name) {
            return Err(format!(
                "路由 {:?} 不在路由库里（可能来自 gateway.toml，需要先「接管」）",
                spec.name
            ));
        }
        let mut pending = st.routes.clone();
        pending.insert(spec.name.clone(), spec.clone());

        let verb = if creating {
            "新建端口"
        } else {
            "修改端口"
        };
        // 记账推迟到 commit：写盘失败时不该留下一条"已修改"的假记录。
        Ok((pending, format!("{verb} {}", spec.name)))
    }

    /// 删掉一条（内存候选）。
    pub fn stage_delete(
        &self,
        name: &str,
    ) -> Result<(BTreeMap<String, RouteConfig>, String), String> {
        let st = self.lock();
        if !st.writable {
            return Err(format!(
                "路由库是只读的，改动无法保存{}",
                st.note
                    .as_ref()
                    .map(|n| format!("（{n}）"))
                    .unwrap_or_default()
            ));
        }
        let mut pending = st.routes.clone();
        if pending.remove(name).is_none() {
            return Err(format!(
                "路由 {name:?} 不在路由库里（如果它来自 gateway.toml，请改配置文件）"
            ));
        }
        Ok((pending, format!("删除端口 {name}")))
    }

    /// 把 `gateway.toml` 里的路由接管进路由库（内存候选）。
    pub fn stage_adopt(
        &self,
        spec: &RouteConfig,
    ) -> Result<(BTreeMap<String, RouteConfig>, String), String> {
        let st = self.lock();
        if !st.writable {
            return Err("路由库是只读的，无法接管".into());
        }
        if st.routes.contains_key(&spec.name) {
            return Err(format!("路由 {:?} 已在路由库里", spec.name));
        }
        let mut pending = st.routes.clone();
        pending.insert(spec.name.clone(), spec.clone());
        Ok((pending, format!("接管配置文件端口 {}", spec.name)))
    }

    /// 写盘成功后再换内存。顺序不能反。
    pub fn commit(
        &self,
        actor: &str,
        pending: BTreeMap<String, RouteConfig>,
        action: &str,
    ) -> Result<(), String> {
        let mut st = self.lock();
        let Some(path) = &self.path else {
            return Err("未配置 routes_file，无法保存".into());
        };
        persist(path, &pending)?;
        st.routes = pending;
        st.recent.push_front(Audit {
            at: std::time::Instant::now(),
            actor: actor.to_string(),
            action: action.to_string(),
            target: String::new(),
        });
        while st.recent.len() > AUDIT_KEEP {
            st.recent.pop_back();
        }
        Ok(())
    }
}

fn probe_writable(path: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("创建目录 {} 失败：{e}", dir.display()))?;
    }
    let tmp = sibling_tmp(path);
    let r = (|| -> std::io::Result<()> {
        std::fs::write(&tmp, b"probe")?;
        std::fs::remove_file(&tmp)
    })();
    r.map_err(|e| format!("{} 不可写：{e}", path.display()))
}

fn sibling_tmp(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

fn persist(path: &Path, routes: &BTreeMap<String, RouteConfig>) -> Result<(), String> {
    let body = if routes.is_empty() {
        String::new()
    } else {
        let file = StoreFile {
            route: routes.values().cloned().collect(),
        };
        toml::to_string_pretty(&file).map_err(|e| format!("序列化路由库失败：{e}"))?
    };
    let text = format!(
        "# esc 网关路由库 —— 由管理面板维护，也可以手工编辑。\n\
         #\n\
         # 改完**立即生效**（面板保存时会现绑监听套接字，失败就整体不动）。\n\
         # 手工改这个文件要重启网关才生效。\n\
         #\n\
         # 同名路由会覆盖网关配置文件里的定义；把这里的一条删掉，配置文件里的\n\
         # 同名端口就会重新生效。配置文件里的路由在面板里是只读的。\n\
         #\n\
         # listen   对外监听地址，如 \"0.0.0.0:2224\"\n\
         # upstream 上游地址，如 \"127.0.0.1:18788\"\n\
         #\n\
         # 一个端口都绑不上时就没人能访问面板了，所以删最后一条路由前先想清楚。\n\n{body}"
    );

    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("创建目录 {} 失败：{e}", dir.display()))?;
    }
    let tmp = sibling_tmp(path);
    {
        use std::io::Write;
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("写 {} 失败：{e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        f.write_all(text.as_bytes())
            .map_err(|e| format!("写 {} 失败：{e}", tmp.display()))?;
        // 先落盘再 rename：否则断电可能留下一个「存在但为空」的路由库，
        // 那会让网关一条路由都没有——包括面板自己那条。
        f.sync_all()
            .map_err(|e| format!("fsync {} 失败：{e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("替换 {} 失败：{e}", path.display())
    })?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 运行时注册表
// ─────────────────────────────────────────────────────────────────────────────

/// 当前生效的路由。**每个请求都从这里读一次**——这是"改上游不用重启"的全部秘密。
#[derive(Debug, Default)]
pub struct RouteRegistry {
    inner: RwLock<HashMap<String, Arc<RouteRuntime>>>,
}

impl RouteRegistry {
    pub fn new(routes: Vec<RouteRuntime>) -> Self {
        let map = routes
            .into_iter()
            .map(|r| (r.name.clone(), Arc::new(r)))
            .collect();
        Self {
            inner: RwLock::new(map),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Arc<RouteRuntime>>> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn replace_all(&self, routes: Vec<RouteRuntime>) {
        let map = routes
            .into_iter()
            .map(|r| (r.name.clone(), Arc::new(r)))
            .collect();
        *self.inner.write().unwrap_or_else(|e| e.into_inner()) = map;
    }

    /// 插入或替换一条（e2e 里把占位端口改成真实端口时用）。
    pub fn replace_route(&self, route: RouteRuntime) {
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(route.name.clone(), Arc::new(route));
    }

    pub fn get(&self, name: &str) -> Option<Arc<RouteRuntime>> {
        self.read().get(name).cloned()
    }

    /// 按监听地址找。accept 之后拿到的 `local_addr` 是稳定的，
    /// 所以**请求里用它查**而不是用路由名：改名之后老连接也能立刻用上新定义。
    pub fn at(&self, addr: SocketAddr) -> Option<Arc<RouteRuntime>> {
        self.read().values().find(|r| r.listen == addr).cloned()
    }

    pub fn all(&self) -> Vec<Arc<RouteRuntime>> {
        let mut v: Vec<_> = self.read().values().cloned().collect();
        v.sort_by_key(|r| r.listen.port());
        v
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.read().keys().cloned().collect();
        v.sort();
        v
    }

    pub fn count(&self) -> usize {
        self.read().len()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 监听套接字的热插拔
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct Running {
    addr: SocketAddr,
    cancel: CancellationToken,
}

/// 当前正在监听的端口。
#[derive(Debug, Default)]
pub struct ListenerSet {
    inner: Mutex<HashMap<SocketAddr, Running>>,
}

impl ListenerSet {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SocketAddr, Running>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn addrs(&self) -> Vec<SocketAddr> {
        let mut v: Vec<SocketAddr> = self.lock().keys().copied().collect();
        v.sort_by_key(|a| a.port());
        v
    }

    /// 把一个**外部已经绑好**的监听登记进来，返回它自己的取消令牌。
    ///
    /// 只给测试用：e2e 不走 `run()`（那里 listen 写死了配置里的端口），而是
    /// 自己绑 :0。登记之后，热应用的 `prepare` 才会把这两个端口当成"已经在听"，
    /// 不会再去 bind 一次（那必然 EADDRINUSE），也不会在删除时漏掉。
    pub fn adopt(&self, addr: SocketAddr) -> CancellationToken {
        let token = CancellationToken::new();
        self.lock().insert(
            addr,
            Running {
                addr,
                cancel: token.clone(),
            },
        );
        token
    }

    /// 关掉某个端口的监听（进程退出时用）。
    pub fn cancel(&self, addr: SocketAddr) {
        let running = self.lock().remove(&addr);
        if let Some(r) = running {
            r.cancel.cancel();
        }
    }

    /// 第一阶段：把该绑的端口全绑上。
    ///
    /// 任何一个绑不上，**已绑的也一起放掉**并返回错误——调用方此时还没写盘、
    /// 也没换注册表，于是一切保持原样。这就是"要么全成，要么全不动"。
    pub async fn prepare(&self, wanted: &[RouteRuntime]) -> Result<Prepared, String> {
        let running: HashSet<SocketAddr> = self.lock().keys().copied().collect();
        let want_set: HashSet<SocketAddr> = wanted.iter().map(|r| r.listen).collect();

        let mut start = Vec::new();
        for r in wanted {
            if running.contains(&r.listen) {
                // 这个端口已经在听：内容更新走注册表，不必重新绑定
                continue;
            }
            match tokio::net::TcpListener::bind(r.listen).await {
                Ok(l) => start.push((Arc::new(r.clone()), l)),
                Err(e) => {
                    // start 在这里被 drop，刚绑上的套接字立刻释放
                    return Err(format!(
                        "无法监听 {}：{e}。常见原因：端口被别的进程占着，\
                         或小于 1024 的端口需要额外权限（容器里要加 NET_BIND_SERVICE）",
                        r.listen
                    ));
                }
            }
        }
        let stop = self
            .lock()
            .iter()
            .filter(|(a, _)| !want_set.contains(a))
            .map(|(_, r)| Running {
                addr: r.addr,
                cancel: r.cancel.clone(),
            })
            .collect();
        Ok(Prepared { start, stop })
    }

    /// 第二阶段：起新监听、停掉多余的。
    ///
    /// 到这一步端口已经全部绑成功、磁盘也写成功了，所以这里不该再失败——
    /// 于是它返回 `()`，不装作还能回滚。
    pub fn commit(&self, gw: &Arc<crate::proxy::Gateway>, prepared: Prepared) {
        let mut inner = self.lock();
        for r in prepared.stop {
            r.cancel.cancel();
            inner.remove(&r.addr);
            tracing::info!(addr = %r.addr, "route listener stopped");
        }
        for (route, listener) in prepared.start {
            let token = CancellationToken::new();
            inner.insert(
                route.listen,
                Running {
                    addr: route.listen,
                    cancel: token.clone(),
                },
            );
            let gw = gw.clone();
            tokio::spawn(crate::proxy::serve_route(gw, route, listener, token));
        }
    }
}

/// 已绑定但还没开始 serve 的监听。
#[derive(Debug)]
pub struct Prepared {
    start: Vec<(Arc<RouteRuntime>, tokio::net::TcpListener)>,
    stop: Vec<Running>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("esc-gw-routes-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn spec(name: &str, listen: &str, upstream: &str) -> RouteConfig {
        RouteConfig {
            name: name.into(),
            listen: listen.into(),
            upstream: upstream.into(),
            allowed_cidrs: vec![],
            strip_prefix: None,
            max_body_bytes: None,
        }
    }

    #[test]
    fn a_disabled_store_refuses_writes_with_a_reason() {
        let s = RouteStore::load(None);
        assert!(!s.writable());
        let e = s.stage(spec("a", "0.0.0.0:2224", "127.0.0.1:18788"), true);
        assert!(e.unwrap_err().contains("只读"));
    }

    #[test]
    fn create_persists_and_reloads() {
        let dir = tmpdir("create");
        let path = dir.join("gateway-routes.toml");
        let s = RouteStore::load(Some(&path));
        assert!(s.writable(), "{:?}", s.note());

        let (pending, action) = s
            .stage(spec("p1", "0.0.0.0:2224", "127.0.0.1:18788"), true)
            .unwrap();
        s.commit("admin", pending, &action).unwrap();

        // 审计只在内存里（重启清空，权威记录在 tracing 日志）
        assert_eq!(s.recent()[0].action, "新建端口 p1");

        let s2 = RouteStore::load(Some(&path));
        let got = s2.get("p1").expect("重载后应有 p1");
        assert_eq!(got.listen, "0.0.0.0:2224");
    }

    #[test]
    fn file_is_owner_only() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = tmpdir("mode");
            let path = dir.join("gateway-routes.toml");
            let s = RouteStore::load(Some(&path));
            let (p, a) = s
                .stage(spec("p1", "0.0.0.0:2224", "127.0.0.1:18788"), true)
                .unwrap();
            s.commit("admin", p, &a).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn duplicate_create_is_rejected() {
        let dir = tmpdir("dup");
        let s = RouteStore::load(Some(&dir.join("gateway-routes.toml")));
        let (p, a) = s
            .stage(spec("p1", "0.0.0.0:2224", "127.0.0.1:18788"), true)
            .unwrap();
        s.commit("admin", p, &a).unwrap();
        let e = s
            .stage(spec("p1", "0.0.0.0:2225", "127.0.0.1:18788"), true)
            .unwrap_err();
        assert!(e.contains("已存在"), "{e}");
    }

    #[test]
    fn editing_an_unknown_route_points_at_adopt() {
        let dir = tmpdir("unknown");
        let s = RouteStore::load(Some(&dir.join("gateway-routes.toml")));
        let e = s
            .stage(spec("ghost", "0.0.0.0:2224", "127.0.0.1:18788"), false)
            .unwrap_err();
        assert!(e.contains("接管"), "错误信息要指出下一步：{e}");
    }

    #[test]
    fn deleting_a_config_route_is_refused() {
        let dir = tmpdir("delcfg");
        let s = RouteStore::load(Some(&dir.join("gateway-routes.toml")));
        let e = s.stage_delete("from-config").unwrap_err();
        assert!(e.contains("gateway.toml"), "{e}");
    }

    #[test]
    fn a_broken_file_does_not_panic_and_is_reported() {
        let dir = tmpdir("broken");
        let path = dir.join("gateway-routes.toml");
        std::fs::write(&path, "这不是 toml {{{").unwrap();
        let s = RouteStore::load(Some(&path));
        assert_eq!(s.count(), 0);
        assert!(s.note().unwrap().contains("解析失败"));
    }

    #[test]
    fn bad_entries_are_skipped_but_good_ones_survive() {
        let dir = tmpdir("partial");
        let path = dir.join("gateway-routes.toml");
        std::fs::write(
            &path,
            r#"
[[route]]
name = "good"
listen = "0.0.0.0:2224"
upstream = "127.0.0.1:18788"

[[route]]
name = "bad-listen"
listen = "不是地址"
upstream = "127.0.0.1:18788"
"#,
        )
        .unwrap();
        let s = RouteStore::load(Some(&path));
        assert!(s.get("good").is_some());
        assert!(s.get("bad-listen").is_none(), "坏条目必须拒绝");
        assert!(!s.rejected().is_empty());
    }

    #[test]
    fn registry_looks_up_by_listen_address() {
        let rts = crate::config::validate_route_list(&[
            spec("a", "0.0.0.0:2224", "127.0.0.1:18788"),
            spec("b", "0.0.0.0:2225", "127.0.0.1:18789"),
        ])
        .unwrap();
        let reg = RouteRegistry::new(rts);
        // 按名字和按地址都要能找到同一条
        assert_eq!(reg.get("a").unwrap().name, "a");
        assert_eq!(reg.at("0.0.0.0:2224".parse().unwrap()).unwrap().name, "a");
        assert!(reg.at("0.0.0.0:9999".parse().unwrap()).is_none());
        assert_eq!(reg.names(), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn registry_replace_all_swaps_everything() {
        let reg = RouteRegistry::new(
            crate::config::validate_route_list(&[spec("a", "0.0.0.0:2224", "127.0.0.1:1")])
                .unwrap(),
        );
        reg.replace_all(
            crate::config::validate_route_list(&[spec("b", "0.0.0.0:2225", "127.0.0.1:2")])
                .unwrap(),
        );
        assert!(reg.get("a").is_none());
        assert!(reg.get("b").is_some());
    }
}
