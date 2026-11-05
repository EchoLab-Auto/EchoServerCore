//! 账号库：**管理面板**管理的账号，叠加在 `gateway.toml` 的 `[[user]]` 之上。
//!
//! # 为什么不是直接改写 gateway.toml
//!
//! 三个理由，都不是洁癖：
//!
//! 1. **两个写者**。操作员随时 SSH 上去手改配置，面板同时在写，谁也不知道对方
//!    改了什么。分开两个文件，至少冲突点只有一个（同名账号，见下）。
//! 2. **注释会被吃掉**。`gateway.toml` 里那些「为什么这么配」的注释是运维资产，
//!    用 `toml::to_string` 重写一遍就全没了。
//! 3. **配置文件常常是只读挂载**。容器里 `/etc/esc/gateway.toml` 往往是 `:ro`，
//!    甚至整个根文件系统只读。账号库要能落到可写卷上。
//!
//! # 优先级：账号库覆盖配置文件
//!
//! 同名账号以账号库为准。这样有一个很实在的好处：**配置文件永远是可用的救援
//! 通道**。账号库误删、损坏、卷没挂上——`gateway.toml` 里的账号照样能登进来，
//! 而不是「管理面板挂了 + 账号库丢了 = 谁也进不去」。
//!
//! 代价是「改了 gateway.toml 里的口令却还是旧口令能登」这种困惑。所以：
//!
//! * 启动时对每个被覆盖的同名账号打一条 WARN；
//! * 面板的账号列表里把「来自配置文件」和「账号库覆盖了配置文件」标出来。
//!
//! # 写盘
//!
//! 「写同目录临时文件 → fsync → rename」原子替换，权限 0600。先在磁盘上成功，
//! 再更新内存——反过来会出现「面板显示改成功了，重启后变回去」。

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::config::{hash_password, valid_user_name, UserRuntime};

/// 口令最短长度。面板拒绝更短的新口令。
///
/// 不管这条也「能用」，但一个公网入口的管理员口令是 6 个字符时，
/// 前面那些 argon2、限速、SameSite 的功夫全白费。
pub const MIN_PASSWORD_LEN: usize = 8;

/// 「最近操作」保留条数。只在内存里，重启即清空——权威审计在 tracing 日志里。
pub const AUDIT_KEEP: usize = 50;

/// 账号库里的一条账号。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoredUser {
    pub name: String,
    /// argon2id 哈希。**面板不显示、不返回**，只能整体重置。
    pub hash: String,
    /// 授权范围。`["*"]` = 全部端口（含以后新增的）。
    #[serde(default)]
    pub routes: Vec<String>,
    /// 是否允许进管理面板。
    #[serde(default)]
    pub admin: bool,
}

impl StoredUser {
    pub fn to_runtime(&self) -> UserRuntime {
        let all_routes = self.routes.iter().any(|r| r == "*");
        UserRuntime {
            name: self.name.clone(),
            hash: self.hash.clone(),
            all_routes,
            routes: self.routes.iter().filter(|r| *r != "*").cloned().collect(),
            admin: self.admin,
        }
    }
}

/// 账号库文件的磁盘格式：和 `gateway.toml` 一样的 `[[user]]`。
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreFile {
    #[serde(default)]
    user: Vec<StoredUser>,
}

/// 一次账号变更的记录（内存环形缓冲）。
#[derive(Debug, Clone)]
pub struct Audit {
    pub at: Instant,
    pub actor: String,
    pub action: String,
    pub target: String,
}

#[derive(Debug)]
struct State {
    users: BTreeMap<String, StoredUser>,
    /// 账号库是否可写。面板据此决定「只读」还是「可编辑」。
    writable: bool,
    /// 不可写 / 载入异常的原因，直接显示在面板上。
    note: Option<String>,
    /// 载入时被拒绝的坏条目（`<名字>: <原因>`）。
    rejected: Vec<String>,
    recent: VecDeque<Audit>,
}

/// 面板管理的账号集合。
#[derive(Debug)]
pub struct AccountStore {
    path: Option<PathBuf>,
    state: Mutex<State>,
}

