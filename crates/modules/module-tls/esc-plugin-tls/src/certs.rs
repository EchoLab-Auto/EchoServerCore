//! 证书加载与 SNI 选择。
//!
//! # 与 nginx 的一处有意差异
//!
//! 原配置里每个 `server` 块自带一对 `ssl_certificate` / `ssl_certificate_key`，
//! 于是「哪个名字用哪张证书」是按**端口 × 名字**决定的。这里改成**全局按 SNI 选一张**：
//!
//! ```text
//! 客户端 SNI = cloud.tx.evencewu.asia  →  [[cert]] 里 sni 列表命中的那张
//! 其它 / 没有 SNI                       →  defaults.cert
//! ```
//!
//! 为什么可以这样：原配置里的两张证书分工是「真证书只服务一个域名，自签兜底」。
//! 全局选一张在**所有**端口上都给出同样的答案，而原来 2223/2226 上那些块只声明了
//! 自签 —— 用域名访问它们时浏览器会警告一次。合并之后那次警告没有了，这不是退步。
//!
//! 代价：将来若出现「同一个域名在不同端口要用不同证书」的需求（客户端证书分流之类），
//! 这个模型表达不了，得重新引入按站点的证书绑定。写在这里是为了那天能一眼看到边界。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rustls::crypto::ring as provider;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;

use crate::config::CertConfig;

/// 加载好的证书集合。
pub struct CertStore {
    /// 名字 → 可用证书。`HashMap` 而不是有序表：这里只按名字查。
    by_name: HashMap<String, Arc<CertifiedKey>>,
    /// SNI（已小写）→ 名字。
    sni_index: HashMap<String, String>,
    /// SNI 没匹配上时用哪张。
    default: Option<String>,
}

impl CertStore {
    /// 读盘并构建。
    ///
    /// 任何一张证书读失败都整体失败（fail-closed）：一个「大部分端口能用、某个
    /// 域名握手失败」的部署，排查成本远高于启动时报一行错。
    pub fn load(certs: &[CertConfig], default: &Option<String>) -> Result<Self> {
        let mut by_name = HashMap::new();
        let mut sni_index = HashMap::new();

        for c in certs {
            let key = load_certified_key(&c.cert, &c.key).with_context(|| {
                format!("证书 {:?}（{} / {}）加载失败", c.name, c.cert.display(), c.key.display())
            })?;

            for name in &c.sni {
                let normalized = name.trim().to_ascii_lowercase();
                if normalized.is_empty() {
                    continue;
                }
                // 同一个 SNI 名字被两张证书声明 = 配置写重了。谁生效取决于遍历
                // 顺序，那是最糟的一类不确定。
                if let Some(prev) = sni_index.insert(normalized.clone(), c.name.clone()) {
                    bail!("SNI 名字 {normalized:?} 同时被证书 {prev:?} 和 {:?} 声明", c.name);
                }
            }

            by_name.insert(c.name.clone(), Arc::new(key));
        }

        Ok(Self {
            by_name,
            sni_index,
            default: default.clone(),
        })
    }

    /// 按 SNI 选证书。`None` = 没有可用的（实际不会发生：`validate` 保证了
    /// 至少有一张，且 `default` 指向存在的名字）。
    pub fn resolve(&self, sni: Option<&str>) -> Option<Arc<CertifiedKey>> {
        if let Some(name) = sni {
            let key = name.trim().to_ascii_lowercase();
            if let Some(cert_name) = self.sni_index.get(&key) {
                if let Some(k) = self.by_name.get(cert_name) {
                    return Some(Arc::clone(k));
                }
            }
        }
        self.default
            .as_ref()
            .and_then(|n| self.by_name.get(n))
            .cloned()
    }

    pub fn names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.by_name.keys().map(String::as_str).collect();
        v.sort_unstable();
        v
    }

    /// SNI 名字清单（小写，已排序）。用于启动日志：一眼看出「域名访问会拿到哪张证书」。
    pub fn sni_names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.sni_index.keys().map(String::as_str).collect();
        v.sort_unstable();
        v
    }
}

impl std::fmt::Debug for CertStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印私钥相关内容（`CertifiedKey` 的 Debug 会很长且没用）
        f.debug_struct("CertStore")
            .field("certs", &self.names())
            .field("sni", &self.sni_names())
            .field("default", &self.default)
            .finish()
    }
}

/// 把证书 + 私钥装配成 `CertifiedKey`。
fn load_certified_key(cert: &Path, key: &Path) -> Result<CertifiedKey> {
    let certs = esc_common::tls::load_certs(cert)?;
    let key_der = esc_common::tls::load_key(key)?;

    // `any_supported_type` 认 PKCS#8 / PKCS#1 / SEC1，且会检查曲线与算法是否在
    // 当前 provider 的支持范围内。比按扩展名猜类型可靠。
    let signing = provider::sign::any_supported_type(&key_der)
        .with_context(|| format!("私钥 {} 的算法不被支持", key.display()))?;

    Ok(CertifiedKey::new(certs, signing))
}

