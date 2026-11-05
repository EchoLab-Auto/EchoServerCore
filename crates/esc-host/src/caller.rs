//! 宿主服务的**调用者策略**：谁能调哪个 `HostCall` 服务。
//!
//! # 为什么这不是形式主义
//!
//! 框架在 v0.12.0 之后会把调用者身份交给宿主（`CallContext::caller`，由
//! supervisor 从连接归属填写，插件无法伪造）。拿到之后如果不判断，那条信息就
//! 白拿了——"谁都能调"这件事会继续靠**部署上的巧合**（"我们只有自己写的插件"）
//! 维持，而不是靠代码。
//!
//! 三处宿主服务的权限差别很大：
//!
//! | 服务 | 能做什么 | 该给谁 |
//! |---|---|---|
//! | `esc.control` | 增删端口映射、踢 spoke、重载 | 隧道面板 |
//! | `esc.edge` | 增删边缘路由（含绑 80 的那张表） | 隧道面板 |
//! | `esc.plugins` | 读插件树（只读） | 隧道面板、运维工具 |
//!
//! # 现在的规则
//!
//! 一张**允许清单**，默认拒绝。清单里目前只有隧道面板一个 id：
//!
//! ```text
//! 面板插件（唯一消费者）→ 允许
//! 其它任何插件         → forbidden + warn 日志
//! ```
//!
//! 默认拒绝而不是默认允许，是因为"新加一个插件忘了配权限"的后果不对称：
//! 默认允许 = 它立刻能删掉所有映射；默认拒绝 = 它调不通，日志里一条 warn，
//! 下一个人补一行清单即可。
//!
//! # 为什么不做成配置
//!
//! 权限表一旦可配置，就有了"配置写错导致权限开太大"的失效模式，而这一步在
//! 引入第三方插件之前没有收益。现在把它写成常量 + 测试，比写进 TOML 更安全：
//! 改它必须经过一次代码评审，而不是改一行配置重启。

use echo_plugin_host::host_service::CallContext;
use tracing::warn;

/// 宿主服务调用的允许清单。
#[derive(Debug, Clone)]
pub struct CallerPolicy {
    /// 允许调用宿主服务的插件 id。
    allowed: Vec<String>,
}

impl CallerPolicy {
    /// 只允许给定的插件 id。
    pub fn new(allowed: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowed: allowed.into_iter().map(Into::into).collect(),
        }
    }

    /// 允许清单（只读，测试与日志用）。
    pub fn allowed(&self) -> &[String] {
        &self.allowed
    }

    /// 校验调用者。不允许时返回契约里的 `forbidden` 错误码。
    ///
    /// 错误消息里**不回显允许清单**：那等于把内部拓扑告诉一个本不该知道它的
    /// 插件。清单只进日志（运维看），不进响应。
    pub fn check(&self, cx: &CallContext, service: &str) -> Result<(), (String, String)> {
        if self.allowed.iter().any(|id| id == &cx.caller) {
            return Ok(());
        }
        warn!(
            caller = %cx.caller,
            service,
            "拒绝宿主服务调用：调用者不在允许清单里"
        );
        Err((
            "forbidden".to_string(),
            format!("插件 {:?} 无权调用宿主服务 {service}", cx.caller),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx(caller: &str) -> CallContext {
        CallContext {
            caller: caller.to_string(),
        }
    }

    #[test]
    fn allowed_caller_passes() {
        let policy = CallerPolicy::new(["esc.tunnel.panel"]);
        assert!(policy.check(&cx("esc.tunnel.panel"), "esc.control").is_ok());
    }

    #[test]
    fn unknown_caller_is_forbidden_with_contract_code() {
        let policy = CallerPolicy::new(["esc.tunnel.panel"]);
        let err = policy
            .check(&cx("someone.else"), "esc.control")
            .expect_err("陌生调用者必须被拒");
        assert_eq!(err.0, "forbidden", "错误码要能被面板映射成 403");
        assert!(
            err.1.contains("someone.else"),
            "消息应点名调用者（便于排查）：{}",
            err.1
        );
    }

    #[test]
    fn error_message_does_not_leak_the_allow_list() {
        // 拒绝消息不能把内部拓扑告诉一个本不该知道它的插件。
        let policy = CallerPolicy::new(["esc.tunnel.panel"]);
        let err = policy.check(&cx("x"), "esc.control").unwrap_err();
        assert!(
            !err.1.contains("esc.tunnel.panel"),
            "拒绝消息不应回显允许清单：{}",
            err.1
        );
    }

    #[test]
    fn empty_policy_denies_everyone() {
        // 未配置 = 全拒（fail-closed）。哪怕调用者是空字符串也一样。
        let policy = CallerPolicy::new(std::iter::empty::<String>());
        assert!(policy.check(&cx(""), "esc.control").is_err());
        assert!(policy.check(&cx("esc.tunnel.panel"), "esc.control").is_err());
    }
}
