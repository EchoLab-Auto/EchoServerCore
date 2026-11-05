//! 两个 Web 面板的静态前端资源：**编译期嵌入**，运行时不需要 Node / CDN / 外网。
//!
//! # 为什么这些资源在一个 crate 里
//!
//! 隧道面板（`:2223`）与服务器面板（`:2224`）的前端来自**同一个 Vite 工程**
//! （`ui/`）——共享 Vue、`@echolab-auto/ui-frame` 组件库和一个公共 chunk。之前
//! 产物被复制进两个 crate 各自的 `web/`，代价是：
//!
//! - 每个 crate 各有一份 60 行的 `web.rs`（`escape_title` / `Asset` / 路径映射
//!   完全重复）；
//! - 公共 chunk（`panel.js` 168 KB + `panel.css` 146 KB）在仓库里存了两份，
//!   md5 相同；
//! - "改前端"要记得往两个地方各写一遍。
//!
//! 现在产物只存一份，路径映射只写一份，两个面板各自只需要声明**自己是哪个入口**。
//!
//! # 为什么是零依赖
//!
//! 它只做「把一个路径映射到一段 `&'static str`」。加 axum 会让"两个面板共用
//! 一份前端"变成"两个面板共用一份依赖图"——而 serve 逻辑本来就要和各面板的
//! 认证、路由表结合，收进来只会更绕。
//!
//! # 产物为什么入库
//!
//! `cargo build` **绝不调用 npm**：Docker 构建、CI、离线机器都只有 Rust 工具链。
//! 所以 `web/` 里的四个文件是随仓库提交的构建产物——这条约定由
//! [`tests::artifacts_are_fresh`] 守着：前端源码改了却没重建，`cargo test` 会红。

/// 一个面板入口。
///
/// 两个入口共用 `panel.js` / `panel.css`（Vue + 组件库 + 公共插件树），
/// 只有入口脚本不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// 隧道面板（映射 / Spoke / 边缘路由 / 配置导出）。
    Tunnel,
    /// 服务器面板（主机指标 / 容器 / 镜像 / 后台任务）。
    Server,
}

impl Entry {
    /// 全部入口（遍历用）。
    pub const ALL: [Entry; 2] = [Entry::Tunnel, Entry::Server];

    /// 入口脚本的文件名（就是 `/ui/` 路径下暴露的名字）。
    pub const fn script(self) -> &'static str {
        match self {
            Entry::Tunnel => "tunnel.js",
            Entry::Server => "server.js",
        }
    }

    /// 入口名（`ui/` 里的构建入口名，日志与测试里可读）。
    pub const fn name(self) -> &'static str {
        match self {
            Entry::Tunnel => "tunnel",
            Entry::Server => "server",
        }
    }
}

/// 一段可以原样回给浏览器的静态资源。
///
/// 只区分 CSS 与 JS：这两种的 `content-type` 不同，而本项目没有其它形态的
/// 静态资源（图片以 data URI 内联，字体由 CSS 自带）。**不做**通用 MIME 猜测
/// ——那需要一张表和一个依赖，而收益是零。
pub enum Asset {
    Css(&'static str),
    JavaScript(&'static str),
}

impl Asset {
    /// `content-type` 头。
    pub const fn content_type(&self) -> &'static str {
        match self {
            Asset::Css(_) => "text/css; charset=utf-8",
            Asset::JavaScript(_) => "text/javascript; charset=utf-8",
        }
    }

    /// 资源正文。
    pub const fn body(&self) -> &'static str {
        match self {
            Asset::Css(body) => body,
            Asset::JavaScript(body) => body,
        }
    }
}

/// 面板的 HTML 外壳。
///
/// 只有一个 `<div id="app">`：渲染全在前端做。`title` 来自配置，必须转义
/// （它可能含有任意用户配置的字符）。
pub fn index_html(title: &str, entry: Entry) -> String {
    format!(
        "<!DOCTYPE html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\" />\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\
         <title>{}</title><link rel=\"stylesheet\" href=\"/ui/panel.css\" />\
         <script type=\"module\" src=\"/ui/{}\"></script></head>\
         <body><div id=\"app\"></div></body></html>",
        escape_title(title),
        entry.script(),
    )
}

/// 路径 → 资源；不认识就 `None`（调用方回 404）。
///
/// 接受 `assets/` 前缀与 Vite 的 hash 文件名：构建器已把入口脚本改写成固定名，
/// 但浏览器缓存里可能还留着带 hash 的旧引用，别让它变 404。
pub fn asset(path: &str) -> Option<Asset> {
    let path = path.strip_prefix("assets/").unwrap_or(path);
    // Vite 产出的公共 chunk 叫 `panel-<hash>.js`；映射到固定的 `panel.js`。
    let path = if path.starts_with("panel-") && path.ends_with(".js") {
        "panel.js"
    } else {
        path
    };
    match path {
        "panel.css" => Some(Asset::Css(include_str!("../web/panel.css"))),
        "panel.js" => Some(Asset::JavaScript(include_str!("../web/panel.js"))),
        "tunnel.js" => Some(Asset::JavaScript(include_str!("../web/tunnel.js"))),
        "server.js" => Some(Asset::JavaScript(include_str!("../web/server.js"))),
        _ => None,
    }
}