impl AccountStore {
    /// 载入账号库。任何问题都**不阻塞启动**，只记进 `note`/`rejected`。
    ///
    /// 这里的取舍：账号库解析失败时宁可「用配置文件里的账号继续服务 + 面板顶上
    /// 挂一条红条」，也不肯让网关拒绝启动——认证网关不启动意味着所有面板端口
    /// 直接不可用（或者更糟：有人为了让服务起来，临时把面板的 auth 删了）。
    /// 而且配置文件兜底仍然有效，最坏情况也不会锁死。
    pub fn load(path: Option<&Path>) -> Self {
        let mut st = State {
            users: BTreeMap::new(),
            writable: false,
            note: None,
            rejected: Vec::new(),
            recent: VecDeque::new(),
        };

        let Some(path) = path else {
            st.note =
                Some("未配置 users_file，账号只能通过 gateway.toml 管理（面板为只读）。".into());
            return Self {
                path: None,
                state: Mutex::new(st),
            };
        };

        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(raw) => match toml::from_str::<StoreFile>(&raw) {
                    Ok(file) => {
                        for u in file.user {
                            match validate_entry(&u) {
                                Ok(()) => {
                                    if st.users.insert(u.name.clone(), u.clone()).is_some() {
                                        st.rejected
                                            .push(format!("{}: 同名账号重复，后一条生效", u.name));
                                    }
                                }
                                Err(e) => st.rejected.push(e),
                            }
                        }
                    }
                    Err(e) => {
                        st.note = Some(format!(
                            "账号库 {} 解析失败（{e}）——已忽略，当前只用 gateway.toml 里的账号",
                            path.display()
                        ));
                    }
                },
                Err(e) => {
                    st.note = Some(format!(
                        "账号库 {} 读取失败（{e}）——当前只用 gateway.toml 里的账号",
                        path.display()
                    ));
                }
            }
        }

        // 探测可写性。为什么不等到第一次写再报：管理面板上要**提前**显示
        // 「这里是只读的，改了不会存」，而不是让操作员填完表单才被拒。
        match probe_writable(path) {
            Ok(()) => st.writable = true,
            Err(e) => {
                if st.note.is_none() {
                    st.note = Some(format!("账号库不可写：{e}"));
                }
            }
        }

        Self {
            path: Some(path.to_path_buf()),
            state: Mutex::new(st),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // 中毒锁也继续用：账号库里的数据是自洽的（写盘成功才换内存），
        // 没有「改了一半」的中间态。
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
        self.lock().users.len()
    }

    /// 账号库里的管理员数量（不含配置文件里的）。
    pub fn admin_count(&self) -> usize {
        self.lock().users.values().filter(|u| u.admin).count()
    }

    pub fn get(&self, name: &str) -> Option<StoredUser> {
        self.lock().users.get(name).cloned()
    }

    /// 全部账号，按名字排序。
    pub fn all(&self) -> Vec<StoredUser> {
        self.lock().users.values().cloned().collect()
    }

    pub fn recent(&self) -> Vec<Audit> {
        self.lock().recent.iter().cloned().collect()
    }

    // ── 变更 ──────────────────────────────────────────────────────────────

    /// 新建账号。
    pub fn create(
        &self,
        actor: &str,
        name: &str,
        password: &str,
        routes: Vec<String>,
        admin: bool,
    ) -> Result<(), String> {
        let name = name.trim();
        check_name(name)?;
        check_password(password)?;
        let hash = hash_password(password).map_err(|e| format!("生成口令哈希失败：{e}"))?;
        let entry = StoredUser {
            name: name.to_string(),
            hash,
            routes,
            admin,
        };

        let mut st = self.lock();
        if st.users.contains_key(name) {
            return Err(format!("账号 {name:?} 在账号库里已存在"));
        }
        let mut pending = st.users.clone();
        pending.insert(name.to_string(), entry);
        self.commit(&mut st, pending)?;
        push_audit(&mut st, actor, "新建账号", name);
        Ok(())
    }

    /// 更新账号。`password` 为 `None` 表示不改口令。
    pub fn update(
        &self,
        actor: &str,
        name: &str,
        password: Option<&str>,
        routes: Vec<String>,
        admin: bool,
    ) -> Result<(), String> {
        let mut st = self.lock();
        let mut pending = st.users.clone();
        let cur = pending.get(name).cloned().ok_or_else(|| {
            format!("账号 {name:?} 不在账号库里（可能在 gateway.toml 里，需要先「接管」）")
        })?;
        let hash = match password {
            Some(p) => {
                check_password(p)?;
                hash_password(p).map_err(|e| format!("生成口令哈希失败：{e}"))?
            }
            None => cur.hash,
        };
        pending.insert(
            name.to_string(),
            StoredUser {
                name: name.to_string(),
                hash,
                routes,
                admin,
            },
        );
        self.commit(&mut st, pending)?;
        let what = if password.is_some() {
            "修改账号（含口令）"
        } else {
            "修改账号"
        };
        push_audit(&mut st, actor, what, name);
        Ok(())
    }

    /// 删除账号。
    pub fn delete(&self, actor: &str, name: &str) -> Result<(), String> {
        let mut st = self.lock();
        let mut pending = st.users.clone();
        if pending.remove(name).is_none() {
            return Err(format!(
                "账号 {name:?} 不在账号库里（如果它来自 gateway.toml，请改配置文件）"
            ));
        }
        self.commit(&mut st, pending)?;
        push_audit(&mut st, actor, "删除账号", name);
        Ok(())
    }

    /// 把 `gateway.toml` 里的账号「接管」进账号库。
    ///
    /// 复制哈希而不是重设口令：哈希已经是现成的，不需要（也不该）知道原口令。
    /// 接管之后面板才能改它——否则一个来自配置文件的账号在面板里永远是只读的，
    /// 「连自己的口令都改不了」是很差的体验。
    pub fn adopt(&self, actor: &str, user: &UserRuntime) -> Result<(), String> {
        let mut routes = user.routes.clone();
        if user.all_routes {
            routes.insert(0, "*".to_string());
        }
        let entry = StoredUser {
            name: user.name.clone(),
            hash: user.hash.clone(),
            routes,
            admin: user.admin,
        };
        let mut st = self.lock();
        if st.users.contains_key(&user.name) {
            return Err(format!("账号 {:?} 已在账号库里", user.name));
        }
        let mut pending = st.users.clone();
        pending.insert(user.name.clone(), entry);
        self.commit(&mut st, pending)?;
        push_audit(&mut st, actor, "接管配置文件账号", &user.name);
        Ok(())
    }

    /// 只改口令（自助改密用）。
    pub fn set_password(&self, actor: &str, name: &str, password: &str) -> Result<(), String> {
        check_password(password)?;
        let hash = hash_password(password).map_err(|e| format!("生成口令哈希失败：{e}"))?;
        let mut st = self.lock();
        let mut pending = st.users.clone();
        let cur = pending
            .get(name)
            .cloned()
            .ok_or_else(|| format!("账号 {name:?} 不在账号库里"))?;
        pending.insert(name.to_string(), StoredUser { hash, ..cur });
        self.commit(&mut st, pending)?;
        push_audit(&mut st, actor, "修改口令", name);
        Ok(())
    }

    /// 写盘成功后再换内存。**顺序不能反**。
    fn commit(&self, st: &mut State, pending: BTreeMap<String, StoredUser>) -> Result<(), String> {
        if !st.writable {
            return Err(format!(
                "账号库是只读的，改动无法保存{}",
                st.note
                    .as_ref()
                    .map(|n| format!("（{n}）"))
                    .unwrap_or_default()
            ));
        }
        let Some(path) = &self.path else {
            return Err("未配置 users_file，无法保存".into());
        };
        persist(path, &pending)?;
        st.users = pending;
        Ok(())
    }
}

