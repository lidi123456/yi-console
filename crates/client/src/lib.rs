//! Yi Console client core, shared by the Tauri app and the web mode.

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
