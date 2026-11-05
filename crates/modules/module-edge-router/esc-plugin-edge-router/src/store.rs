//! 路由表的**持久化**：一个可写的状态文件。
//!
//! # 为什么不是直接改 `host.toml`
//!
//! 面板上"保存路由"最直觉的做法是回写配置文件，但那会踩三个问题：
//!
//! 1. **systemd 沙箱**。本仓的 `esc-host.service` 有 `ReadOnlyPaths=/etc/esc`，
//!    放开它等于放弃"配置只读"这条边界；
//! 2. **注释与格式**。TOML 解析再序列化会丢掉人写的注释与排版 —— 配置文件
//!    是给人看的，不该被机器改写成另一个样子；
//! 3. **回滚**。改坏了配置，服务可能起不来；而状态文件坏了，删掉它就回到基线。
//!
//! 所以采用**分层**：`host.toml` 里的 `routes` 是**基线**（只读），运行期的
//! 增删写进这里这个状态文件（`/var/lib/esc` 下，`StateDirectory` 已保证可写）。
//! 启动时若状态文件存在，它**整体覆盖**基线 —— 语义是"最后一次人工调整的
//! 结果"，而不是"和基线合并"，否则删不掉基线里的条目。
//!
//! 想回到基线：删掉状态文件 → 重启 `esc-host`。
//!
//! # 原子性
//!
//! 写盘走 `同目录临时文件 → fsync → rename`。rename 在同一文件系统上是原子的，
//! 所以读者（包括下一个启动的自己）永远看不到写了一半的文件。临时文件名带 pid，
//! 避免两个进程同时写时互相踩（虽然正常情况下只有一个宿主）。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use esc_control_api::ControlError;

/// 状态文件的头部注释。**每次写盘都重写它**，因为读这个文件的人（多半是半年后
/// 的运维）需要知道：这是谁写的、优先级怎么算、想回滚该怎么办。
const HEADER: &str = "\
# 由 esc.edge-router 在运行期写入（面板或控制面增删路由时）。
#
# 优先级：本文件**整体覆盖** host.toml 里的 [edge_router.config.routes]。
# 回到配置文件的基线：删掉本文件，然后重启 esc-host。
#
# 注意：手改本文件后需要重启 esc-host 才生效（插件只在 mount 时读它）。
";

/// 一个路由表状态文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteStore {
    path: PathBuf,
}

impl RouteStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读状态文件。
    ///
    /// * 文件不存在 → `Ok(None)`（正常：还没人在面板上改过）
    /// * 文件存在但内容坏了 → `Err`（**fail-closed**）
    ///
    /// 为什么坏了不"忽略并退回基线"：那样用户会以为自己的改动还在，直到某次
    /// 重启后路由莫名消失。宁可挂载失败并明确给出"删掉它就能回到基线"，这是
    /// 可操作的。
    pub fn load(&self) -> Result<Option<BTreeMap<String, u16>>, ControlError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(ControlError::other(format!(
                    "无法读取路由状态文件 {}: {e}",
                    self.path.display()
                )))
            }
        };
        parse(&raw).map(Some).map_err(|e| {
            ControlError::invalid(format!(
                "路由状态文件 {} 内容无效: {e}（删掉它可回到 host.toml 的基线）",
                self.path.display()
            ))
        })
    }

    /// 原子写回整个路由表。
    pub fn save(&self, routes: &BTreeMap<String, u16>) -> Result<(), ControlError> {
        let body = render(routes);

        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| {
            ControlError::other(format!("无法创建目录 {}: {e}", dir.display()))
        })?;

        // 临时文件与目标同目录：跨文件系统的 rename 不是原子的。
        let tmp = dir.join(format!(
            ".{}.tmp.{}",
            self.path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("routes"),
            std::process::id()
        ));
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| {
                ControlError::other(format!("无法写入 {}: {e}", tmp.display()))
            })?;
            f.write_all(body.as_bytes())
                .and_then(|()| f.sync_all())
                .map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    ControlError::other(format!("写入路由状态文件失败: {e}"))
                })?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            ControlError::other(format!(
                "无法替换路由状态文件 {}: {e}",
                self.path.display()
            ))
        })?;

        // 目录项本身也要落盘，否则掉电后可能只保住 rename 一半的效果。
        // 失败不算致命（多数文件系统本来就会顺带同步），所以只忽略。
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }
}

