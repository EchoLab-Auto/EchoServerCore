//! 隧道面板的静态前端：实现见 [`esc_panel_web`]，这里只绑定**入口**。
//!
//! 两个面板共用同一份 Vue 工程与构建产物（`crates/plugins/panel/esc-panel-web/`），差别只有
//! 入口脚本（`tunnel.js` / `server.js`）。此前这个文件有 60 行——`escape_title`、
//! `Asset` 枚举、路径映射各写一遍——而两份是逐字相同。

pub use esc_panel_web::{asset, Asset};

/// 本面板的 HTML 外壳。
pub fn index_html(title: &str) -> String {
    esc_panel_web::index_html(title, esc_panel_web::Entry::Tunnel)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 守卫「这个面板绑的是哪个入口」——复制粘贴最容易错的就这一行。
    #[test]
    fn this_panel_serves_the_tunnel_entry() {
        let html = index_html("t");
        assert!(html.contains("/ui/tunnel.js"), "{html}");
        assert!(!html.contains("/ui/server.js"), "{html}");
        assert!(asset("tunnel.js").is_some(), "入口脚本必须嵌进二进制");
    }
}
