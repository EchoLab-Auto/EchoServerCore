//! `esc-control-api`：面板 ↔ 宿主 ↔ 引擎 之间唯一的共享契约。
//!
//! 刻意做得很薄：只有数据模型、方法名、JSON 信封和一个 trait。
//! 任何一端（图形界面 / 隧道内核）都可以在不碰另一端的情况下重写。

pub mod dispatch;
pub mod model;

pub use dispatch::{
    dispatch, method, ControlRequest, ControlResponse, TunnelControl, CONTROL_SERVICE,
};
pub use model::{
    ControlError, EngineStatus, ErrorKind, ExportedConfig, Mapping, MappingState, MappingStats,
    MappingUpsert, MappingView, Role, SpokeView,
};