fn push_audit(st: &mut State, actor: &str, action: &str, target: &str) {
    st.recent.push_front(Audit {
        at: Instant::now(),
        actor: actor.to_string(),
        action: action.to_string(),
        target: target.to_string(),
    });
    while st.recent.len() > AUDIT_KEEP {
        st.recent.pop_back();
    }
}

fn check_name(name: &str) -> Result<(), String> {
    if valid_user_name(name) {
        Ok(())
    } else {
        Err(format!(
            "用户名 {name:?} 不合法：只能用字母、数字、`.` `-` `_`，长度 1..=64"
        ))
    }
}

fn check_password(p: &str) -> Result<(), String> {
    if p.chars().count() < MIN_PASSWORD_LEN {
        return Err(format!("口令至少 {MIN_PASSWORD_LEN} 个字符"));
    }
    Ok(())
}

fn validate_entry(u: &StoredUser) -> Result<(), String> {
    if !valid_user_name(&u.name) {
        return Err(format!("{:?}: 用户名不合法", u.name));
    }
    if !u.hash.starts_with("$argon2") {
        return Err(format!(
            "{:?}: hash 不是 argon2 格式（拒绝明文口令）",
            u.name
        ));
    }
    if u.routes.is_empty() {
        return Err(format!("{:?}: routes 为空，此账号无法访问任何端口", u.name));
    }
    Ok(())
}

/// 探测账号库是否可写：能建目录、能写临时文件、能删。
///
/// 检查权限位是不准的（ACL、只读挂载、uid 映射都看不出来），所以直接试一次。
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

