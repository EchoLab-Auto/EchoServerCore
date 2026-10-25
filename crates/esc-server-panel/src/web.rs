//! 前端资源：**单文件**，没有构建步骤。
//!
//! 和隧道面板同一个理由：要能当一个静态资源嵌进二进制里，`scp` 一个文件就部署。
//! 任何需要 `npm` 的方案都会给运维加一道门槛，而这道门槛的收益（组件化）在
//! 一个单页仪表盘上几乎为零。

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
