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
    /// 来自配置文件的端口，被面板下架了。
    ConfigDisabled,
    /// 来自路由库的端口，被面板下架了。
    StoreDisabled,
}

impl RouteSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Config => "配置文件（只读）",
            Self::Store => "路由库",
            Self::StoreOverConfig => "路由库（覆盖了配置文件）",
            Self::ConfigDisabled => "配置文件（已下架）",
            Self::StoreDisabled => "路由库（已下架）",
        }
    }
    /// 现在能不能编辑。**都能**——面板会隐式接管，不需要用户先点「接管」。
    pub fn editable(self) -> bool {
        true
    }
}

/// 一条生效的路由。
#[derive(Debug, Clone)]
pub struct EffectiveRoute {
    pub spec: RouteConfig,
    pub source: RouteSource,
    /// 已下架：不监听，但列在面板里可恢复。
    pub disabled: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// 路由库（磁盘）
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreFile {
    #[serde(default)]
    route: Vec<RouteConfig>,
    /// 被面板**下架**的端口。存的是下架那一刻的完整定义，所以能一键恢复。
    ///
    /// 为什么需要它：配置文件里的端口面板删不掉（文件是只读的、也不该被程序
    /// 改写）。没有墓碑的话，"删除"会变成"删掉路由库里那条覆盖项，然后配置
    /// 里那条又冒出来"。墓碑让"下架"成为一个可表达的稳定状态。
    #[serde(default)]
    disabled: Vec<RouteConfig>,
}

/// 一次改动的候选状态：生效的 + 已下架的。
///
/// 两者必须一起进两阶段流程。漏掉 `disabled` 就会出现"文件里下架了、内存里还在
/// 监听"，或者反过来——两种都是要人命的（端口占着却管不着）。
#[derive(Debug, Clone)]
pub struct Pending {
    pub routes: BTreeMap<String, RouteConfig>,
    pub disabled: BTreeMap<String, RouteConfig>,
}

#[derive(Debug)]
struct State {
    routes: BTreeMap<String, RouteConfig>,
    /// 已下架的端口（下架时的快照）。它们不监听，但列在面板里可以恢复。
    disabled: BTreeMap<String, RouteConfig>,
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
            disabled: BTreeMap::new(),
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
                        for r in file.disabled {
                            // 下架的条目也要求能解析——解析不了说明配置写坏了，
                            // 静默忽略会让人以为"恢复"按钮丢了。
                            match r.to_runtime() {
                                Ok(_) => {
                                    st.disabled.insert(r.name.clone(), r);
                                }
                                Err(e) => st.rejected.push(format!("（已下架）{e}")),
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
    /// 已下架的端口（下架时的快照）。
    pub fn all_disabled(&self) -> Vec<RouteConfig> {
        self.lock().disabled.values().cloned().collect()
    }
    pub fn disabled_count(&self) -> usize {
        self.lock().disabled.len()
    }
    pub fn recent(&self) -> Vec<Audit> {
        self.lock().recent.iter().cloned().collect()
    }

    /// 写入一条路由的候选状态（新建或覆盖）。
    ///
    /// **不再要求「先接管」**：面板改一条来自 `gateway.toml` 的端口时，直接在这里
    /// 生成一条同名的库内条目——效果就是隐式接管。用户不必先理解两个文件的关系
    /// 才能编辑；显式的「接管」按钮因此去掉了。
    ///
    /// 只拦一种情况：**新建**时名字已被占用（库里或配置里）。同名新增会覆盖掉一个
    /// 正在服务的端口，而用户以为自己只是加了一个。
    ///
    /// 这里刻意不判断"能不能绑上"——那是 [`ListenerSet::prepare`] 的事。两阶段的
    /// 意义就是把"能不能"和"写不写"分开。
    pub fn stage(
        &self,
        spec: RouteConfig,
        creating: bool,
        known_names: &[String],
    ) -> Result<(Pending, String), String> {
        let st = self.readonly_check()?;
        if creating && known_names.iter().any(|n| n == &spec.name) {
            return Err(format!("端口名 {} 已经存在（换一个名字）", spec.name));
        }
        let mut p = st.pending();
        // 从"已下架"里编辑 = 顺手恢复它。否则改完还是不生效，很费解。
        p.disabled.remove(&spec.name);
        p.routes.insert(spec.name.clone(), spec.clone());
        let verb = if creating {
            "新建端口"
        } else {
            "修改端口"
        };
        Ok((p, format!("{verb} {}", spec.name)))
    }

    /// 下架一条路由（内存候选）。
    ///
    /// 不下架的话，来自配置文件的端口"删不掉"——它是只读文件里的一条，
    /// 删掉库里的覆盖项只会让它又冒出来。所以下架 = 在 `disabled` 里留一个墓碑
    /// （带完整定义），恢复时原样搬回来。
    pub fn stage_disable(&self, spec: &RouteConfig) -> Result<(Pending, String), String> {
        let st = self.readonly_check()?;
        let mut p = st.pending();
        p.routes.remove(&spec.name);
        p.disabled.insert(spec.name.clone(), spec.clone());
        Ok((p, format!("下架端口 {}", spec.name)))
    }

    /// **真删除**一条路由（内存候选）：从库里抹掉，不留墓碑。
    ///
    /// 和下架的区别是"以后还要不要它"：下架留着定义以便恢复，删除是"这个端口
    /// 我以后不再需要"。
    ///
    /// 只对**路由库自己拥有**的条目有意义。配置文件里的端口删不掉（只读文件），
    /// 墓碑也删不得（删了配置里那条就会复活）——这两种情况由调用方拦在前面，
    /// 因为这里看不到配置文件。
    pub fn stage_delete(&self, name: &str) -> Result<(Pending, String), String> {
        let st = self.readonly_check()?;
        let mut p = st.pending();
        let was_enabled = p.routes.remove(name).is_some();
        let was_disabled = p.disabled.remove(name).is_some();
        if !was_enabled && !was_disabled {
            return Err(format!("端口 {name:?} 不在路由库里"));
        }
        Ok((p, format!("删除端口 {name}")))
    }

    /// 恢复一个已下架的端口（内存候选）。
    pub fn stage_enable(&self, name: &str) -> Result<(Pending, String), String> {
        let st = self.readonly_check()?;
        let Some(spec) = st.disabled.get(name).cloned() else {
            return Err(format!("端口 {name:?} 不在已下架列表里"));
        };
        let mut p = st.pending();
        p.disabled.remove(name);
        p.routes.insert(name.to_string(), spec);
        Ok((p, format!("恢复端口 {name}")))
    }

    /// 写盘成功后再换内存。顺序不能反。
    pub fn commit(&self, actor: &str, pending: Pending, action: &str) -> Result<(), String> {
        let mut st = self.lock();
        let Some(path) = &self.path else {
            return Err("未配置 routes_file，无法保存".into());
        };
        persist(path, &pending)?;
        st.routes = pending.routes;
        st.disabled = pending.disabled;
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

impl State {
    fn pending(&self) -> Pending {
        Pending {
            routes: self.routes.clone(),
            disabled: self.disabled.clone(),
        }
    }
}

/// 只读检查。抽出来是因为每个 stage_* 都要先说同一句话。
impl RouteStore {
    fn readonly_check(&self) -> Result<std::sync::MutexGuard<'_, State>, String> {
        let st = self.lock();
        if st.writable {
            Ok(st)
        } else {
            Err(format!(
                "路由库是只读的，改动无法保存{}",
                st.note
                    .as_ref()
                    .map(|n| format!("（{n}）"))
                    .unwrap_or_default()
            ))
        }
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

fn persist(path: &Path, pending: &Pending) -> Result<(), String> {
    let file = StoreFile {
        route: pending.routes.values().cloned().collect(),
        disabled: pending.disabled.values().cloned().collect(),
    };
    let body = if pending.routes.is_empty() && pending.disabled.is_empty() {
        String::new()
    } else {
        toml::to_string_pretty(&file).map_err(|e| format!("序列化路由库失败：{e}"))?
    };
    let text = format!(
        "# esc 网关路由库 —— 由管理面板维护，也可以手工编辑。\n\
         #\n\
         # 改完**立即生效**（面板保存时会现绑监听套接字，失败就整体不动）。\n\
         # 手工改这个文件要重启网关才生效。\n\
         #\n\
         # 同名端口会覆盖网关配置文件里的定义。想让配置文件里那条重新生效，\n\
         # 把这里同名的删掉（或把它挪进 [[disabled]] 之外的区域——面板上的\n\
         # 「恢复」按钮做的就是这件事的反向操作）。\n\
         #\n\
         # listen   对外监听地址，如 \"0.0.0.0:2224\"\n\
         # upstream 上游地址，如 \"127.0.0.1:18788\"\n\
         #\n\
         # [[disabled]] 是**已下架**的端口：不监听，但定义留着。\n\
         # 配置文件里的端口没法从面板里真正删掉（那是只读文件），所以「删除」\n\
         # 写在这里，恢复时原样搬回上面。\n\n{body}"
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
        let e = s.stage(spec("a", "0.0.0.0:2224", "127.0.0.1:18788"), true, &[]);
        assert!(e.unwrap_err().contains("只读"));
    }

    #[test]
    fn create_persists_and_reloads() {
        let dir = tmpdir("create");
        let path = dir.join("gateway-routes.toml");
        let s = RouteStore::load(Some(&path));
        assert!(s.writable(), "{:?}", s.note());

        let (pending, action) = s
            .stage(spec("p1", "0.0.0.0:2224", "127.0.0.1:18788"), true, &[])
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
                .stage(spec("p1", "0.0.0.0:2224", "127.0.0.1:18788"), true, &[])
                .unwrap();
            s.commit("admin", p, &a).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn editing_a_route_that_only_exists_in_the_config_just_works() {
        // 以前这里要求先「接管」，现在隐式接管：面板改一条配置文件里的端口，
        // 直接在库里生成同名条目。
        let dir = tmpdir("upsert");
        let s = RouteStore::load(Some(&dir.join("gateway-routes.toml")));
        let (p, action) = s
            .stage(
                spec("from-config", "0.0.0.0:2226", "127.0.0.1:18789"),
                false,
                &[],
            )
            .unwrap();
        s.commit("admin", p, &action).unwrap();
        let got = s.get("from-config").expect("应已写进路由库");
        assert_eq!(got.listen, "0.0.0.0:2226");
    }

    #[test]
    fn creating_with_a_name_that_already_exists_is_rejected() {
        // 新建撞名字会覆盖掉一个正在服务的端口，而用户以为自己只是加了一个
        let dir = tmpdir("dupname");
        let s = RouteStore::load(Some(&dir.join("gateway-routes.toml")));
        let known = vec!["server-panel".to_string()];
        let e = s
            .stage(
                spec("server-panel", "0.0.0.0:2226", "127.0.0.1:1"),
                true,
                &known,
            )
            .unwrap_err();
        assert!(e.contains("已经存在"), "{e}");
    }

    #[test]
    fn disabling_keeps_the_definition_for_restore() {
        let dir = tmpdir("disable");
        let path = dir.join("gateway-routes.toml");
        let s = RouteStore::load(Some(&path));
        let spec_a = spec("a", "0.0.0.0:2224", "127.0.0.1:18788");
        let (p, act) = s.stage(spec_a.clone(), true, &[]).unwrap();
        s.commit("admin", p, &act).unwrap();

        let (p, act) = s.stage_disable(&spec_a).unwrap();
        s.commit("admin", p, &act).unwrap();
        assert!(s.get("a").is_none(), "下架后不该还在生效列表里");
        assert_eq!(s.disabled_count(), 1);
        // 定义留着，才有得恢复
        assert_eq!(s.all_disabled()[0].upstream, "127.0.0.1:18788");

        let (p, act) = s.stage_enable("a").unwrap();
        s.commit("admin", p, &act).unwrap();
        assert_eq!(s.get("a").unwrap().listen, "0.0.0.0:2224");
        assert_eq!(s.disabled_count(), 0);
    }

    #[test]
    fn disabled_survives_a_reload() {
        let dir = tmpdir("disable-persist");
        let path = dir.join("gateway-routes.toml");
        let s = RouteStore::load(Some(&path));
        let spec_a = spec("a", "0.0.0.0:2224", "127.0.0.1:18788");
        let (p, act) = s.stage_disable(&spec_a).unwrap();
        s.commit("admin", p, &act).unwrap();

        // 重启后墓碑还在——否则配置文件里那条会悄悄复活
        let s2 = RouteStore::load(Some(&path));
        assert_eq!(s2.disabled_count(), 1, "下架状态必须写进文件");
        assert_eq!(s2.all_disabled()[0].name, "a");
    }

    #[test]
    fn deleting_removes_it_completely_including_the_tombstone() {
        let dir = tmpdir("delete");
        let path = dir.join("gateway-routes.toml");
        let s = RouteStore::load(Some(&path));
        let a = spec("a", "0.0.0.0:2224", "127.0.0.1:18788");
        let (p, act) = s.stage(a.clone(), true, &[]).unwrap();
        s.commit("admin", p, &act).unwrap();

        // 下架后是墓碑
        let (p, act) = s.stage_disable(&a).unwrap();
        s.commit("admin", p, &act).unwrap();
        assert_eq!(s.disabled_count(), 1);

        // 删除：连墓碑一起没了，重启也不会回来
        let (p, act) = s.stage_delete("a").unwrap();
        s.commit("admin", p, &act).unwrap();
        assert_eq!(s.count(), 0);
        assert_eq!(
            s.disabled_count(),
            0,
            "墓碑也要一起删掉，否则它会一直占着名字"
        );
        let reloaded = RouteStore::load(Some(&path));
        assert_eq!(reloaded.count(), 0);
        assert_eq!(reloaded.disabled_count(), 0);
    }

    #[test]
    fn deleting_something_absent_is_an_error() {
        let dir = tmpdir("delabsent");
        let s = RouteStore::load(Some(&dir.join("gateway-routes.toml")));
        assert!(s.stage_delete("ghost").unwrap_err().contains("不在路由库"));
    }

    #[test]
    fn enabling_something_that_is_not_disabled_is_an_error() {
        let dir = tmpdir("norestore");
        let s = RouteStore::load(Some(&dir.join("gateway-routes.toml")));
        let e = s.stage_enable("ghost").unwrap_err();
        assert!(e.contains("不在已下架列表"), "{e}");
    }

    #[test]
    fn deleting_a_config_route_leaves_a_tombstone() {
        // 没有墓碑的话，"删除配置文件里的端口"等于删掉库里的覆盖项，
        // 而配置里那条马上又冒出来——用户会觉得删除按钮坏了。
        let dir = tmpdir("delcfg");
        let s = RouteStore::load(Some(&dir.join("gateway-routes.toml")));
        let cfg_only = spec("from-config", "0.0.0.0:2225", "127.0.0.1:18788");
        let (p, act) = s.stage_disable(&cfg_only).unwrap();
        s.commit("admin", p, &act).unwrap();
        assert_eq!(s.disabled_count(), 1);
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
