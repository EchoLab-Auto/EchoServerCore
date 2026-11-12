//! TLS 配置的**运行期状态文件**。
//!
//! # 为什么不是直接改 `host.toml`
//!
//! 面板上「保存」最直觉的做法是回写配置文件，但那会踩三个问题（与
//! `esc.edge-router` 的 `store.rs` 是同一套理由，这里重列是因为它们各自独立
//! 成立 —— 读这一份的人不该被迫去读另一份）：
//!
//! 1. **systemd 沙箱**。`esc-host.service` 有 `ReadOnlyPaths=/etc/esc`，放开它
//!    等于放弃「配置只读」这条边界；
//! 2. **注释与格式**。TOML 解析再序列化会丢掉人写的注释。`host.toml` 里那段
//!    TLS 配置带着大量「为什么这么配」的说明（2223 为什么留 301、HSTS 为什么
//!    关着、证书为什么按 SNI 全局选），被机器重写一遍就全没了；
//! 3. **回滚**。改坏了配置，服务可能起不来；而状态文件坏了，删掉它就回到基线。
//!
//! # 分层
//!
//! ```text
//! host.toml  [tls.config]   基线（只读）—— 首次启动的种子
//! tls.toml                  运行期状态 —— 存在则**整体覆盖**基线
//! ```
//!
//! 「整体覆盖」而不是「合并」：否则删不掉基线里的条目 —— 面板删掉一个站点，
//! 重启后它又从 `host.toml` 里冒出来，而面板上确实没有它。这种不一致最难查。
//!
//! 想回到基线：删掉状态文件 → 重启 `esc-host`。
//!
//! # 原子性
//!
//! 写盘走「同目录临时文件 → rename」。rename 在同一文件系统上是原子的，所以
//! 读者（包括下一个启动的自己）永远看不到写了一半的文件。
//!
//! 文件按 `0600` 保存：它含私钥**路径**（不是私钥本身，但足以指出目标位置）。

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::TlsModuleConfig;

/// 状态文件的头部注释。**每次写盘都重写它** —— 读这个文件的人（多半是半年后
/// 的运维）需要知道：这是谁写的、优先级怎么算、想回滚该怎么办。
const HEADER: &str = "\
# 由 esc.tls 在运行期写入（面板保存 TLS 配置时）。
#
# 优先级：本文件**整体覆盖** host.toml 里的 [tls.config]。
# 回到配置文件的基线：删掉本文件，然后重启 esc-host。
#
# 手改本文件不会立即生效 —— 在面板上再点一次「保存」，或者重启 esc-host。
";

/// 一份 TLS 配置状态文件。
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读状态文件。
    ///
    /// * 文件不存在 → `Ok(None)`（正常：还没人在面板上改过）
    /// * 解析失败 → `Err`。**不静默回落到基线**：一份被手改坏的状态文件会让
    ///   面板显示的东西和实际跑的东西分家，那比启动失败更难查。
    pub fn load(&self) -> Result<Option<TlsModuleConfig>, String> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(format!(
                    "无法读取 TLS 状态文件 {}: {e}\n（删掉它可以回到 host.toml 的基线）",
                    self.path.display()
                ))
            }
        };
        let cfg: TlsModuleConfig = toml::from_str(&raw).map_err(|e| {
            format!(
                "TLS 状态文件 {} 解析失败: {e}\n\
                 （这是面板写进去的文件；删掉它可以回到 host.toml 的基线）",
                self.path.display()
            )
        })?;
        Ok(Some(cfg))
    }

    /// 写状态文件。失败时调用方**已经把配置应用到运行期了** —— 所以这里是
    /// 「持久化失败」而不是「保存失败」，调用方要把这个区别如实告诉用户。
    pub fn save(&self, cfg: &TlsModuleConfig) -> Result<(), String> {
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| write_failed(&self.path, "建目录", e))?;
        }

        let body =
            toml::to_string_pretty(cfg).map_err(|e| format!("TLS 配置无法序列化成 TOML: {e}"))?;
        let text = format!("{HEADER}\n{body}");

        // 临时文件必须与目标同目录：跨文件系统的 rename 会失败（EXDEV）。
        // 代价是这个**目录**必须可写 —— systemd 沙箱下这就是
        // `StateDirectory=` / `ReadWritePaths=` 要覆盖的位置。
        let tmp = self.path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| write_failed(&tmp, "创建", e))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
            }
            f.write_all(text.as_bytes())
                .map_err(|e| write_failed(&tmp, "写入", e))?;
            // fsync 之后再 rename：否则掉电可能留下一个「名字对、内容空」的文件。
            f.sync_all().map_err(|e| write_failed(&tmp, "落盘", e))?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            write_failed(&self.path, "替换", e)
        })?;
        Ok(())
    }
}

