//! 隧道面板的静态前端：Vue 3 + `@echolab-auto/ui-frame`。
//!
//! 源码与构建方法见 `web/`。产物在构建前端后嵌入二进制，运行时不需要
//! Node、CDN 或任何外网资源。这里只负责标题转义和资源路由，业务 API 不变。

pub fn index_html(title: &str) -> String {
    let safe = escape_title(title);
    format!(
        "<!DOCTYPE html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\" />\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\
         <title>{safe}</title><link rel=\"stylesheet\" href=\"/ui/panel.css\" />\
         <script type=\"module\" src=\"/ui/tunnel.js\"></script></head>\
         <body><div id=\"app\"></div></body></html>"
    )
}

fn escape_title(title: &str) -> String {
    title
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub enum Asset {
    Css(&'static str),
    JavaScript(&'static str),
}

/// 编译进二进制的静态资源。content-type 由路由调用方按此枚举设置。
pub fn asset(path: &str) -> Option<Asset> {
    // 构建器在 HTML/CSS/JS 中引用固定文件名，hash 只用于开发缓存。
    let path = path.strip_prefix("assets/").unwrap_or(path);
    let path = if path.starts_with("panel-") && path.ends_with(".js") {
        "panel.js"
    } else {
        path
    };
    match path {
        "panel.css" => Some(Asset::Css(include_str!("../web/panel.css"))),
        "panel.js" | "assets/panel.js" => Some(Asset::JavaScript(include_str!("../web/panel.js"))),
        "tunnel.js" => Some(Asset::JavaScript(include_str!("../web/tunnel.js"))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_frame_assets_are_embedded_and_title_is_escaped() {
        let html = index_html("核心 <测试>");
        assert!(html.contains("核心 &lt;测试&gt;"));
        assert!(html.contains("/ui/tunnel.js"));
        assert!(html.contains("/ui/panel.css"));
        for path in ["panel.css", "panel.js", "tunnel.js"] {
            assert!(asset(path).is_some(), "缺少 {path}");
        }
        assert!(asset("../secret").is_none());
    }
}
