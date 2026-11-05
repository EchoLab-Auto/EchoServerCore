//! 宿主二进制入口：读配置 → 起宿主 → 等信号 → 优雅退出。
//!
//! 组装逻辑在 `esc_host::Host` 里（lib），这样集成测试能复用同一套接线，
//! 不会出现「测试里跑的是另一份逻辑」这种假通过。

fn main() -> anyhow::Result<()> {
    // 参数解析必须严格：`esc-host --help` 绝不能变成「用默认配置启动服务」。
    let cfg_path = match esc_host::parse_args(std::env::args().skip(1)) {
        Ok(esc_host::Invocation::Run(path)) => path,
        Ok(esc_host::Invocation::Help) => {
            println!("{}", esc_host::usage());
            return Ok(());
        }
        Ok(esc_host::Invocation::Version) => {
            println!("esc-host {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    // 显式构建 multi-thread 运行时：控制面会并发处理多个面板请求，且
    // `HostCall` 的回执任务与隧道转发共用一个池。`block_in_place` 已随框架
    // 的 `AsyncHostService` 一起消失（v0.12.0），不再是这里的理由。
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