/// 原子写：临时文件 → fsync → rename。
fn persist(path: &Path, users: &BTreeMap<String, StoredUser>) -> Result<(), String> {
    let body = if users.is_empty() {
        // 空库写成一堆 `user = []` 很难看，也容易让人以为格式错了
        String::new()
    } else {
        let file = StoreFile {
            user: users.values().cloned().collect(),
        };
        toml::to_string_pretty(&file).map_err(|e| format!("序列化账号库失败：{e}"))?
    };
    let text = format!(
        "# esc 网关账号库 —— 由管理面板维护，也可以手工编辑。\n\
         #\n\
         # 手工改完要**重启网关**才生效（运行中的进程用的是内存里的副本）。\n\
         # 同名账号会覆盖网关配置文件里的定义；把这里的一条删掉，配置文件\n\
         # 里的同名账号就会重新生效。\n\
         #\n\
         # 口令只存 argon2id 哈希，忘掉了就重置：在面板里改，或者用\n\
         #   esc-gateway hash-password\n\
         # 生成一个再填到 hash 字段。明文口令会被拒绝。\n\n{body}"
    );

    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("创建目录 {} 失败：{e}", dir.display()))?;
    }
    // 临时文件必须与目标同目录：跨文件系统的 rename 会失败（EXDEV）
    let tmp = sibling_tmp(path);
    {
        use std::io::Write;
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("写 {} 失败：{e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // 口令哈希不是明文口令，但也不该让同机其他用户随便读——离线爆破
            // 一个 8 字符口令只需要几小时。
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        f.write_all(text.as_bytes())
            .map_err(|e| format!("写 {} 失败：{e}", tmp.display()))?;
        // fsync 之后再 rename：否则断电可能留下一个「存在但内容为空」的账号库，
        // 那比没有账号库更糟——空库不会回落到配置文件，只是没人能登录。
        f.sync_all()
            .map_err(|e| format!("fsync {} 失败：{e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("替换 {} 失败：{e}", path.display())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        // Instant 不提供「现在几点」，用它算唯一后缀会退化成固定值，
        // 并行跑的测试就会互相踩文件。用单调计数器 + 进程 id。
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("esc-gw-store-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    const PW: &str = "correct-horse-battery";

    #[test]
    fn disabled_store_refuses_writes_with_a_clear_reason() {
        let s = AccountStore::load(None);
        assert!(!s.writable());
        assert_eq!(s.count(), 0);
        let e = s
            .create("a", "bob", PW, vec!["*".into()], false)
            .unwrap_err();
        assert!(e.contains("只读"), "错误信息要能直接看懂：{e}");
    }

    #[test]
    fn create_persists_and_reloads() {
        let dir = tmpdir("create");
        let path = dir.join("users.toml");
        let s = AccountStore::load(Some(&path));
        assert!(s.writable(), "note={:?}", s.note());
        s.create("admin-act", "alice", PW, vec!["*".into()], true)
            .unwrap();
        assert_eq!(s.count(), 1);

        // 重新载入：证明真写盘了，不只是内存
        let s2 = AccountStore::load(Some(&path));
        let u = s2.get("alice").expect("重载后应有 alice");
        assert!(u.admin);
        assert_eq!(u.routes, vec!["*".to_string()]);
        assert_ne!(u.hash, PW, "绝不能把明文写进账号库");
        assert!(crate::config::verify_password(PW, &u.hash));
    }

    #[test]
    fn file_is_owner_only() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = tmpdir("mode");
            let path = dir.join("users.toml");
            let s = AccountStore::load(Some(&path));
            s.create("a", "bob", PW, vec!["*".into()], false).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "账号库应为 0600");
        }
    }

    #[test]
    fn short_password_is_rejected() {
        let dir = tmpdir("shortpw");
        let s = AccountStore::load(Some(&dir.join("users.toml")));
        let e = s
            .create("a", "bob", "1234567", vec!["*".into()], false)
            .unwrap_err();
        assert!(e.contains("至少"), "{e}");
        assert_eq!(s.count(), 0, "被拒绝的创建不能留下账号");
    }

    #[test]
    fn bad_user_names_are_rejected() {
        let dir = tmpdir("badname");
        let s = AccountStore::load(Some(&dir.join("users.toml")));
        for bad in ["", "a b", "a/b", "../x", "用户名", &"x".repeat(65)] {
            assert!(
                s.create("a", bad, PW, vec!["*".into()], false).is_err(),
                "{bad:?} 应该被拒绝"
            );
        }
        assert_eq!(s.count(), 0);
    }

    #[test]
    fn duplicate_is_rejected() {
        let dir = tmpdir("dup");
        let s = AccountStore::load(Some(&dir.join("users.toml")));
        s.create("a", "bob", PW, vec!["*".into()], false).unwrap();
        let e = s
            .create("a", "bob", PW, vec!["*".into()], false)
            .unwrap_err();
        assert!(e.contains("已存在"), "{e}");
    }

    #[test]
    fn update_without_password_keeps_the_old_hash() {
        let dir = tmpdir("keep");
        let path = dir.join("users.toml");
        let s = AccountStore::load(Some(&path));
        s.create("a", "bob", PW, vec!["*".into()], false).unwrap();
        let before = s.get("bob").unwrap().hash;
        s.update("a", "bob", None, vec!["p1".into()], true).unwrap();
        let after = s.get("bob").unwrap();
        assert_eq!(before, after.hash, "没给新口令就不能动哈希");
        assert_eq!(after.routes, vec!["p1".to_string()]);
        assert!(after.admin);
    }

    #[test]
    fn update_of_unknown_account_points_at_adopt() {
        let dir = tmpdir("noadopt");
        let s = AccountStore::load(Some(&dir.join("users.toml")));
        let e = s
            .update("a", "ghost", None, vec!["*".into()], false)
            .unwrap_err();
        assert!(e.contains("接管"), "错误信息要指出下一步动作：{e}");
    }

    #[test]
    fn delete_removes_from_disk() {
        let dir = tmpdir("del");
        let path = dir.join("users.toml");
        let s = AccountStore::load(Some(&path));
        s.create("a", "bob", PW, vec!["*".into()], false).unwrap();
        s.delete("a", "bob").unwrap();
        assert_eq!(s.count(), 0);
        assert!(AccountStore::load(Some(&path)).get("bob").is_none());
    }

    #[test]
    fn adopt_copies_the_hash_without_knowing_the_password() {
        let dir = tmpdir("adopt");
        let s = AccountStore::load(Some(&dir.join("users.toml")));
        let hash = crate::config::hash_password(PW).unwrap();
        let rt = UserRuntime {
            name: "admin".into(),
            hash: hash.clone(),
            all_routes: true,
            routes: vec![],
            admin: true,
        };
        s.adopt("admin", &rt).unwrap();
        let got = s.get("admin").unwrap();
        assert_eq!(got.hash, hash, "接管应原样复制哈希");
        assert_eq!(got.routes, vec!["*".to_string()], "全部权限要落成 *");
        assert!(got.admin);
        // 原口令继续可用
        assert!(crate::config::verify_password(PW, &got.hash));
    }

    #[test]
    fn broken_store_does_not_panic_and_is_reported() {
        let dir = tmpdir("broken");
        let path = dir.join("users.toml");
        std::fs::write(&path, "这不是 toml {{{").unwrap();
        let s = AccountStore::load(Some(&path));
        assert_eq!(s.count(), 0);
        let note = s.note().expect("应记录解析失败");
        assert!(note.contains("解析失败"), "{note}");
    }

    #[test]
    fn bad_entries_are_skipped_but_good_ones_survive() {
        let dir = tmpdir("partial");
        let path = dir.join("users.toml");
        // 明文口令那条必须被拒——这是最危险的手改错误
        std::fs::write(
            &path,
            r#"
[[user]]
name = "good"
hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$11111111111111111111111111111111"
routes = ["*"]

[[user]]
name = "plaintext"
hash = "hunter2"
routes = ["*"]
"#,
        )
        .unwrap();
        let s = AccountStore::load(Some(&path));
        assert!(s.get("good").is_some(), "好条目要保留");
        assert!(s.get("plaintext").is_none(), "明文口令条目必须拒绝");
        let rej = s.rejected();
        assert!(
            rej.iter().any(|r| r.contains("argon2")),
            "要说明拒绝原因：{rej:?}"
        );
    }

    #[test]
    fn audit_records_who_did_what() {
        let dir = tmpdir("audit");
        let s = AccountStore::load(Some(&dir.join("users.toml")));
        s.create("admin-act", "bob", PW, vec!["*".into()], false)
            .unwrap();
        s.delete("admin-act", "bob").unwrap();
        let recent = s.recent();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].action, "删除账号", "最近的在最前面");
        assert_eq!(recent[0].target, "bob");
        assert_eq!(recent[1].action, "新建账号");
    }
}