/// 渲染成 TOML。手写而不是 `toml::to_string`：
/// 1. 不想为一个只有一层字符串→整数的结构引入序列化依赖；
/// 2. 键可能含 `.`（完整域名），需要判断要不要加引号 —— 手写更容易保证正确。
fn render(routes: &BTreeMap<String, u16>) -> String {
    let mut out = String::from(HEADER);
    out.push_str("\n[routes]\n");
    for (k, port) in routes {
        out.push_str(&format!("{} = {port}\n", quote_key(k)));
    }
    out
}

/// 键要引号吗？TOML 的裸键只允许 `A-Za-z0-9_-`；含点（完整域名）必须引号，
/// 否则会被当成嵌套表。
fn quote_key(k: &str) -> String {
    let bare = !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if bare {
        k.to_string()
    } else {
        format!("\"{}\"", k.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// 解析状态文件。只认 `[routes]` 这一张表，其余段落忽略（方便用户加注释段）。
fn parse(raw: &str) -> Result<BTreeMap<String, u16>, String> {
    #[derive(serde::Deserialize)]
    struct File {
        #[serde(default)]
        routes: BTreeMap<String, u16>,
    }
    // `toml` 只在宿主侧是直接依赖；这个 crate 的依赖表里没有它，所以走
    // serde_json 的等价物 —— 不，TOML 就是 TOML。用最小手写解析太脆，
    // 因此这里要求调用方所在 crate 提供 toml（见 Cargo.toml）。
    let f: File = toml::from_str(raw).map_err(|e| e.to_string())?;
    Ok(f.routes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "esc-edge-store-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn missing_file_means_no_overlay() {
        let dir = tmpdir("missing");
        let store = RouteStore::new(dir.join("routes.toml"));
        assert_eq!(store.load().unwrap(), None, "文件不存在应是 None，不是空表");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_roundtrips_including_dotted_keys() {
        let dir = tmpdir("roundtrip");
        let store = RouteStore::new(dir.join("routes.toml"));
        let mut routes = BTreeMap::new();
        routes.insert("cad".to_string(), 2228u16);
        routes.insert("panel".to_string(), 8080);
        // 完整域名含点：必须加引号，否则 TOML 会解析成嵌套表
        routes.insert("cad.special.com".to_string(), 9999);

        store.save(&routes).unwrap();
        assert_eq!(store.load().unwrap(), Some(routes.clone()));

        // 文件里应有可读的头部与引号键（"凭直觉能看懂"也是产品要求）
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(text.contains("回到配置文件的基线"), "{text}");
        assert!(text.contains("\"cad.special.com\" = 9999"), "{text}");
        assert!(text.contains("cad = 2228"), "裸键不该被加引号：{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_replaces_previous_content_entirely() {
        let dir = tmpdir("replace");
        let store = RouteStore::new(dir.join("routes.toml"));
        let mut first = BTreeMap::new();
        first.insert("a".to_string(), 1);
        first.insert("b".to_string(), 2);
        store.save(&first).unwrap();

        let mut second = BTreeMap::new();
        second.insert("c".to_string(), 3);
        store.save(&second).unwrap();

        // 覆盖语义：删掉的条目不能残留（这正是"删掉它回基线"能成立的前提）
        assert_eq!(store.load().unwrap(), Some(second));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_fails_closed_with_actionable_message() {
        let dir = tmpdir("corrupt");
        let path = dir.join("routes.toml");
        std::fs::write(&path, "this is not toml [[[").unwrap();
        let store = RouteStore::new(&path);
        let err = store.load().unwrap_err();
        assert_eq!(err.kind, esc_control_api::ErrorKind::Invalid);
        assert!(err.message.contains("删掉它"), "错误要给出补救办法：{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_parent_directory() {
        // /var/lib/esc 在真实部署里由 StateDirectory 建好，但测试环境未必。
        let dir = tmpdir("mkdir");
        let store = RouteStore::new(dir.join("nested/deeper/routes.toml"));
        store.save(&BTreeMap::new()).unwrap();
        assert!(store.path().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_tmp_file_is_left_behind() {
        let dir = tmpdir("cleanup");
        let store = RouteStore::new(dir.join("routes.toml"));
        let mut routes = BTreeMap::new();
        routes.insert("cad".to_string(), 2228);
        store.save(&routes).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件：{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_sections_are_ignored() {
        // 允许用户在状态文件里加自己的注释段（不至于因此挂载失败）。
        let raw = "\
[routes]
cad = 2228

[my_notes]
why = \"留给下一个人\"
";
        assert_eq!(parse(raw).unwrap().get("cad"), Some(&2228));
    }
}
