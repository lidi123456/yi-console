//! Yi Console 客户端核心，供 Tauri 应用和 Web 模式共用。

pub mod api;
pub mod deploy;
pub mod forward;
pub mod hosts;
pub mod localbin;
pub mod preview;
pub mod rpc;
pub mod settings;
pub mod ssh;
pub mod sshconfig;

pub use api::{ClientHub, Sink};
