//! 服务器面板：主机指标 + Docker 容器管理。
//!
//! 这是一个**独立进程**，不是 `esc-host` 的插件——理由写在 `docs/SERVER-PANEL.md`，
//! 一句话：它需要 docker socket（等价于 root），而隧道进程应该保持"无聊"。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use esc_panel_auth::{AccessControl, AuthConfig};
use esc_server_panel::api::{router, AppState};
use esc_server_panel::docker::Docker;
use esc_server_panel::sys::{self, Sampler};
use serde::Deserialize;

const DEFAULT_CONFIG: &str = "/etc/esc/server-panel.toml";
const DEFAULT_DOCKER_SOCK: &str = "/var/run/docker.sock";

// ─────────────────────────────────────────────────────────────────────────────
// 配置
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default = "default_title")]
    title: String,
    auth: Option<String>,
    #[serde(default)]
    allow_cidrs: Vec<String>,
    #[serde(default = "default_max_fail")]
    max_auth_failures: u32,
    #[serde(default = "default_window")]
    auth_fail_window_secs: u64,
    #[serde(default = "default_docker_sock")]
    docker_socket: String,
    /// 是否允许启停 / 改名 / 改重启策略。
    #[serde(default = "default_true")]
    allow_actions: bool,
    /// 是否允许创建容器、拉镜像。
    ///
    /// 单独一道闸的理由：创建是唯一会「留下东西」的操作，而只读监控的人
    /// （比如值班同学）可能确实只需要启停现有容器。
    #[serde(default = "default_true")]
    allow_create: bool,
    /// 是否允许删除容器 / 镜像。不可撤销，所以单独一个开关。
    #[serde(default = "default_true")]
    allow_delete: bool,
}

fn default_listen() -> String {
    "127.0.0.1:2224".into()
}
fn default_title() -> String {
    "服务器面板".into()
}
fn default_max_fail() -> u32 {
    8
}
fn default_window() -> u64 {
    60
}
fn default_docker_sock() -> String {
    DEFAULT_DOCKER_SOCK.into()
}
fn default_true() -> bool {
    true
}

impl Config {
    fn auth_config(&self) -> AuthConfig {
        AuthConfig {
            auth: self.auth.clone(),
            allow_cidrs: self.allow_cidrs.clone(),
            max_auth_failures: self.max_auth_failures,
            auth_fail_window_secs: self.auth_fail_window_secs,
        }
    }

    /// 监听地址是否只落在回环上。
    fn is_loopback_only(&self) -> bool {
        let host = self
            .listen
            .rsplit_once(':')
            .map(|(h, _)| h.trim_start_matches('[').trim_end_matches(']'))
            .unwrap_or(&self.listen);
        matches!(host, "127.0.0.1" | "localhost" | "::1")
    }

    fn validate(&self) -> Result<(), String> {
        if self.listen.rsplit_once(':').is_none() {
            return Err(format!("listen 需要 host:port，收到 {:?}", self.listen));
        }
        if self.auth.is_none() && !self.is_loopback_only() {
            return Err(format!(
                "listen = {:?} 不是回环地址，但没有配置 auth；\
                 拒绝把无认证的管理面板暴露到网络上",
                self.listen
            ));
        }
        self.auth_config().validate()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 命令行
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Run(PathBuf),
    Help,
    Version,
}

fn usage() -> String {
    format!(
        "esc-server-panel {} —— 服务器状态与 Docker 管理面板

用法:
    esc-server-panel [-c|--config <路径>]

选项:
    -c, --config <路径>   配置文件，默认 {DEFAULT_CONFIG}
                          也可用环境变量 ESC_SERVER_PANEL_CONFIG
    -h, --help            显示本帮助后退出
    -V, --version         显示版本后退出

配置文件示例见仓库 examples/server-panel.toml。",
        env!("CARGO_PKG_VERSION")
    )
}

/// 参数解析。
///
/// 严格解析不是洁癖：`esc-host` 曾经把 `--help` 当成「没给参数」而用默认配置
/// 启动服务，在生产机上会去抢端口。这种误操作成本很高，所以在每个守护进程里
/// 都**拒绝未知参数**。
fn parse_args<I, S>(args: I) -> Result<Invocation, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut config: Option<PathBuf> = None;
    let mut it = args.into_iter().map(Into::into);

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "-V" | "--version" => return Ok(Invocation::Version),
            "-c" | "--config" => {
                let v = it.next().ok_or_else(|| format!("{arg} 需要一个路径参数"))?;
                config = Some(PathBuf::from(v));
            }
            other if other.starts_with("--config=") => {
                let v = other.trim_start_matches("--config=");
                if v.is_empty() {
                    return Err("--config= 后面需要一个路径".into());
                }
                config = Some(PathBuf::from(v));
            }
            other => {
                return Err(format!("无法识别的参数 {other:?}\n\n{}", usage()));
            }
        }
    }

    Ok(Invocation::Run(config.unwrap_or_else(|| {
        PathBuf::from(
            std::env::var("ESC_SERVER_PANEL_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG.to_string()),
        )
    })))
}

