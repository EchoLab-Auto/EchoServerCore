//! `esc-control-api`：面板 ↔ 宿主 ↔ 引擎 之间唯一的共享契约。
//!
//! 刻意做得很薄：只有数据模型、方法名、JSON 信封和一个 trait。
//! 任何一端（图形界面 / 隧道内核）都可以在不碰另一端的情况下重写。

pub mod dispatch;
pub mod model;
pub mod plugin_tree;

pub use dispatch::{
    dispatch, dispatch_edge, method, ControlRequest, ControlResponse, EdgeRouteControl,
    TunnelControl, CONTROL_SERVICE, EDGE_ROUTE_SERVICE,
};
pub use model::{
    ControlError, EdgeRoute, EdgeRouteUpsert, EdgeRouterCounters, EdgeRouterStatus, EngineStatus,
    ErrorKind, ExportedConfig, Mapping, MappingState, MappingStats, MappingUpsert, MappingView,
    Role, SpokeView,
};
pub use plugin_tree::{
    PluginTreeNode, PluginTreeSnapshot, PLUGIN_TREE_METHOD, PLUGIN_TREE_SERVICE,
};
