//! Shared engine boundaries. The legacy backends remain complete-index adapters.
pub mod budget;
pub mod file_source;
pub mod ids;
pub mod recursive;
pub mod resident_memory;
pub mod session;
pub mod top_level;
pub mod tree;
pub mod tree_navigation;

/// S1 sessions use the existing complete-index document as their adapter.
pub use crate::document::Document as Session;

/// Experimental S3 disk-backed sparse scanner; not used by the GUI.
pub mod sparse;

pub mod search;
