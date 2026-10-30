//! 服务器状态 + Docker 管理面板（库部分）。
//!
//! 拆成 lib + bin 是为了让 `api` / `sys` / `docker` 都能被集成测试直接驱动，
//! 不必先起一个真进程。

pub mod api;
pub mod docker;
pub mod plugins;
pub mod sys;
pub mod web;
