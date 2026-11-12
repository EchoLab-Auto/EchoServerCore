//! TLS 终结插件的配置。
//!
//! # 为什么配置长这个样子
//!
//! 它要接替的是一份 nginx 配置，而那份配置的结构是「一张证书表 + 若干 server 块」。
//! 所以这里保留同样的两段式：
//!
//! ```toml
//! [[cert]]                       # 证书从哪来、服务哪些名字
//! name = "self-signed"
//! cert = "/etc/ssl/esc/esc.crt"
//! key  = "/etc/ssl/esc/esc.key"
//!
//! [[site]]                       # 哪个端口、转发到哪
//! name     = "panel"
//! listen   = "0.0.0.0:2222"
//! upstream = "127.0.0.1:18790"
//! ```
//!
//! 与 nginx 的差异（都是**有意**的，写在 [`crate`] 的模块注释里）：证书按 SNI 全局
//! 选一张，不按 server 块各配一份。见 `certs.rs` 的说明。

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

/// 一整份模块配置。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsModuleConfig {
    /// 全局默认值。省略时用 [`Defaults::default`]。
    #[serde(default)]
    pub defaults: Defaults,
    /// 可用证书。
    #[serde(default)]
    pub cert: Vec<CertConfig>,
    /// 监听端口 + 转发规则。
    #[serde(default)]
    pub site: Vec<SiteConfig>,
}

/// 全局默认值。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// SNI 没匹配上时用哪张证书（按 `[[cert]].name` 引用）。
    ///
    /// 省略 = 用第一张。一份证书都没有时挂载直接失败 —— 一个「端口开着但握手
    /// 永远失败」的 TLS 终结器比没起来更难查。
    #[serde(default)]
    pub cert: Option<String>,
    /// 允许 TLS 1.2。默认 `true`，与 nginx 那份配置一致。
    ///
    /// 关掉只留 1.3 更安全，但会让一批老客户端（老 OpenSSL、部分嵌入式 curl）
    /// 直接连不上。真关之前先确认没有那种客户端在跑。
    #[serde(default = "yes")]
    pub tls12: bool,
    /// 请求体上限，字节。默认 12 MiB，与 nginx 的 `client_max_body_size 12m` 对齐。
    #[serde(default = "default_body_limit")]
    pub max_body_bytes: u64,
    /// 握手超时，秒。默认 10。
    ///
    /// 没有它，一个只连不发数据的客户端就能一直占着握手过程中的资源（slowloris）。
    #[serde(default = "default_handshake_timeout")]
    pub handshake_timeout_secs: u64,
    /// 是否在响应里加 `Strict-Transport-Security`。**默认关**。
    ///
    /// HSTS 是按**主机名**记的、不区分端口。在非标准端口上发 HSTS，浏览器会把整个
    /// 主机名标成 HTTPS-only 并改写到 **443**；443 没通的话结果就是访问超时，
    /// 页面根本加载不出来。原 nginx 配置里也留着同样的警告，这里照抄这个取舍。
    #[serde(default)]
    pub hsts: bool,
}

impl Default for Defaults {
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

fn yes() -> bool {
    true
}

fn default_body_limit() -> u64 {
    12 * 1024 * 1024
}

fn default_handshake_timeout() -> u64 {
    10
}

/// 一张证书。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertConfig {
    /// 引用名，`defaults.cert` 与 `site.cert` 用它指过来。
    pub name: String,
    /// PEM 证书链（叶子在前，中间证书在后）。就是 nginx 的 `ssl_certificate`。
    pub cert: PathBuf,
    /// PEM 私钥。就是 nginx 的 `ssl_certificate_key`。
    pub key: PathBuf,
    /// 这张证书服务哪些 SNI 名字。空 = 只作为默认证书用，不参与按名字匹配。
    ///
    /// 匹配是**大小写不敏感的全等**，不做通配 —— 通配（`*.example.com`）会让
    /// 「哪个名字用哪张证书」变成一件要推理的事，而这里只有两三个名字。
    #[serde(default)]
    pub sni: Vec<String>,
}

