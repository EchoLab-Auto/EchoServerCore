//! 宿主插件树的只读契约。
//!
//! 这里描述的是 `esc-host` 的**运行时事实**，不是隧道引擎状态：进程内插件
//! 来自框架的 `PluginRegistry::descriptors()`，父子关系来自 `PluginScope`，
//! 子进程插件来自 `PluginHandle`。面板只消费这份 JSON，不反向依赖宿主或框架。

use serde::{Deserialize, Serialize};

/// 查询宿主插件树的宿主服务名。
pub const PLUGIN_TREE_SERVICE: &str = "esc.plugins";
/// 查询整棵插件树的方法名。
pub const PLUGIN_TREE_METHOD: &str = "tree";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginTreeSnapshot {
    /// JSON 形状版本；与插件框架的线上协议版本不是一回事。
    pub schema_version: u32,
    pub host: String,
    pub framework: String,
    /// 根 `Ctx` 当前实际可用的服务键。
    pub context_services: Vec<String>,
    /// 注册给子进程插件的 HostCall 服务。
    pub host_services: Vec<String>,
    /// 根作用域直属插件；嵌套插件在各自节点的 `children` 里。
    pub plugins: Vec<PluginTreeNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginTreeNode {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: String,
    pub description: String,
    /// `in_process` / `subprocess`。
    pub runtime: String,
    /// 配置是否希望它运行。
    pub enabled: bool,
    /// 进程内插件是否真的挂载；子进程以 Ready 状态视为 mounted。
    pub mounted: bool,
    /// ready / pending / failed / stopped / disabled 等实际状态。
    pub state: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub restarts: u32,
    #[serde(default)]
    pub generation: u64,
    /// 框架 `Inject::required`：硬依赖的是服务键，不是猜测的插件 id。
    #[serde(default)]
    pub required_services: Vec<String>,
    /// 框架 `Inject::optional`。
    #[serde(default)]
    pub optional_services: Vec<String>,
    /// 此刻仍缺失、正在等待的硬依赖。
    #[serde(default)]
    pub pending_services: Vec<String>,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub children: Vec<PluginTreeNode>,
}
