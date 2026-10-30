//! `esc.plugins`：把框架的真实注册表/作用域树投影成只读 JSON。
//!
//! 这层只做观测，不提供启停或挂载操作。两个 Web 面板看到的是同一份契约，
//! 不需要、也不能直接触碰宿主的 `PluginRegistry`。

use std::sync::{Arc, Mutex};

use echo_plugin::{PluginDescriptor, PluginScope};
use echo_plugin_host::host_service::HostService;
use echo_plugin_host::supervisor::{PluginHandle, PluginState};
use esc_control_api::{
    PluginTreeNode, PluginTreeSnapshot, PLUGIN_TREE_METHOD, PLUGIN_TREE_SERVICE,
};
use serde_json::Value;

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
}

impl PluginTreeHostService {
    pub fn new(
        root: Arc<PluginScope>,
        panel: SharedPanelObservation,
        host_services: Vec<String>,
    ) -> Self {
        Self {
            root,
            panel,
            host_services,
        }
    }

    pub fn snapshot(&self) -> PluginTreeSnapshot {
        snapshot(&self.root, &self.panel, self.host_services.clone())
    }
}

impl HostService for PluginTreeHostService {
    fn call(&self, method: &str, _payload: Value) -> Result<Value, (String, String)> {
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

/// 从框架对象生成快照。递归只读 `PluginScope::children_of`，不根据 id 或
/// 配置猜父子关系；服务依赖同样原样采用 descriptor 的 Inject 声明。
pub fn snapshot(
    root: &Arc<PluginScope>,
    panel: &SharedPanelObservation,
    host_services: Vec<String>,
) -> PluginTreeSnapshot {
    let mut plugins = scope_nodes(root);
    plugins.push(panel_node(panel));
    PluginTreeSnapshot {
        schema_version: 1,
        host: "esc-host".into(),
        framework: "plugin-framework".into(),
        context_services: root.ctx().keys(),
        host_services,
        plugins,
    }
}

fn scope_nodes(scope: &Arc<PluginScope>) -> Vec<PluginTreeNode> {
    scope
        .registry()
        .descriptors()
        .into_iter()
        .map(|descriptor| {
            let children = scope
                .children_of(&descriptor.id)
                .iter()
                .flat_map(scope_nodes)
                .collect();
            in_process_node(scope, descriptor, children)
        })
        .collect()
}

fn in_process_node(
    scope: &Arc<PluginScope>,
    descriptor: PluginDescriptor,
    children: Vec<PluginTreeNode>,
) -> PluginTreeNode {
    let mounted = scope.registry().is_mounted(&descriptor.id);
    let state = if !descriptor.enabled {
        "disabled"
    } else if mounted {
        "ready"
    } else if !descriptor.pending_services.is_empty() {
        "pending"
    } else if descriptor.error.is_some() {
        "failed"
    } else {
        "stopped"
    };
    PluginTreeNode {
        id: descriptor.id,
        name: descriptor.name,
        version: descriptor.version,
        kind: descriptor.kind,
        description: descriptor.description,
        runtime: descriptor.runtime,
        enabled: descriptor.enabled,
        mounted,
        state: state.into(),
        error: descriptor.error,
        restarts: 0,
        generation: 0,
        required_services: descriptor.required_services,
        optional_services: descriptor.optional_services,
        pending_services: descriptor.pending_services,
        command: descriptor.command,
        children,
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