// ─────────────────────────────────────────────────────────────────────────────
// main
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,esc_server_panel=debug".into()),
        )
        .init();

    let path = match parse_args(std::env::args().skip(1)) {
        Ok(Invocation::Run(p)) => p,
        Ok(Invocation::Help) => {
            println!("{}", usage());
            return Ok(());
        }
        Ok(Invocation::Version) => {
            println!("esc-server-panel {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };

    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("读取配置文件 {} 失败", path.display()))?;
    let cfg: Config = toml::from_str(&raw)
        .with_context(|| format!("解析配置文件 {} 失败", path.display()))?;
    cfg.validate().map_err(|e| anyhow::anyhow!("配置无效：{e}"))?;

    let docker = Docker::new(&cfg.docker_socket);
    if !docker.socket_exists() {
        // 不是错误：没有 Docker 的机器也能用面板看主机指标。
        tracing::info!(
            socket = %cfg.docker_socket,
            "未发现 Docker socket，容器相关功能将显示为「未安装」"
        );
    }

    let state = AppState {
        sampler: Arc::new(Sampler::new()),
        docker: Arc::new(docker),
        title: cfg.title.clone(),
        access: Arc::new(AccessControl::new(&cfg.auth_config())),
        allow_actions: cfg.allow_actions,
        allow_create: cfg.allow_create,
        allow_delete: cfg.allow_delete,
        jobs: Arc::new(esc_server_panel::docker::Jobs::new()),
    };

    if !cfg.is_loopback_only() {
        tracing::warn!(
            listen = %cfg.listen,
            allow_cidrs = cfg.allow_cidrs.len(),
            allow_actions = cfg.allow_actions,
            allow_create = cfg.allow_create,
            allow_delete = cfg.allow_delete,
            "面板监听在非回环地址：管理面已暴露到网络。\
             口令以 Basic 认证明文传输；且本面板可启停容器——\
             拿到口令等于拿到机器的实质控制权，请务必用长口令并考虑限制 allow_cidrs"
        );
    }
    if !cfg.allow_actions {
        tracing::info!("allow_actions = false，容器启停/改名/重启策略已禁用");
    }
    if !cfg.allow_create {
        tracing::info!("allow_create = false，创建容器与拉镜像已禁用");
    }
    if !cfg.allow_delete {
        tracing::info!("allow_delete = false，删除容器与镜像已禁用");
    }
    if !(cfg.allow_actions || cfg.allow_create || cfg.allow_delete) {
        tracing::info!("三个写开关全关：纯只读监控模式");
    }
    if sys::is_containerised() {
        tracing::info!("检测到本进程运行在容器内，主机指标可能反映的是容器视图");
    }

    let listener = tokio::net::TcpListener::bind(&cfg.listen)
        .await
        .with_context(|| format!("绑定 {} 失败", cfg.listen))?;
    let addr = listener
        .local_addr()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));

    tracing::info!(
        addr = %addr,
        auth = cfg.auth.is_some(),
        docker = %cfg.docker_socket,
        "server panel listening"
    );

    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("收到退出信号");
    })
    .await
    .context("HTTP 服务异常退出")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_loopback_and_safe() {
        let cfg: Config = toml::from_str("").expect("空配置应能解析（全取默认值）");
        assert_eq!(cfg.listen, "127.0.0.1:2224");
        assert!(cfg.is_loopback_only());
        assert!(cfg.validate().is_ok(), "回环 + 无认证是允许的");
        assert_eq!(cfg.max_auth_failures, 8);
        assert_eq!(cfg.docker_socket, "/var/run/docker.sock");
        assert!(cfg.allow_actions);
        assert!(cfg.allow_create);
        assert!(cfg.allow_delete);
    }

    #[test]
    fn write_switches_can_be_disabled_independently() {
        let cfg: Config = toml::from_str(
            r#"
allow_actions = false
allow_create = false
"#,
        )
        .unwrap();
        assert!(!cfg.allow_actions);
        assert!(!cfg.allow_create);
        assert!(cfg.allow_delete, "没提到的开关应保持默认 true");
    }

    #[test]
    fn non_loopback_without_auth_is_rejected() {
        let cfg: Config = toml::from_str(r#"listen = "0.0.0.0:2224""#).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("auth"), "错误信息应指出缺 auth：{err}");
    }

    #[test]
    fn non_loopback_with_auth_is_accepted() {
        let cfg: Config = toml::from_str(
            r#"
listen = "0.0.0.0:2224"
auth = "admin:long-random-password"
"#,
        )
        .unwrap();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        // deny_unknown_fields：配置里拼错的键必须报错，否则「我明明关了啊」会成为谜案
        let res: Result<Config, _> = toml::from_str(r#"allow_action = false"#);
        assert!(res.is_err(), "拼错的键应当被拒绝");
    }

    #[test]
    fn bad_cidr_fails_validation() {
        let cfg: Config = toml::from_str(
            r#"
listen = "0.0.0.0:2224"
auth = "admin:pw"
allow_cidrs = ["10.0.0.0/33"]
"#,
        )
        .unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn args_are_parsed_strictly() {
        assert_eq!(parse_args(["-h"]).unwrap(), Invocation::Help);
        assert_eq!(parse_args(["--version"]).unwrap(), Invocation::Version);
        assert_eq!(
            parse_args(["-c", "/tmp/a.toml"]).unwrap(),
            Invocation::Run("/tmp/a.toml".into())
        );
        assert_eq!(
            parse_args(["--config=/tmp/a.toml"]).unwrap(),
            Invocation::Run("/tmp/a.toml".into())
        );
        // 未知参数不能静默忽略
        assert!(parse_args(["--hlep"]).is_err());
        assert!(parse_args(["/tmp/x"]).is_err());
        assert!(parse_args(["-c"]).is_err());
    }
}
