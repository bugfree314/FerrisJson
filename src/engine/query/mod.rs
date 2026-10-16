//! Structural query entry points. No dependency on the text-search matcher or hub.
mod adapters;
mod plan;
mod request;
mod syntax;
mod predicate;
pub mod stream;

pub use plan::CompiledQuery;
pub use request::QueryRequest;
use crate::{
    document::{Document, NodeIds},
    engine::sparse::{Node, SparseIndex},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Memory,
    Indexed,
    Sparse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    Supported,
    ScalarSubset,
    /// Only the functions and grammar actually provided by serde_json_path.
    LibraryDefined,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cancellation {
    /// Check before/after library evaluation and during node-ID conversion.
    BetweenLibraryCalls,
    DuringTraversal,
}

#[derive(Clone, Copy, Debug)]
pub struct Capabilities {
    pub pointer: Support,
    pub selectors: Support,
    pub filters: Support,
    pub functions: Support,
    pub cancellation: Cancellation,
    pub incremental_results: bool,
}

impl Backend {
    /// Streaming API capabilities; Memory reports its worst-case library fallback.
    /// The older NodeIds compatibility entry points retain their D-stage contract.
    pub fn capabilities(self) -> Capabilities {
        match self {
            Self::Memory => Capabilities {
                pointer: Support::Supported,
                selectors: Support::LibraryDefined,
                filters: Support::LibraryDefined,
                functions: Support::LibraryDefined,
                cancellation: Cancellation::BetweenLibraryCalls,
                incremental_results: false,
            },
            Self::Indexed => Capabilities {
                pointer: Support::Supported,
                selectors: Support::Supported,
                filters: Support::ScalarSubset,
                functions: Support::Unsupported,
                cancellation: Cancellation::DuringTraversal,
                incremental_results: true,
            },
            Self::Sparse => Capabilities {
                pointer: Support::Supported,
                selectors: Support::Supported,
                filters: Support::ScalarSubset,
                functions: Support::Unsupported,
                cancellation: Cancellation::DuringTraversal,
                incremental_results: true,
            },
        }
    }
}

pub struct QueryEngine;

impl QueryEngine {
    pub fn run(input: stream::Input, request: &QueryRequest, generation: u64, cancelled: &dyn Fn() -> bool, notify: &mut dyn FnMut(stream::Event) -> Result<(), String>) -> Result<crate::engine::results::ResultStore, String> {
        stream::run(input, request, generation, cancelled, notify)
    }
    pub fn execute(doc: &Document, request: &QueryRequest, cancelled: &dyn Fn() -> bool) -> Result<NodeIds, String> {
        Self::document(doc, &request.expression, cancelled)
    }

    pub fn backend(doc: &Document) -> Backend {
        if doc.value().is_some() { Backend::Memory } else { Backend::Indexed }
    }

    pub fn document(doc: &Document, expression: &str, cancelled: &dyn Fn() -> bool) -> Result<NodeIds, String> {
        CompiledQuery::compile(doc, expression, cancelled)?.execute(doc, cancelled)
    }

    pub fn find(doc: &Document, expression: &str, cancelled: &dyn Fn() -> bool) -> Result<usize, String> {
        let ids = Self::document(doc, expression, cancelled)?;
        match ids.as_slice() {
            [id] => Ok(*id as usize),
            [] => Err(format!("Path not found: {expression}")),
            _ => Err("Path selected multiple nodes; use --query to retrieve all results".into()),
        }
    }

    /// Preserve Sparse's exact single-Pointer input/error contract, including empty root.
    /// General JSONPath is deliberately not routed through a full-document Value fallback.
    pub fn sparse_pointer(index: &mut SparseIndex, expression: &str, cancelled: &dyn Fn() -> bool) -> Result<Node, String> {
        if cancelled() { return Err("Query cancelled".into()); }
        adapters::sparse::pointer(index, expression, cancelled)
    }
}