/// TLS 握手用的 `ServerConfig`：证书按 SNI 动态选。
pub fn server_config(store: Arc<CertStore>, tls12: bool) -> Result<Arc<ServerConfig>> {
    esc_common::tls::install_crypto_provider();

    let resolver = SniResolver {
        store: Arc::clone(&store),
    };

    // TLS 1.2 留不留是配置项（默认留，见 `Defaults::tls12` 的注释）。所以版本集合
    // 不能写死成 `with_safe_default_protocol_versions`，得显式传。
    let versions: &[&rustls::SupportedProtocolVersion] = if tls12 {
        &[&rustls::version::TLS12, &rustls::version::TLS13]
    } else {
        &[&rustls::version::TLS13]
    };

    let mut config = ServerConfig::builder_with_provider(Arc::new(provider::default_provider()))
        .with_protocol_versions(versions)
        .context("rustls 协议版本配置失败")?
        .with_no_client_auth()
        // `with_cert_resolver` 才是「证书从哪来」的出口。直接给 `cert_resolver`
        // 字段赋值不行 —— `with_no_client_auth()` 返回的是还没定证书的 builder。
        .with_cert_resolver(Arc::new(resolver));

    // **只宣告 http/1.1**。
    //
    // 不宣告 ALPN 时浏览器也会用 HTTP/1.1，但会先试 h2 —— 我们看到的是一个大号的
    // 未知协议头。明确只给 http/1.1 让客户端一开始就不往 h2 想。这个终结器不做
    // HTTP/2（与 esc-gateway 的取舍一致：多路复用对面板没有收益，却要引入一套
    // 帧编解码）。
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(Arc::new(config))
}

/// 按 `ClientHello` 里的 SNI 挑证书。
struct SniResolver {
    store: Arc<CertStore>,
}

impl std::fmt::Debug for SniResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SniResolver")
    }
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.store.resolve(hello.server_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 生成一对自签证书写到临时目录。用 rcgen 是为了测试不依赖仓库里有没有证书。
    fn write_self_signed(dir: &Path, cn: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let cert = rcgen::generate_simple_self_signed(vec![cn.to_string()]).unwrap();
        let cert_path = dir.join(format!("{cn}.crt"));
        let key_path = dir.join(format!("{cn}.key"));
        let mut f = std::fs::File::create(&cert_path).unwrap();
        f.write_all(cert.cert.pem().as_bytes()).unwrap();
        let mut f = std::fs::File::create(&key_path).unwrap();
        // rcgen 0.14 的字段是 `signing_key`（旧版叫 `key_pair`），类型实现了
        // `SigningKey::serialize_pem()`。
        f.write_all(cert.signing_key.serialize_pem().as_bytes()).unwrap();
        (cert_path, key_path)
    }

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "esc-tls-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn sni_selects_the_named_cert_and_falls_back_to_default() {
        let dir = tempdir("sni");
        let (c1, k1) = write_self_signed(&dir, "fallback");
        let (c2, k2) = write_self_signed(&dir, "real");

        let store = CertStore::load(
            &[
                CertConfig {
                    name: "self".into(),
                    cert: c1,
                    key: k1,
                    sni: vec![],
                },
                CertConfig {
                    name: "real".into(),
                    cert: c2,
                    key: k2,
                    // 大小写混着写，验证归一化
                    sni: vec!["Cloud.TX.evencewu.asia".into()],
                },
            ],
            &Some("self".into()),
        )
        .expect("应能加载");

        assert!(store.resolve(Some("cloud.tx.evencewu.asia")).is_some());
        assert!(store.resolve(Some("CLOUD.TX.EVENCEWU.ASIA")).is_some(), "匹配要不分大小写");
        // 未命中的名字走默认，而不是「握手失败」
        assert!(store.resolve(Some("nope.example")).is_some());
        assert!(store.resolve(None).is_some());

        assert_eq!(store.names(), vec!["real", "self"]);
        assert_eq!(store.sni_names(), vec!["cloud.tx.evencewu.asia"]);
    }

    /// 同一个 SNI 名字被两张证书声明 → 谁生效取决于遍历顺序，必须报错。
    #[test]
    fn duplicate_sni_across_certs_is_rejected() {
        let dir = tempdir("dupsni");
        let (c1, k1) = write_self_signed(&dir, "a");
        let (c2, k2) = write_self_signed(&dir, "b");
        let err = CertStore::load(
            &[
                CertConfig {
                    name: "one".into(),
                    cert: c1,
                    key: k1,
                    sni: vec!["x.example".into()],
                },
                CertConfig {
                    name: "two".into(),
                    cert: c2,
                    key: k2,
                    sni: vec!["x.example".into()],
                },
            ],
            &Some("one".into()),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("同时被"), "{err}");
    }

    #[test]
    fn missing_cert_file_fails_loudly() {
        let err = CertStore::load(
            &[CertConfig {
                name: "gone".into(),
                cert: "/definitely/not/here.crt".into(),
                key: "/definitely/not/here.key".into(),
                sni: vec![],
            }],
            &Some("gone".into()),
        )
        .unwrap_err()
        .to_string();
        // 错误信息要带上是哪张、哪个文件 —— 否则 12 张证书里找一张缺的很难
        assert!(err.contains("gone"), "{err}");
        assert!(err.contains("not/here"), "{err}");
    }

    #[test]
    fn tls13_only_config_builds() {
        let dir = tempdir("tls13");
        let (c, k) = write_self_signed(&dir, "only13");
        let store = Arc::new(
            CertStore::load(
                &[CertConfig {
                    name: "s".into(),
                    cert: c,
                    key: k,
                    sni: vec![],
                }],
                &Some("s".into()),
            )
            .unwrap(),
        );
        server_config(store, false).expect("只留 TLS 1.3 也要能构出配置");
    }

    #[test]
    fn tls12_and_tls13_config_builds() {
        let dir = tempdir("both");
        let (c, k) = write_self_signed(&dir, "both");
        let store = Arc::new(
            CertStore::load(
                &[CertConfig {
                    name: "s".into(),
                    cert: c,
                    key: k,
                    sni: vec![],
                }],
                &Some("s".into()),
            )
            .unwrap(),
        );
        server_config(store, true).expect("TLS 1.2 + 1.3 也要能构出配置");
    }
}
