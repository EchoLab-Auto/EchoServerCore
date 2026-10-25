//! 宿主二进制入口：读配置 → 起宿主 → 等信号 → 优雅退出。
//!
//! 组装逻辑在 `esc_host::Host` 里（lib），这样集成测试能复用同一套接线，
//! 不会出现「测试里跑的是另一份逻辑」这种假通过。

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let cfg_path = std::env::args()
        .skip(1)
        .collect::<Vec<_>>()
        .windows(2)
        .find(|w| w[0] == "--config" || w[0] == "-c")
        .map(|w| PathBuf::from(&w[1]))
        .or_else(|| std::env::var("ESC_HOST_CONFIG").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/etc/esc/host.toml"));

    // 用显式构建的 multi-thread 运行时：`HostService::call` 里的 `block_in_place`
    // 依赖它。
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
        esc_host::run_until_signal(&cfg_path, None).await
    })
}