fn escape_title(title: &str) -> String {
    title
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_escapes_title_and_points_at_the_entry_script() {
        let html = index_html("核心 <测试> & \"引号\"", Entry::Tunnel);
        assert!(html.contains("<title>核心 &lt;测试&gt; &amp; &quot;引号&quot;</title>"));
        assert!(html.contains("/ui/panel.css"));
        assert!(html.contains("/ui/tunnel.js"));
        assert!(!html.contains("<测试>"), "未转义的标签会破坏页面：{html}");

        let server = index_html("服务器", Entry::Server);
        assert!(server.contains("/ui/server.js"));
        assert!(!server.contains("tunnel.js"));
    }

    #[test]
    fn both_entries_share_the_common_chunk() {
        // 公共 chunk 只有一份，两个入口都用它——这是把前端收进一个 crate 的
        // 主要理由（此前 168 KB 的 panel.js 在仓库里存了两份）。
        for entry in Entry::ALL {
            let html = index_html("t", entry);
            assert!(html.contains("/ui/panel.js") || html.contains("/ui/panel.css"));
        }
        assert!(matches!(asset("panel.js"), Some(Asset::JavaScript(_))));
        assert!(matches!(asset("panel.css"), Some(Asset::Css(_))));
    }

    #[test]
    fn every_entry_script_is_embedded() {
        for entry in Entry::ALL {
            assert!(
                asset(entry.script()).is_some(),
                "入口 {} 的脚本没有嵌进二进制",
                entry.name()
            );
        }
    }

    #[test]
    fn hashed_and_prefixed_paths_still_resolve() {
        // 浏览器缓存里可能还引用着带 hash 的旧文件名。
        assert!(asset("assets/panel-abc123.js").is_some());
        assert!(asset("assets/panel.css").is_some());
        assert!(asset("tunnel.js").is_some());
    }

    #[test]
    fn unknown_and_traversing_paths_are_rejected() {
        for path in ["", "../secret", "/etc/passwd", "nope.js", "panel.html"] {
            assert!(asset(path).is_none(), "{path} 不该被当成静态资源");
        }
    }

    #[test]
    fn content_types_match_the_kind() {
        let css = asset("panel.css").unwrap();
        assert_eq!(css.content_type(), "text/css; charset=utf-8");
        assert!(css.body().len() > 1000, "CSS 不该是空壳");

        let js = asset("panel.js").unwrap();
        assert_eq!(js.content_type(), "text/javascript; charset=utf-8");
        assert!(js.body().len() > 1000, "JS 不该是空壳");
    }

    /// **产物必须与源码一致**。
    ///
    /// `web/` 是构建产物、随仓库提交（理由见 crate 文档）。提交产物最大的风险
    /// 不是体积，是**漂移**：改了 `ui/src/` 却忘了重建，于是源码与线上行为
    /// 不一致，而没有任何东西会报错。
    ///
    /// 防线是 `ui/scripts/embed.mjs` 每次构建都写 `web/.build-stamp`（对 `ui/src`
    /// 全量内容算的 FNV-1a），这里用同样的算法重算一遍比对。用内容哈希而不是
    /// 时间戳：`git clone` 出来的文件时间戳全都一样，比 mtime 只会制造假警报。
    #[test]
    fn artifacts_are_fresh() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let src = manifest.join("ui/src");
        if !src.is_dir() {
            // 复制出去的源码包 / 极简检出：没有前端源码就没有"新鲜度"可言。
            eprintln!("跳过：{} 不存在（无前端源码）", src.display());
            return;
        }
        let stamp_path = manifest.join("web/.build-stamp");
        let recorded = match std::fs::read_to_string(&stamp_path) {
            Ok(text) => text.trim().to_string(),
            Err(e) => panic!(
                "读不到 {}：{e}\n请按 crates/plugins/panel/esc-panel-web/ui/README.md 重建前端",
                stamp_path.display()
            ),
        };
        let actual = hash_tree(&src);
        assert_eq!(
            actual,
            recorded,
            "前端源码与 web/ 产物不一致——改了 ui/src 却没重建。\n\
             修复：cd crates/plugins/panel/esc-panel-web/ui && npm run build\n\
             （它会重写 web/ 与 web/.build-stamp）"
        );
    }

    /// 对目录下全部文件算一个内容哈希（路径 + 内容，按路径排序）。
    fn hash_tree(dir: &std::path::Path) -> String {
        // FNV-1a 64：无需依赖，用于"检测意外漂移"足够——这不是防篡改哈希。
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        fn feed(mut h: u64, bytes: &[u8]) -> u64 {
            for byte in bytes {
                h ^= u64::from(*byte);
                h = h.wrapping_mul(PRIME);
            }
            h
        }

        let mut files: Vec<std::path::PathBuf> = Vec::new();
        collect(dir, &mut files);
        files.sort();

        let mut h = OFFSET;
        for file in files {
            // 路径统一用 `/`，与 Node 侧的 path.relative 结果一致。
            let rel = file
                .strip_prefix(dir)
                .unwrap_or(&file)
                .to_string_lossy()
                .replace('\\', "/");
            h = feed(h, rel.as_bytes());
            h = feed(h, &[0]);
            let content = std::fs::read(&file).unwrap_or_default();
            h = feed(h, &content);
            h = feed(h, &[0]);
        }
        format!("{h:016x}")
    }

    fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, out);
            } else {
                out.push(path);
            }
        }
    }
}
