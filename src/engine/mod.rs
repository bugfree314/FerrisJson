//! Shared engine boundaries. The legacy backends remain complete-index adapters.
pub mod budget;
pub mod ids;
pub mod session;

/// S1 sessions use the existing complete-index document as their adapter.
pub use crate::document::Document as Session;