fn write_failed(path: &Path, what: &str, e: std::io::Error) -> String {
    let hint = match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            "（目录不可写。systemd 沙箱下要确认 StateDirectory= 覆盖了这个目录）"
        }
        std::io::ErrorKind::NotFound => "（上级目录不存在）",
        _ => "",
    };
    format!("{what} {} 失败: {e}{hint}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CertConfig, Defaults, SiteConfig};

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "esc-tls-store-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample() -> TlsModuleConfig {
        TlsModuleConfig {
            defaults: Defaults {
                cert: Some("self".into()),
                tls12: false,
                max_body_bytes: 2048,
                handshake_timeout_secs: 7,
                hsts: true,
            },
            cert: vec![CertConfig {
                name: "self".into(),
                cert: "/etc/esc/pki/self.crt".into(),
                key: "/etc/esc/pki/self.key".into(),
                sni: vec!["a.example".into(), "b.example".into()],
            }],
            site: vec![
                SiteConfig {
                    name: "panel".into(),
                    listen: vec!["0.0.0.0:2222".into(), "0.0.0.0:2224".into()],
                    upstream: Some("127.0.0.1:18790".into()),
                    redirect: None,
                    websocket: true,
                    log_query: false,
                    read_timeout_secs: 3600,
                    max_body_bytes: None,
                    hsts: None,
                },
                SiteConfig {
                    name: "old".into(),
                    listen: vec!["0.0.0.0:2223".into()],
                    upstream: None,
                    redirect: Some("https://{hostname}:2224{uri}".into()),
                    websocket: false,
                    log_query: false,
                    read_timeout_secs: 300,
                    max_body_bytes: Some(99),
                    hsts: Some(false),
                },
            ],
            store: None,
        }
    }

    /// 文件不存在是**正常情况**（还没人改过），不是错误。
    #[test]
    fn missing_file_is_not_an_error() {
        let d = dir("missing");
        let store = ConfigStore::new(d.join("nope.toml"));
        assert!(store.load().unwrap().is_none());
    }

    /// 写进去再读出来，语义必须一模一样。
    ///
    /// 这条是**这个模块存在的理由**：面板保存后再重启，跑的东西必须还是那一份。
    /// 特别注意两处容易丢语义的地方：`Option` 字段的 `None`（继承默认）不能在
    /// 往返里变成 `Some(默认值)`，`listen` 的多端口不能丢。
    #[test]
    fn roundtrip_preserves_inheritance() {
        let d = dir("roundtrip");
        let store = ConfigStore::new(d.join("tls.toml"));
        let cfg = sample();
        store.save(&cfg).unwrap();

        let back = store.load().unwrap().expect("刚写进去的");
        assert_eq!(back.defaults.max_body_bytes, 2048);
        assert!(!back.defaults.tls12);
        assert!(back.defaults.hsts);
        assert_eq!(back.cert[0].sni, vec!["a.example", "b.example"]);
        assert_eq!(back.site[0].listen.len(), 2);
        // 「继承默认」必须还是 None，不能被写成显式值
        assert_eq!(back.site[0].max_body_bytes, None);
        assert_eq!(back.site[0].hsts, None);
        // 显式指定的也必须还在
        assert_eq!(back.site[1].max_body_bytes, Some(99));
        assert_eq!(back.site[1].hsts, Some(false));
        assert_eq!(back.site[1].upstream, None);
        assert_eq!(
            back.site[1].redirect.as_deref(),
            Some("https://{hostname}:2224{uri}")
        );
    }

    /// 写盘要带上头部说明。半年后打开这个文件的人得知道它是谁写的、
    /// 想回滚该怎么办。
    #[test]
    fn header_explains_priority_and_rollback() {
        let d = dir("header");
        let store = ConfigStore::new(d.join("tls.toml"));
        store.save(&sample()).unwrap();
        let raw = std::fs::read_to_string(store.path()).unwrap();
        assert!(raw.starts_with("# 由 esc.tls"), "{raw}");
        assert!(raw.contains("整体覆盖"), "要写明优先级");
        assert!(raw.contains("删掉本文件"), "要写明怎么回滚");
    }

    /// 目录不存在时自动创建（`/var/lib/esc` 在真实部署里由 StateDirectory 建好，
    /// 但测试环境与手工部署未必）。
    #[test]
    fn creates_missing_directories() {
        let d = dir("mkdir");
        let store = ConfigStore::new(d.join("nested/deeper/tls.toml"));
        store.save(&sample()).unwrap();
        assert!(store.path().exists());
    }

    /// 坏文件必须报错并指出「删掉它能回到基线」—— 静默回落会让面板显示的东西
    /// 和实际跑的东西分家。
    #[test]
    fn corrupt_file_reports_how_to_recover() {
        let d = dir("corrupt");
        let path = d.join("tls.toml");
        std::fs::write(&path, "这不是 toml [[[").unwrap();
        let err = ConfigStore::new(&path).load().unwrap_err();
        assert!(err.contains("删掉它"), "{err}");
    }
}