/// 一个监听端口。
///
/// `name` 必须唯一：它出现在日志、错误信息、以及（将来）控制面的标识里。
/// 不用端口号当标识是因为端口会变，而日志里一句 `site=panel` 比
/// `site=0.0.0.0:2222` 好认。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteConfig {
    pub name: String,
    /// 监听地址，如 `0.0.0.0:2222`。可以是单个字符串，也可以是数组
    /// （同一个 site 挂多个端口，例如 443 与 2224 同一个后端）。
    #[serde(deserialize_with = "string_or_vec")]
    pub listen: Vec<String>,
    /// 明文转发目标，如 `127.0.0.1:18790`。与 `redirect` 二选一。
    #[serde(default)]
    pub upstream: Option<String>,
    /// 301 跳转目标模板，如 `https://{host}:2224{uri}`。与 `upstream` 二选一。
    ///
    /// 占位符：`{host}` = Host 头原样（含端口），`{hostname}` = 去掉端口的 Host，
    /// `{uri}` = 路径 + 查询串，`{path}` = 只有路径。
    ///
    /// 要「跳到另一个端口」时用 `{hostname}` —— nginx 的 `$host` 不含端口，
    /// 而 `{host}` 是**含**端口的（对应 nginx 的 `$http_host`）。写成
    /// `https://{host}:2224{uri}` 会拼出 `https://x:2223:2224/…`。
    #[serde(default)]
    pub redirect: Option<String>,
    /// 透传 WebSocket 升级。默认 `false` —— 显式打开，因为「能升级」意味着这条
    /// 链路可以被用来跑任意长连接，不该是默认行为。
    #[serde(default)]
    pub websocket: bool,
    /// 是否把查询串写进访问日志。默认 `false`。
    ///
    /// 面板的令牌走 `?token=`，原样记下来等于把凭据落到磁盘。代价是丢了查询参数
    /// —— 对一个承载令牌的端口来说这个交换值得（原 nginx 配置里有个专门的
    /// `log_format scrubbed` 做同一件事）。
    #[serde(default)]
    pub log_query: bool,
    /// 等上游响应的超时，秒。默认 300。WebSocket 站点通常要更大（3600）。
    #[serde(default = "default_upstream_timeout")]
    pub read_timeout_secs: u64,
    /// 这个端口的请求体上限，覆盖 `defaults.max_body_bytes`。
    #[serde(default)]
    pub max_body_bytes: Option<u64>,
    /// 这个端口是否发 HSTS，覆盖 `defaults.hsts`。
    #[serde(default)]
    pub hsts: Option<bool>,
}

fn default_upstream_timeout() -> u64 {
    300
}

/// `listen = "0.0.0.0:1"` 与 `listen = ["0.0.0.0:1", "0.0.0.0:2"]` 都接受。
fn string_or_vec<'de, D>(d: D) -> Result<Vec<String>, D::Error>
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

// ─────────────────────────────────────────────────────────────────────────────
// 校验后的运行时形态
// ─────────────────────────────────────────────────────────────────────────────

/// 一个校验通过的监听端口。
#[derive(Debug, Clone)]
pub struct Site {
    pub name: String,
    pub addr: SocketAddr,
    pub action: Action,
    pub websocket: bool,
    pub log_query: bool,
    pub read_timeout: std::time::Duration,
    pub max_body_bytes: u64,
    pub hsts: bool,
}

/// 端口收到请求之后做什么。
#[derive(Debug, Clone)]
pub enum Action {
    /// 明文转发到这个地址。
    Proxy(SocketAddr),
    /// 301 到模板展开后的地址。
    Redirect(String),
}

