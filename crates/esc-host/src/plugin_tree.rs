//! `esc.plugins`：把框架的真实注册表/作用域树投影成只读 JSON。
//!
//! 这层只做观测，不提供启停或挂载操作。两个 Web 面板看到的是同一份契约，
//! 不需要、也不能直接触碰宿主的 `PluginRegistry`。

use std::sync::{Arc, Mutex};

use echo_plugin::{PluginScope, PluginTreeNode as FrameworkNode};
use echo_plugin_host::async_trait;
use echo_plugin_host::host_service::{AsyncHostService, CallContext};
use echo_plugin_host::supervisor::{PluginHandle, PluginState};
use esc_control_api::{
    PluginTreeNode, PluginTreeSnapshot, PLUGIN_TREE_METHOD, PLUGIN_TREE_SERVICE,
};
use serde_json::Value;

use crate::caller::CallerPolicy;
use crate::TUNNEL_PANEL_PLUGIN_ID;

/// 子进程面板没有住在 `PluginRegistry` 里，因此单独保存它的框架句柄。
/// `PluginHandle` 本身提供实时状态，不是启动时复制的一份字符串。
#[derive(Default)]
pub struct PanelObservation {
    pub enabled: bool,
    pub command: String,
    pub handle: Option<PluginHandle>,
    pub start_error: Option<String>,
}

pub type SharedPanelObservation = Arc<Mutex<PanelObservation>>;

pub struct PluginTreeHostService {
    root: Arc<PluginScope>,
    panel: SharedPanelObservation,
    host_services: Vec<String>,
    policy: Arc<CallerPolicy>,
}

impl PluginTreeHostService {
    pub fn new(
        root: Arc<PluginScope>,
        panel: SharedPanelObservation,
        host_services: Vec<String>,
        policy: Arc<CallerPolicy>,
    ) -> Self {
        Self {
            root,
            panel,
            host_services,
            policy,
        }
    }

    pub fn snapshot(&self) -> PluginTreeSnapshot {
        snapshot(&self.root, &self.panel, self.host_services.clone())
    }
}

#[async_trait]
impl AsyncHostService for PluginTreeHostService {
    async fn call(
        &self,
        cx: CallContext,
        method: &str,
        _payload: Value,
    ) -> Result<Value, (String, String)> {
        self.policy.check(&cx, PLUGIN_TREE_SERVICE)?;
        if method != PLUGIN_TREE_METHOD {
            return Err((
                "not_found".into(),
                format!("{PLUGIN_TREE_SERVICE} 只有 {PLUGIN_TREE_METHOD} 方法，收到 {method:?}"),
            ));
        }
        serde_json::to_value(self.snapshot())
            .map_err(|e| ("other".into(), format!("插件树序列化失败: {e}")))
    }
}

/// 从框架对象生成快照。
///
/// 进程内部分**直接用框架的 `PluginScope::snapshot()`**：遍历注册表、拼接
/// 子作用域、判定 `ready/pending/failed/disabled` 都是框架的事，宿主不该重写
/// 一遍（重写就会与框架的语义漂移）。这里只做两件框架看不见的事：
///
/// 1. 补上宿主自己的名字与服务清单（框架不知道有 `HostCall` 服务表）；
/// 2. 追加**子进程**插件节点——它不在 `PluginRegistry` 里，状态在
///    `PluginHandle` 手上。
pub fn snapshot(
    root: &Arc<PluginScope>,
    panel: &SharedPanelObservation,
    host_services: Vec<String>,
) -> PluginTreeSnapshot {
    let framework = root.snapshot();
    let mut plugins: Vec<PluginTreeNode> = framework
        .plugins
        .iter()
        .map(from_framework_node)
        .collect();
    plugins.push(panel_node(panel));
    PluginTreeSnapshot {
        schema_version: framework.schema_version,
        host: "esc-host".into(),
        framework: "echo-fabric-arch".into(),
        context_services: framework.context_services,
        host_services,
        plugins,
    }
}

/// 框架节点 → 契约节点。
///
/// 只差 `restarts` / `generation` 两个字段：它们是**子进程**才有的运行态，
/// 进程内插件没有独立进程可重启，恒为 0。框架的树投影刻意不含这两项——
/// 它描述的是注册表事实，不是 supervisor 事实。
fn from_framework_node(node: &FrameworkNode) -> PluginTreeNode {
    PluginTreeNode {
        id: node.id.clone(),
        name: node.name.clone(),
        version: node.version.clone(),
        kind: node.kind.clone(),
        description: node.description.clone(),
        runtime: node.runtime.clone(),
        enabled: node.enabled,
        mounted: node.mounted,
        state: node.state.clone(),
        error: node.error.clone(),
        restarts: 0,
        generation: 0,
        required_services: node.required_services.clone(),
        optional_services: node.optional_services.clone(),
        pending_services: node.pending_services.clone(),
        command: node.command.clone(),
        children: node.children.iter().map(from_framework_node).collect(),
    }
}

fn panel_node(shared: &SharedPanelObservation) -> PluginTreeNode {
    let panel = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (mounted, state, error, restarts, generation) = match &panel.handle {
        Some(handle) => {
            let state = handle.state();
            (
                state == PluginState::Ready,
                state.as_str().to_string(),
                state.error().map(str::to_string),
                handle.restarts(),
                handle.generation(),
            )
        }
        None if !panel.enabled => (false, "disabled".into(), None, 0, 0),
        None => (
            false,
            if panel.start_error.is_some() {
                "failed".into()
            } else {
                "pending".into()
            },
            panel.start_error.clone(),
            0,
            0,
        ),
    };
    PluginTreeNode {
        id: TUNNEL_PANEL_PLUGIN_ID.into(),
        name: "隧道面板".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        kind: "web-panel".into(),
        description: "端口映射、边缘路由与插件树的 Web 管理面".into(),
        runtime: "subprocess".into(),
        enabled: panel.enabled,
        mounted,
        state,
        error,
        restarts,
        generation,
        required_services: Vec::new(),
        optional_services: Vec::new(),
        pending_services: Vec::new(),
        command: panel.command.clone(),
        children: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_plugin::{BuiltinPlugin, PluginManifest};

    #[test]
    fn snapshot_reads_registry_and_marks_disabled_panel() {
        let root = PluginScope::root(Arc::new(echo_context::Ctx::default()));
        root.registry()
            .register(Arc::new(BuiltinPlugin::new(
                PluginManifest::builtin("test.plugin", "测试", "1.2.3", "test", "", ""),
                |_| Ok(Vec::new()),
            )))
            .unwrap();
        root.registry().mount_all(
            &echo_plugin::MountContext::new()
                .with_ctx(Arc::clone(root.ctx()))
                .with_tree(Arc::clone(&root)),
        );

        let panel = Arc::new(Mutex::new(PanelObservation::default()));
        let tree = snapshot(&root, &panel, vec![PLUGIN_TREE_SERVICE.into()]);
        assert_eq!(tree.plugins.len(), 2);
        assert_eq!(tree.plugins[0].id, "test.plugin");
        assert_eq!(tree.plugins[0].version, "1.2.3");
        assert_eq!(tree.plugins[0].state, "ready");
        assert!(tree.plugins[0].mounted);
        assert_eq!(tree.plugins[1].id, TUNNEL_PANEL_PLUGIN_ID);
        assert_eq!(tree.plugins[1].state, "disabled");
        assert_eq!(tree.host_services, vec![PLUGIN_TREE_SERVICE]);
    }
}
