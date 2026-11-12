//! 主机内部的**隔离网段**：声明、归属校验、可用性自检与拓扑报告。
//!
//! # 它要修的那个旁路
//!
//! 没有隔离网段时，网关的前门只能写在可达地址上。cloud2 上原本是这样：
//!
//! ```text
//! 公网 ──▶ esc-host（TLS 终结 :443）──▶ 0.0.0.0:18784（网关）──▶ 127.0.0.1:18788
//!                      └──────────────▶ 公网也可以直连 :18784，绕过 TLS
//! ```
//!
//! 第二条箭头就是旁路：证书、SNI 选择、明文策略在这个端口上全部不生效。
//! 它之所以"没事"，只是因为没人知道端口——而这不是一个安全属性。
//!
//! 隔离网段把这条旁路从"没人知道"降级为"结构上不存在"：
//!
//! ```text
//! 公网 ──▶ TLS 终结 :443 ──▶ 隔离网段 127.213.0.1:18801 ──▶ 认证 ──▶ 127.0.0.1:18788
//! ```
//!
//! 网关只在这个网段的地址上开监听；从公网接口进来、目的地址是网段地址的包，
//! 根本到不了套接字。要进认证后的上游，唯一的路径是先过 TLS 终结。
//!
//! # 为什么默认落在 127.0.0.0/8
//!
//! 内核已经把整段 127/8 配在 `lo` 上：`127.213.0.1` 这类地址不需要任何权限、
//! 不需要创建设备就能 bind。而两件事共同保证它外部不可达：
//!
//! * 任何路由器都不转发 127/8（RFC 6890）；
//! * 本机 `route_localnet = 0`（默认）时，从**非 lo 接口**收上来的
//!   127/8 目的地址包按 martian 丢弃。
//!
//! 也就是说，隔离性来自地址族语义，而不是一条可能被改错、被 flush 掉的防火墙
//! 规则。少一条能配错的东西，就少一次事故。
//!
//! # 需要独立设备的场景（`kind = link`）
//!
//! 非 127/8 的私有网段（`10.213.0.0/24` 之类，挂在 dummy/bridge 上）也支持。
//! 但网关**只校验、不创建**：它跑在容器里，既没有 NET_ADMIN 也没有 iproute2，
//! 假装能建设备才是危险的做法。地址不存在就在启动时报错，并把该敲的命令打出来
//! ——宁可起不来，也不要起一个"监听在公网地址上、但以为自己在网段里"的网关。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use serde::Deserialize;

/// 配置里的一个网段声明（`[[segment]]`）。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SegmentConfig {
    /// 网段名。路由靠它归属（`segment = "edge"`）。
    pub name: String,
    /// 网段，IPv4 CIDR，如 `127.213.0.0/16`。
    pub cidr: String,
    /// 给人看的备注。会出现在 `check` 与面板里，不参与任何判定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 网段的两条实现路径。差别只在"地址从哪来"，不在"谁能连上"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// 127.0.0.0/8 之内的地址：内核已经配在 `lo` 上，零权限可用，
    /// 外部不可达由内核保证。
    Loopback,
    /// 其它私有网段：要求本机已经把那块地址配在某个设备上（dummy/bridge）。
    /// 网关只校验存在性，不创建设备。
    Link,
}

impl SegmentKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Loopback => "loopback（内核保证外部不可达）",
            Self::Link => "link（要求本机已有该地址）",
        }
    }
}

/// 一个解析好的网段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub name: String,
    /// 归一化后的 CIDR 文本（`127.213.0.5/16` → `127.213.0.0/16`）。
    cidr: String,
    /// 已按掩码归零的基地址。
    base: u32,
    prefix: u8,
    pub kind: SegmentKind,
    pub note: Option<String>,
}

impl Segment {
    pub fn parse(cfg: &SegmentConfig) -> Result<Self, String> {
        if !crate::config::valid_name(&cfg.name) {
            return Err(format!(
                "segment 名 {:?} 不合法：只能用字母、数字、`.` `-` `_`，长度 1..=64",
                cfg.name
            ));
        }
        let (base, prefix) = parse_cidr(&cfg.cidr)
            .map_err(|e| format!("segment {:?} 的 cidr: {e}", cfg.name))?;
        // 127.0.0.0/8 的第一个字节固定是 127。用它判"是不是回环族"，
        // 不需要再和 127/8 做一遍区间运算。
        let kind = if base >> 24 == 127 {
            SegmentKind::Loopback
        } else {
            SegmentKind::Link
        };
        Ok(Self {
            name: cfg.name.clone(),
            cidr: format!("{}/{}", Ipv4Addr::from(base), prefix),
            base,
            prefix,
            kind,
            note: cfg.note.clone(),
        })
    }

    pub fn cidr(&self) -> &str {
        &self.cidr
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// 网段的基地址（`127.213.0.0/16` → `127.213.0.0`）。
    pub fn base(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.base)
    }

