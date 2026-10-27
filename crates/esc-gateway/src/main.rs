//! 二进制入口：`esc-gateway [-c <配置>]` 或 `esc-gateway hash-password`。

use std::path::PathBuf;

use anyhow::Context;
use tokio_util::sync::CancellationToken;

const DEFAULT_CONFIG: &str = "/etc/esc/gateway.toml";

fn usage() -> String {
    format!(
        "esc-gateway {} —— 面板前面的认证反向代理

用法:
    esc-gateway [-c|--config <路径>]
    esc-gateway hash-password [口令]
    esc-gateway check [-c|--config <路径>]
    esc-gateway health [-c|--config <路径>]

子命令:
    hash-password   生成 argon2id 哈希（不传口令则从标准输入读，避免进 shell 历史）
    check           只校验配置并打印将要监听的端口，不绑定任何东西
    health          探活：向配置里第一个 route 的端口发一个 /__auth/healthz
                    （容器健康检查用——镜像里没有 shell，不能用 curl）

选项:
    -c, --config <路径>   配置文件，默认 {DEFAULT_CONFIG}
                          也可用环境变量 ESC_GATEWAY_CONFIG
    -h, --help            显示本帮助后退出
    -V, --version         显示版本后退出

配置文件示例见仓库 examples/gateway.toml。",
        env!("CARGO_PKG_VERSION")
    )
}

#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Run(PathBuf),
    Check(PathBuf),
    Health(PathBuf),
    HashPassword(Option<String>),
    Help,
    Version,
}

