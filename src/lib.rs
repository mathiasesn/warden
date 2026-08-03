//! warden — a local, read-only library and CLI over coding-agent session logs.
//!
//! warden never calls a model provider: there is no network code in the crate,
//! and no dependency capable of making a request.

pub mod adapters;
pub mod cli;
pub mod commands;
pub mod config;
pub mod doctor;
pub mod ingest;
pub mod output;
pub mod store;