    /// 自检用的地址：基地址 +1。
    ///
    /// 避开基地址本身是因为它常被当成"网段标识"写在文档和路由表里，
    /// 而 `/31`、`/32` 太窄，只能退回基地址。
    pub fn probe_addr(&self) -> Ipv4Addr {
        if self.prefix >= 31 {
            self.base()
        } else {
            Ipv4Addr::from(self.base + 1)
        }
    }

    /// 地址是否属于本网段。IPv6 一律不属于（网段只支持 IPv4）。
    pub fn contains(&self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => (u32::from(v4) & self.mask()) == self.base,
            IpAddr::V6(_) => false,
        }
    }

    fn mask(&self) -> u32 {
        // prefix == 0 在 parse 阶段已经被拒，这里不必再防左移 32 位。
        u32::MAX << (32 - self.prefix as u32)
    }

    /// 两个网段是否重叠。
    ///
    /// 重叠必须拒：一个地址同时属于两个网段时，"这条路由归谁管"就退化成
    /// "谁先出现在配置里"，那是最难查的一类问题。
    fn overlaps(&self, other: &Segment) -> bool {
        let p = self.prefix.min(other.prefix);
        let mask = u32::MAX << (32 - p as u32);
        (self.base & mask) == (other.base & mask)
    }

    /// 网段地址在本机是否真的可用（bind 一次端口 0 就够）。
    ///
    /// 同步实现：只在启动和 `check`/`segment` 子命令里各调一次，耗时是微秒级，
    /// 为它引一层 async 包装没有收益。
    pub fn probe(&self) -> Result<(), String> {
        let addr = SocketAddr::new(IpAddr::V4(self.probe_addr()), 0);
        match std::net::TcpListener::bind(addr) {
            Ok(l) => {
                // 立刻释放：这里只想知道"能不能绑"，不想占着。
                drop(l);
                Ok(())
            }
            Err(e) => Err(format!(
                "网段 {:?}（{}）的地址 {} 在本机不可用：{e}\n      {}",
                self.name,
                self.cidr,
                self.probe_addr(),
                self.missing_addr_hint()
            )),
        }
    }

    fn missing_addr_hint(&self) -> String {
        match self.kind {
            SegmentKind::Loopback => "127/8 整段都在 lo 上，绑不上通常说明本进程没有共享主机网络\
                 （容器要 --network host，否则自己的 lo 是另一份）"
                .to_string(),
            SegmentKind::Link => format!(
                "先在本机把这块地址建出来再启动网关，例如：\n      \
                 ip link add escseg0 type dummy\n      \
                 ip addr add {}/{} dev escseg0\n      \
                 ip link set escseg0 up",
                self.base(),
                self.prefix
            ),
        }
    }
}

impl std::fmt::Display for Segment {
    /// 写成 `名字（cidr）`：报错、日志、面板里都是这个形态，
    /// 省得每处都自己拼一遍（拼法不一致时最难对着日志找配置）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}（{}）", self.name, self.cidr)
    }
}

/// 一组网段（配置里可以有多个，彼此不许重叠）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SegmentSet {
    list: Vec<Segment>,
}

impl SegmentSet {
    /// 空集合：没写 `[[segment]]` 时的形态，行为与加这个特性之前完全一致。
    pub fn empty() -> Self {
        Self { list: Vec::new() }
    }

