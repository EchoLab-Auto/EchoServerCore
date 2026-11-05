//! Web 面板插件入口。
//!
//! **帧只走 stdout，日志一律走 stderr** —— 这是插件框架的硬约定，
//! 违反的话宿主的 JSON 帧解析会被日志打断。

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Err(e) = esc_plugin_tunnel_panel::run().await {
        eprintln!("panel plugin exited: {e}");
        std::process::exit(1);
    }
}