fn parse_args<I, S>(args: I) -> Result<Invocation, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut it = args.into_iter().map(Into::into).peekable();
    let mut config: Option<PathBuf> = None;

    // 第一个位置参数可能是子命令
    let mut sub: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();

    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "-V" | "--version" => return Ok(Invocation::Version),
            "-c" | "--config" => {
                let v = it.next().ok_or_else(|| format!("{a} 需要一个路径参数"))?;
                config = Some(PathBuf::from(v));
            }
            other if other.starts_with("--config=") => {
                let v = other.trim_start_matches("--config=");
                if v.is_empty() {
                    return Err("--config= 后面需要一个路径".into());
                }
                config = Some(PathBuf::from(v));
            }
            other if other.starts_with('-') => {
                return Err(format!("无法识别的选项 {other:?}\n\n{}", usage()));
            }
            other => {
                if sub.is_none() {
                    sub = Some(other.to_string());
                } else {
                    rest.push(other.to_string());
                }
            }
        }
    }

    let cfg_path = || {
        config.clone().unwrap_or_else(|| {
            PathBuf::from(
                std::env::var("ESC_GATEWAY_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG.to_string()),
            )
        })
    };

    match sub.as_deref() {
        None => Ok(Invocation::Run(cfg_path())),
        Some("check") => Ok(Invocation::Check(cfg_path())),
        Some("health") => Ok(Invocation::Health(cfg_path())),
        Some("hash-password") => Ok(Invocation::HashPassword(rest.into_iter().next())),
        Some(other) => Err(format!("未知子命令 {other:?}\n\n{}", usage())),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,esc_gateway=debug".into()),
        )
        .init();

    let inv = match parse_args(std::env::args().skip(1)) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };

    match inv {
        Invocation::Help => {
            println!("{}", usage());
            Ok(())
        }
        Invocation::Version => {
            println!("esc-gateway {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Invocation::HashPassword(pw) => {
            let pw = match pw {
                Some(p) => p,
                None => {
                    // 从 stdin 读：命令行参数会留在 shell 历史与 ps 输出里。
                    eprintln!("从标准输入读取口令（输入不回显）：");
                    rpassword_read()?
                }
            };
            if pw.is_empty() {
                anyhow::bail!("口令不能为空");
            }
            // 哈希只打到 stdout，方便 `>> gateway.toml` 或复制
            println!("{}", esc_gateway::config::hash_password(&pw)?);
            Ok(())
        }
        Invocation::Check(path) => {
            let cfg = esc_gateway::GatewayConfig::load(&path)?;
            let rt = cfg.build().map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("OK  gateway config: {}", path.display());
            println!("    title        : {}", rt.title);
            println!(
                "    session ttl  : {} 小时",
                rt.session_ttl.as_secs() / 3600
            );
            println!(
                "    cookie       : {}{}",
                rt.cookie_name,
                if rt.secure_cookie { "  (Secure)" } else { "" }
            );
            println!("    users        : {}", rt.users.len());
            for u in rt.users.values() {
                let scope = if u.all_routes {
                    "*".to_string()
                } else {
                    u.routes.join(", ")
                };
                println!(
                    "      - {:<12} routes: {scope}{}",
                    u.name,
                    if u.admin { "  [admin]" } else { "" }
                );
            }
            match &rt.users_file {
                Some(p) => {
                    let store = esc_gateway::AccountStore::load(Some(p));
                    let state = if store.writable() {
                        format!("可写，{} 个账号", store.count())
                    } else {
                        format!(
                            "只读（{}）",
                            store.note().unwrap_or_else(|| "原因未知".into())
                        )
                    };
                    println!("    users_file   : {} —— {state}", p.display());
                    if store.count() > 0 {
                        for u in store.all() {
                            let scope = if u.routes.iter().any(|r| r == "*") {
                                "*".to_string()
                            } else {
                                u.routes.join(", ")
                            };
                            println!(
                                "      - {:<12} routes: {scope}{}",
                                u.name,
                                if u.admin { "  [admin]" } else { "" }
                            );
                        }
                    }
                    for r in store.rejected() {
                        println!("      ! 被拒绝的条目：{r}");
                    }
                }
                None => println!("    users_file   : 未配置 —— 管理面板为只读"),
            }
            let admins = rt.users.values().filter(|u| u.admin).count();
            if admins == 0 {
                println!();
                println!("⚠ 配置文件里没有管理员账号（admin = true），管理面板没人能进。");
                println!("  要启用账号管理：给某个 [[user]] 加 admin = true。");
            }

            match &rt.routes_file {
                Some(p) => {
                    let st = esc_gateway::RouteStore::load(Some(p));
                    let state = if st.writable() {
                        format!("可写，{} 条", st.count())
                    } else {
                        format!("只读（{}）", st.note().unwrap_or_else(|| "原因未知".into()))
                    };
                    println!("    routes_file  : {} —— {state}", p.display());
                    for r in st.all() {
                        println!("      - {:<16} {} → {}", r.name, r.listen, r.upstream);
                    }
                    for r in st.rejected() {
                        println!("      ! 被拒绝的条目：{r}");
                    }
                }
                None => println!("    routes_file  : 未配置 —— 端口只能在配置文件里改"),
            }

            println!("    routes       : {}", rt.routes.len());
            for r in rt.routes.values() {
                println!(
                    "      - {:<16} {} → {}  cidrs={} body={}KB",
                    r.name,
                    r.listen,
                    r.upstream,
                    r.allow.len(),
                    r.max_body_bytes / 1024
                );
            }
            println!();
            match rt.admin_listen {
                Some(a) => {
                    println!("管理面板：独立端口 {a}（/__admin/，含登录页）");
                    println!("         转发端口上的 /__admin/ 只给一页「在别处」的说明");
                }
                None => println!("管理面板：登录后在任意端口访问 /__admin/"),
            }
            if !rt.admin_allow.is_empty() {
                println!("         （仅限 admin_allowed_cidrs 中的来源）");
            }
            if !rt.secure_cookie {
                println!();
                println!("⚠ 未开启 secure_cookie：网关与浏览器之间是明文 HTTP。");
                println!("  口令与 session cookie 在链路上可被读取。生产环境请在前面加 TLS");
                println!("  （nginx/Caddy），或等网关自己支持 TLS 后打开 secure_cookie。");
            }
            Ok(())
        }
        Invocation::Health(path) => {
            // 只校验配置（不启动服务），然后向第一个 route 的端口发一个探活请求。
            let cfg = esc_gateway::GatewayConfig::load(&path)?;
            let rt = cfg.build().map_err(|e| anyhow::anyhow!("{e}"))?;
            let Some(route) = rt.routes.values().min_by_key(|r| r.listen.port()) else {
                anyhow::bail!("配置里没有 route");
            };
            // 监听在 0.0.0.0 时连回环
            let host = if route.listen.ip().is_unspecified() {
                "127.0.0.1".to_string()
            } else {
                route.listen.ip().to_string()
            };
            let addr = format!("{host}:{}", route.listen.port());
            match probe(&addr).await {
                Ok(()) => {
                    println!("ok {addr}");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("unhealthy {addr}: {e}");
                    std::process::exit(1);
                }
            }
        }
        Invocation::Run(path) => {
            let cfg = esc_gateway::GatewayConfig::load(&path)
                .with_context(|| format!("加载配置 {}", path.display()))?;
            let rt = cfg.build().map_err(|e| anyhow::anyhow!("{e}"))?;
            if !rt.secure_cookie {
                tracing::warn!(
                    "网关对外是明文 HTTP：口令与 session cookie 在链路上可被读取。\
                     生产环境应在前面加 TLS 终结（nginx/Caddy），并打开 secure_cookie"
                );
            }
            if rt.admin_listen.is_none() {
                tracing::info!(
                    "管理面板挂在每个转发端口的 /__admin/ 上；                     想让它独占一个端口就配 admin_listen"
                );
            }
            if !rt.users.values().any(|u| u.admin) {
                tracing::warn!(
                    "配置文件里没有管理员账号（admin = true）：/__admin/ 没人能进。\
                     需要账号管理就给某个 [[user]] 加 admin = true"
                );
            }
            let shutdown = CancellationToken::new();
            let sig = shutdown.clone();
            tokio::spawn(async move {
                let mut term =
                    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    {
                        Ok(s) => s,
                        Err(_) => return,
                    };
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                tracing::info!("shutdown signal received");
                sig.cancel();
            });
            esc_gateway::run(rt, shutdown).await
        }
    }
}