impl TlsModuleConfig {
    /// 解析 + 校验，一次把所有问题都报出来。
    ///
    /// **一次报完而不是遇到第一个就返回**：配置错通常是一批（复制粘贴改了一半），
    /// 一条一条试很浪费时间。
    pub fn validate(&self) -> Result<Validated, Vec<String>> {
        let mut errors = Vec::new();

        if self.cert.is_empty() {
            errors.push("至少要在 [[cert]] 里配一张证书，否则端口起来也握手不了".into());
        }
        if self.site.is_empty() {
            errors.push("至少要在 [[site]] 里配一个监听端口".into());
        }

        // 证书名唯一
        let mut cert_names = BTreeMap::new();
        for c in &self.cert {
            if c.name.trim().is_empty() {
                errors.push("[[cert]] 的 name 不能为空".into());
                continue;
            }
            if cert_names.insert(c.name.clone(), ()).is_some() {
                errors.push(format!("证书名 {:?} 重复", c.name));
            }
        }

        // 默认证书：显式指定的必须存在；没指定时退回第一张
        let default_cert = match &self.defaults.cert {
            Some(n) if !cert_names.contains_key(n) => {
                errors.push(format!(
                    "defaults.cert = {n:?} 没有对应证书。可用的：{}",
                    keys(&cert_names)
                ));
                None
            }
            Some(n) => Some(n.clone()),
            None => self.cert.first().map(|c| c.name.clone()),
        };

        // 站点名唯一 + 监听地址不重复（同一个地址配两遍是典型的复制粘贴残留，
        // 第二个 bind 一定失败，但错误信息会指向"地址被占用"，误导性很强）
        let mut site_names = BTreeMap::new();
        let mut addrs: BTreeMap<SocketAddr, String> = BTreeMap::new();
        let mut sites = Vec::new();

        for s in &self.site {
            if s.name.trim().is_empty() {
                errors.push("[[site]] 的 name 不能为空".into());
                continue;
            }
            if site_names.insert(s.name.clone(), ()).is_some() {
                errors.push(format!("site 名 {:?} 重复", s.name));
            }

            let action = match (&s.upstream, &s.redirect) {
                (Some(_), Some(_)) => {
                    errors.push(format!(
                        "site {:?}: upstream 与 redirect 只能配一个",
                        s.name
                    ));
                    continue;
                }
                (None, None) => {
                    errors.push(format!(
                        "site {:?}: 要配 upstream（转发）或 redirect（跳转），两个都空等于什么都不做",
                        s.name
                    ));
                    continue;
                }
                (Some(u), None) => match u.parse::<SocketAddr>() {
                    Ok(a) => Action::Proxy(a),
                    Err(_) => {
                        errors.push(format!(
                            "site {:?}: upstream {u:?} 不是合法 host:port（要能解析成 SocketAddr，\
                             需要用 IP 而不是主机名，例如 127.0.0.1:18790）",
                            s.name
                        ));
                        continue;
                    }
                },
                (None, Some(r)) => {
                    if !r.starts_with("https://") && !r.starts_with("http://") {
                        errors.push(format!(
                            "site {:?}: redirect {r:?} 要以 http:// 或 https:// 开头",
                            s.name
                        ));
                        continue;
                    }
                    Action::Redirect(r.clone())
                }
            };

            if s.websocket && matches!(action, Action::Redirect(_)) {
                errors.push(format!(
                    "site {:?}: redirect 的站点配 websocket 没有意义（301 不会升级）",
                    s.name
                ));
            }

            for raw in &s.listen {
                let Ok(addr) = raw.parse::<SocketAddr>() else {
                    errors.push(format!(
                        "site {:?}: listen {raw:?} 不是合法 host:port（要用 IP，例如 0.0.0.0:2222）",
                        s.name
                    ));
                    continue;
                };
                if let Some(other) = addrs.get(&addr) {
                    errors.push(format!(
                        "监听地址 {addr} 同时配在 site {other:?} 和 {:?} 上，只能有一个",
                        s.name
                    ));
                    continue;
                }
                addrs.insert(addr, s.name.clone());

                sites.push(Site {
                    name: s.name.clone(),
                    addr,
                    action: action.clone(),
                    websocket: s.websocket,
                    log_query: s.log_query,
                    read_timeout: std::time::Duration::from_secs(s.read_timeout_secs.max(1)),
                    max_body_bytes: s.max_body_bytes.unwrap_or(self.defaults.max_body_bytes),
                    hsts: s.hsts.unwrap_or(self.defaults.hsts),
                });
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(Validated {
            default_cert,
            tls12: self.defaults.tls12,
            handshake_timeout: std::time::Duration::from_secs(
                self.defaults.handshake_timeout_secs.max(1),
            ),
            sites,
        })
    }
}

/// [`TlsModuleConfig::validate`] 的产物。证书本体在 `certs.rs` 里加载。
#[derive(Debug, Clone)]
pub struct Validated {
    pub default_cert: Option<String>,
    pub tls12: bool,
    pub handshake_timeout: std::time::Duration,
    pub sites: Vec<Site>,
}

fn keys(map: &BTreeMap<String, ()>) -> String {
    if map.is_empty() {
        "（一个都没有）".to_string()
    } else {
        map.keys().cloned().collect::<Vec<_>>().join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> TlsModuleConfig {
        serde_json::from_str(json).expect("配置应能解析")
    }

    /// 最小可用配置。
    fn minimal() -> TlsModuleConfig {
        parse(
            r#"{
              "cert": [{"name":"self","cert":"/a.crt","key":"/a.key"}],
              "site": [{"name":"p","listen":"127.0.0.1:1","upstream":"127.0.0.1:2"}]
            }"#,
        )
    }

    #[test]
    fn minimal_config_validates_and_defaults_apply() {
        let v = minimal().validate().expect("应通过");
        assert_eq!(v.default_cert.as_deref(), Some("self"), "没指定时退回第一张");
        assert!(v.tls12, "TLS 1.2 默认开");
        assert_eq!(v.sites.len(), 1);
        assert_eq!(v.sites[0].max_body_bytes, 12 * 1024 * 1024);
        assert!(!v.sites[0].websocket, "WebSocket 默认关");
        assert!(!v.sites[0].log_query, "查询串默认不记");
        assert!(!v.sites[0].hsts, "HSTS 默认关");
    }

    #[test]
    fn listen_accepts_one_or_many() {
        let one = minimal();
        assert_eq!(one.validate().unwrap().sites.len(), 1);
        let many = parse(
            r#"{
              "cert": [{"name":"self","cert":"/a.crt","key":"/a.key"}],
              "site": [{"name":"p","listen":["127.0.0.1:1","127.0.0.1:2"],"upstream":"127.0.0.1:3"}]
            }"#,
        );
        let v = many.validate().expect("应通过");
        assert_eq!(v.sites.len(), 2, "一个 site 挂两个端口应展开成两个");
        assert_eq!(v.sites[0].name, "p");
        assert_eq!(v.sites[1].name, "p");
    }

    #[test]
    fn duplicate_listen_address_is_reported() {
        let cfg = parse(
            r#"{
              "cert": [{"name":"self","cert":"/a.crt","key":"/a.key"}],
              "site": [
                {"name":"a","listen":"127.0.0.1:9","upstream":"127.0.0.1:1"},
                {"name":"b","listen":"127.0.0.1:9","upstream":"127.0.0.1:2"}
              ]
            }"#,
        );
        let errs = cfg.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("同时配在")),
            "重复地址要报出来：{errs:?}"
        );
    }

    #[test]
    fn upstream_and_redirect_are_exclusive() {
        let cfg = parse(
            r#"{
              "cert": [{"name":"self","cert":"/a.crt","key":"/a.key"}],
              "site": [{"name":"p","listen":"127.0.0.1:1","upstream":"127.0.0.1:2","redirect":"https://x/"}]
            }"#,
        );
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("只能配一个")), "{errs:?}");
    }

    /// 两个都不配 = 端口开着但什么都不做，这几乎一定是写漏了。
    #[test]
    fn site_without_target_is_rejected() {
        let cfg = parse(
            r#"{
              "cert": [{"name":"self","cert":"/a.crt","key":"/a.key"}],
              "site": [{"name":"p","listen":"127.0.0.1:1"}]
            }"#,
        );
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("什么都不做")), "{errs:?}");
    }

    /// 主机名当 upstream 是常见错误：解析需要 DNS，而这里只接受已解析的 SocketAddr。
    #[test]
    fn upstream_must_be_a_socket_addr() {
        let cfg = parse(
            r#"{
              "cert": [{"name":"self","cert":"/a.crt","key":"/a.key"}],
              "site": [{"name":"p","listen":"127.0.0.1:1","upstream":"localhost:2"}]
            }"#,
        );
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("不是合法 host:port")), "{errs:?}");
    }

    #[test]
    fn unknown_default_cert_is_reported_with_available_names() {
        let cfg = parse(
            r#"{
              "defaults": {"cert": "nope"},
              "cert": [{"name":"self","cert":"/a.crt","key":"/a.key"}],
              "site": [{"name":"p","listen":"127.0.0.1:1","upstream":"127.0.0.1:2"}]
            }"#,
        );
        let errs = cfg.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("self")), "要列出可用的名字：{errs:?}");
    }

    /// 未知键必须报错：`websockt = true` 这种拼写错误被静默忽略，会变成
    /// 「我明明开了 WebSocket 啊」的谜案。
    #[test]
    fn unknown_keys_are_rejected() {
        let err = serde_json::from_str::<TlsModuleConfig>(
            r#"{"site":[{"name":"p","listen":"127.0.0.1:1","upstream":"127.0.0.1:2","websockt":true}]}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("websockt"), "{err}");
    }

    #[test]
    fn per_site_overrides_win_over_defaults() {
        let cfg = parse(
            r#"{
              "defaults": {"max_body_bytes": 100, "hsts": false},
              "cert": [{"name":"self","cert":"/a.crt","key":"/a.key"}],
              "site": [
                {"name":"a","listen":"127.0.0.1:1","upstream":"127.0.0.1:2"},
                {"name":"b","listen":"127.0.0.1:3","upstream":"127.0.0.1:4","max_body_bytes":999,"hsts":true}
              ]
            }"#,
        );
        let v = cfg.validate().unwrap();
        assert_eq!(v.sites[0].max_body_bytes, 100);
        assert!(!v.sites[0].hsts);
        assert_eq!(v.sites[1].max_body_bytes, 999);
        assert!(v.sites[1].hsts);
    }
}
