//! The JSONL event store (MVP §2). Append-only, monthly partitions, no database.
//!
//! `scanner` is the single read path; nothing outside this module opens event
//! files directly.

pub mod id;
pub mod paths;
pub mod record;
pub mod scanner;
pub mod writer;

pub use id::{event_id, text_hash, EventIdentity};
pub use paths::{expand_tilde, Partition, StorePaths};
pub use record::{Event, IngestCursor, PromptRecord, ToolCall, RECORD_VERSION};
pub use scanner::{Scan, ScanQuery, ScanStats, Scanner};
pub use writer::StoreWriter;