/// 向 `addr` 发一个 `GET /__auth/healthz`，判断是否 200。
///
/// 手写而不是用 HTTP 客户端：只需要发一个固定请求读一行状态行，
/// 为此引一个客户端 crate 不划算（而且容器镜像里越少依赖越好）。
async fn probe(addr: &str) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut sock = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .map_err(|_| anyhow::anyhow!("连接超时"))??;

    let req = format!("GET /__auth/healthz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).await?;

    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(std::time::Duration::from_secs(3), sock.read(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("读响应超时"))??;
    let line = String::from_utf8_lossy(&buf[..n]);
    let status = line
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("");
    if status == "200" {
        Ok(())
    } else {
        anyhow::bail!("健康端点返回 {status:?}")
    }
}

/// 从终端读一行口令，不回显。
///
/// 用 `stty -echo` 而不是引入 `rpassword`：为一个功能引一个 crate 不值，
/// 而这里的边界很清楚——只处理「有终端」与「没有终端（管道）」两种情况。
fn rpassword_read() -> anyhow::Result<String> {
    use std::io::{BufRead, Write};

    // 非 tty（比如 `echo pw | esc-gateway hash-password`）时直接读。
    // 用 std 的 `IsTerminal` 而不是 extern isatty：少一个 unsafe，少一份 FFI 假设。
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        return Ok(line.trim_end_matches(['\r', '\n']).to_string());
    }

    let _ = std::process::Command::new("stty").arg("-echo").status();
    let mut line = String::new();
    let r = std::io::stdin().lock().read_line(&mut line);
    let _ = std::process::Command::new("stty").arg("echo").status();
    // 回显被关掉时用户按的回车不会显示出来，补一个换行
    println!();
    std::io::stdout().flush().ok();
    r?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_args_runs_with_default_config() {
        std::env::remove_var("ESC_GATEWAY_CONFIG");
        assert_eq!(
            parse_args(Vec::<String>::new()).unwrap(),
            Invocation::Run(DEFAULT_CONFIG.into())
        );
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(parse_args(["-h"]).unwrap(), Invocation::Help);
        assert_eq!(parse_args(["--version"]).unwrap(), Invocation::Version);
        assert_eq!(parse_args(["-V"]).unwrap(), Invocation::Version);
    }

    #[test]
    fn config_forms_are_equivalent() {
        assert_eq!(
            parse_args(["-c", "/tmp/a.toml"]).unwrap(),
            Invocation::Run("/tmp/a.toml".into())
        );
        assert_eq!(
            parse_args(["--config=/tmp/a.toml"]).unwrap(),
            Invocation::Run("/tmp/a.toml".into())
        );
        // 子命令 + 配置
        assert_eq!(
            parse_args(["check", "-c", "/tmp/a.toml"]).unwrap(),
            Invocation::Check("/tmp/a.toml".into())
        );
    }

    #[test]
    fn hash_password_subcommand_takes_optional_argument() {
        assert_eq!(
            parse_args(["hash-password"]).unwrap(),
            Invocation::HashPassword(None)
        );
        assert_eq!(
            parse_args(["hash-password", "s3cret"]).unwrap(),
            Invocation::HashPassword(Some("s3cret".into()))
        );
    }

    #[test]
    fn health_subcommand_is_parsed() {
        assert_eq!(
            parse_args(["health"]).unwrap(),
            Invocation::Health(DEFAULT_CONFIG.into())
        );
    }

    #[test]
    fn unknown_subcommand_and_flags_are_rejected() {
        assert!(parse_args(["frobnicate"]).is_err());
        assert!(parse_args(["--nope"]).is_err());
        assert!(parse_args(["-c"]).is_err());
        assert!(parse_args(["--config="]).is_err());
    }
}
