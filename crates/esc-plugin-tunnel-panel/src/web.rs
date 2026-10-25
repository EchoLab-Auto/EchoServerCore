//! 前端资源：**单文件**，没有构建步骤。
//!
//! 之所以不用 React/Vite：面板要能作为静态资源嵌进一个 3 MB 的二进制里，
//! 让运维 `scp` 一个文件就能用。任何需要 `npm` 的方案都会给部署加一道门槛。

/// 页面 HTML（标题在渲染时替换）。
pub fn index_html(title: &str) -> String {
    // 标题只用于 <title> 与页头，做一次最小转义防止配置里塞进标签。
    let safe = title
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    RAW.replace("__TITLE__", &safe)
}

const RAW: &str = include_str!("../web/index.html");
