pub mod ann;
pub mod assembler;
pub mod embedding;
pub mod index_db;
pub mod intent_filter;
pub mod orchestrator;
pub mod retrieval;
pub mod rule_render;
pub mod rule_source;
pub mod types;

// Embedding/vector-lane health probe, re-exported at the `context` level so
// both difflore-core and difflore-cli can resolve it without reaching into
// `index_db`.
pub use index_db::{
    EmbeddingDiagnostics, gather_embedding_diagnostics, gather_embedding_diagnostics_with_activity,
};

/// Default number of rules injected into context. Keep aligned with the local
/// recall budget.
pub const DEFAULT_TOP_K_RULES: usize = 3;
