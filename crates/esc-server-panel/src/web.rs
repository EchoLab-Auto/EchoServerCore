//! 服务器面板的静态前端：Vue 3 + `@echolab-auto/ui-frame`。
//! 产物由 `web/` 的 Vite 工程构建并嵌入；部署和运行时不需要 Node 或 CDN。

pub fn index_html(title: &str) -> String {
    let safe = title
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    format!(
        "<!DOCTYPE html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\" />\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\
         <title>{safe}</title><link rel=\"stylesheet\" href=\"/ui/panel.css\" />\
         <script type=\"module\" src=\"/ui/server.js\"></script></head>\
         <body><div id=\"app\"></div></body></html>"
    )
}

pub enum Asset {
    Css(&'static str),
    JavaScript(&'static str),
}

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
        "server.js" => Some(Asset::JavaScript(include_str!("../web/server.js"))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_frame_assets_are_embedded() {
        let html = index_html("服务器 <面板>");
        assert!(html.contains("服务器 &lt;面板&gt;"));
        assert!(html.contains("/ui/server.js"));
        for path in ["panel.css", "panel.js", "server.js"] {
            assert!(asset(path).is_some(), "缺少 {path}");
        }
        assert!(asset("../secret").is_none());
    }
}
