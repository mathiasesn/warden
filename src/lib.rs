//! warden — a local, read-only library and CLI over coding-agent session logs.
//!
//! warden never calls a model provider: there is no network code in the crate,
//! and no dependency capable of making a request.

pub mod cli;
pub mod config;
pub mod store;
