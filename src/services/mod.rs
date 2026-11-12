//! The services that need no capture: the shared clipboard, the shells for
//! the KVM's terminal panel, file reception, and updates from the KVM.

pub mod clipboard;
mod credit;
pub mod file_transfer;
pub mod terminal;
pub mod update;
