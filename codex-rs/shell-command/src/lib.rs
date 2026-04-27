//! Command parsing and safety utilities shared across Codex crates.

mod shell_detect;

pub mod bash;
pub(crate) mod command_safety;
mod metadata_write;
pub mod parse_command;
pub mod powershell;

pub use command_safety::is_dangerous_command;
pub use command_safety::is_safe_command;
pub use metadata_write::metadata_write_forbidden_reason;