    pub fn parse(cfgs: &[SegmentConfig]) -> Result<Self, String> {
        let mut list: Vec<Segment> = Vec::with_capacity(cfgs.len());
        for c in cfgs {
            let seg = Segment::parse(c)?;
            if let Some(prev) = list.iter().find(|s| s.name == seg.name) {
                return Err(format!(
                    "segment 名 {:?} 重复（一个名字只能指一个网段：{prev} 与 {seg}）",
                    seg.name
                ));
            }
            if let Some(prev) = list.iter().find(|s| s.overlaps(&seg)) {
                return Err(format!(
                    "segment {:?}（{}）与 {:?}（{}）重叠：一个地址不能同时属于两个网段",
                    seg.name, seg.cidr, prev.name, prev.cidr
                ));
            }
            list.push(seg);
        }
        Ok(Self { list })
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn all(&self) -> &[Segment] {
        &self.list
    }

    pub fn get(&self, name: &str) -> Option<&Segment> {
        self.list.iter().find(|s| s.name == name)
    }

    pub fn names(&self) -> Vec<String> {
        self.list.iter().map(|s| s.name.clone()).collect()
    }

    /// 这个地址落在哪个网段里。归属校验与"网段内私接监听"的拦截都靠它。
    pub fn find_for_ip(&self, ip: IpAddr) -> Option<&Segment> {
        self.list.iter().find(|s| s.contains(ip))
    }
}

/// `127.213.0.0/16` → `(已归零的基地址, 前缀长度)`。
///
/// 只认 IPv4：IPv6 的网段语义（链路本地、唯一本地、内嵌 IPv4）太多，写个半吊子
/// 的判定比不支持更危险——和 `AllowRule` 的口径一致。
fn parse_cidr(text: &str) -> Result<(u32, u8), String> {
    let text = text.trim();
    let (addr, len) = text
        .rsplit_once('/')
        .ok_or_else(|| format!("网段 {text:?} 缺少前缀长度（形如 127.213.0.0/16）"))?;
    let addr: Ipv4Addr = addr
        .trim()
        .parse()
        .map_err(|_| format!("网段 {text:?} 的地址部分不是 IPv4（网段只支持 IPv4）"))?;
    let prefix: u8 = len
        .trim()
        .parse()
        .map_err(|_| format!("网段 {text:?} 的前缀长度不是数字"))?;
    if prefix > 32 {
        return Err(format!("网段 {text:?} 的前缀长度超过 32"));
    }
    if prefix == 0 {
        return Err(format!(
            "网段 {text:?} 的前缀长度是 0（等于整个 IPv4 空间），不能当隔离网段"
        ));
    }
    let mask = u32::MAX << (32 - prefix as u32);
    Ok((u32::from(addr) & mask, prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str, cidr: &str) -> SegmentConfig {
        SegmentConfig {
            name: name.into(),
            cidr: cidr.into(),
            note: None,
        }
    }

    #[test]
    fn loopback_segment_is_recognised_and_normalised() {
        let s = Segment::parse(&cfg("edge", "127.213.0.5/16")).unwrap();
        assert_eq!(s.cidr(), "127.213.0.0/16");
        assert_eq!(s.kind, SegmentKind::Loopback);
        assert_eq!(s.probe_addr(), Ipv4Addr::new(127, 213, 0, 1));
    }

    #[test]
    fn private_segment_is_link_kind() {
        let s = Segment::parse(&cfg("dmz", "10.213.0.0/24")).unwrap();
        assert_eq!(s.kind, SegmentKind::Link);
        assert_eq!(s.base(), Ipv4Addr::new(10, 213, 0, 0));
    }

    #[test]
    fn contains_respects_prefix_boundaries() {
        let s = Segment::parse(&cfg("edge", "127.213.0.0/24")).unwrap();
        assert!(s.contains("127.213.0.1".parse().unwrap()));
        assert!(s.contains("127.213.0.254".parse().unwrap()));
        assert!(!s.contains("127.213.1.1".parse().unwrap()));
        assert!(!s.contains("::1".parse().unwrap()));
    }

    #[test]
    fn bad_cidrs_are_rejected() {
        for bad in ["127.213.0.1", "127.213.0.0/33", "127.213.0.0/0", "edge/16"] {
            let err = Segment::parse(&cfg("edge", bad)).unwrap_err();
            assert!(err.contains("cidr"), "{bad} → {err}");
        }
    }

    #[test]
    fn bad_names_are_rejected() {
        let err = Segment::parse(&cfg("edge/1", "127.213.0.0/16")).unwrap_err();
        assert!(err.contains("不合法"), "{err}");
    }

    #[test]
    fn duplicate_and_overlapping_segments_are_rejected() {
        let err = SegmentSet::parse(&[
            cfg("edge", "127.213.0.0/16"),
            cfg("edge", "127.214.0.0/16"),
        ])
        .unwrap_err();
        assert!(err.contains("重复"), "{err}");

        let err = SegmentSet::parse(&[
            cfg("edge", "127.213.0.0/16"),
            cfg("edge2", "127.213.55.0/24"),
        ])
        .unwrap_err();
        assert!(err.contains("重叠"), "{err}");
    }

    #[test]
    fn disjoint_segments_are_fine_and_lookup_works() {
        let set = SegmentSet::parse(&[
            cfg("edge", "127.213.0.0/16"),
            cfg("dmz", "10.213.0.0/24"),
        ])
        .unwrap();
        assert_eq!(set.len(), 2);
        let hit = set.find_for_ip("127.213.9.9".parse().unwrap()).unwrap();
        assert_eq!(hit.name, "edge");
        assert!(set.find_for_ip("127.0.0.1".parse().unwrap()).is_none());
        assert!(set.find_for_ip("10.213.0.7".parse().unwrap()).is_some());
    }

    /// 回环网段在本机一定能绑——这条测试顺带证明了"默认方案零权限可用"。
    #[test]
    fn loopback_probe_succeeds_on_this_host() {
        let s = Segment::parse(&cfg("edge", "127.213.0.0/16")).unwrap();
        s.probe().expect("127/8 的地址应当可直接绑定");
    }

    /// 未配置的设备地址绑不上：必须报错，且错误里要给出该敲的命令。
    #[test]
    fn missing_link_address_fails_with_a_recipe() {
        let s = Segment::parse(&cfg("dmz", "10.213.99.0/24")).unwrap();
        let err = match s.probe() {
            Ok(()) => return, // 这台机器上真配了 10.213.99.1，跳过
            Err(e) => e,
        };
        assert!(err.contains("ip link add"), "{err}");
    }
}
