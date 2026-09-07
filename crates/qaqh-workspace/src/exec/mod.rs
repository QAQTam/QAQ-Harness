//! exec — 命令执行工具（bash/pwsh 共享引擎）。
//!
//! 由单文件 `exec.rs` 拆分（Phase 2-2）：各子模块按既有分段切分，对外 API 不变。
//! 唯一对外入口为 [`register`]（`registration.rs` 调用）与 `pub(super)` handler
//!（测试经 `super::*` 可达）。

pub mod direct;
pub mod handler;
pub mod pipe;
pub mod register;
pub mod shell;
pub mod truncate;

pub use register::register;

#[cfg(test)]
pub(crate) use direct::direct_exec;
#[cfg(test)]
pub(crate) use handler::{
    detect_background_derivation, handle_run_bash, handle_run_pwsh, normalize_command_rg,
    normalize_rg_argv, shell_available,
};
#[cfg(test)]
pub(crate) use pipe::{PipePumpCtx, Readiness, drain_pipe_to_registry};
#[cfg(test)]
pub(crate) use register::register_shell_tool;
#[cfg(test)]
pub(crate) use shell::{Shell, base64_decode, executable_in_dirs, ps_encode};
#[cfg(test)]
pub(crate) use truncate::token_truncate;

#[cfg(test)]
mod tests;
