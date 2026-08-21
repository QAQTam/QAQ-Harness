//! QAQ-Harness LSP client runtime.
//!
//! Connects to existing language servers (rust-analyzer, ...) over stdio
//! JSON-RPC and exposes semantic queries (hover / definition / document
//! symbols / diagnostics) to the agent — a semantic layer on top of the
//! text-based grep/read tooling.
//!
//! Stack: `lsp-types` 0.97 (LSP 3.16 + 3.17 proposed types) for the protocol
//! models, `lsp-server` 0.10 for message framing only (client-side dispatch
//! is implemented here).
//!
//! # Example
//!
//! ```no_run
//! use std::path::{Path, PathBuf};
//! use qaqh_lsp::{LspClient, ServerConfig};
//!
//! let mut client = LspClient::start(ServerConfig {
//!     root: PathBuf::from("."),
//!     ..Default::default()
//! }).expect("start rust-analyzer");
//! let uri = client.open_document(Path::new("src/lib.rs"), "rust", "fn main() {}")
//!     .expect("open document");
//! client.close_document(&uri).expect("close");
//! client.shutdown().expect("shutdown");
//! ```
//!
//! The doc-test above requires `rust-analyzer` on `PATH`; run it with
//! `cargo test --doc -- --ignored` if needed — it is not part of the default
//! test suite.

pub mod client;
pub mod query;
pub mod uri;

pub use client::{LspClient, LspError, ServerConfig};
pub use query::{position_1based, uri_of};

/// Re-export of the protocol models so callers do not need a direct
/// `lsp-types` dependency.
pub use lsp_types;
