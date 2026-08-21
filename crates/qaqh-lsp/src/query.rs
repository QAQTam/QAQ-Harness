//! Semantic query layer over [`LspClient`]: document sync and the four
//! query families the agent needs (hover / definition / document symbols /
//! diagnostics).
//!
//! Request parameters are built as JSON values (thin, protocol-shaped);
//! responses deserialize into the strongly-typed `lsp_types` models.

use std::path::Path;
use std::str::FromStr;

use lsp_types::{
    Diagnostic, DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentSymbolResponse,
    GotoDefinitionResponse, Hover, Position, PublishDiagnosticsParams, TextDocumentIdentifier,
    TextDocumentItem, Uri,
};
use serde_json::json;

use crate::client::{LspClient, LspError};
use crate::uri::path_to_uri;

/// Convert an absolute filesystem path to an `lsp_types::Uri`.
pub fn uri_of(path: &Path) -> Result<Uri, LspError> {
    let s = path_to_uri(path);
    Uri::from_str(&s).map_err(|e| LspError::BadUri {
        uri: s,
        reason: e.to_string(),
    })
}

/// Build an LSP 0-based `Position` from a 1-based line/column pair (the
/// display convention used across QAQ-Harness tooling).
pub fn position_1based(line: u32, column: u32) -> Position {
    Position {
        line: line.saturating_sub(1),
        character: column.saturating_sub(1),
    }
}

impl LspClient {
    /// Open a document (`textDocument/didOpen`, full content sync). Returns
    /// the document URI for subsequent queries.
    pub fn open_document(
        &mut self,
        path: &Path,
        language_id: &str,
        text: &str,
    ) -> Result<Uri, LspError> {
        let uri = uri_of(path)?;
        let version = self.doc_version;
        self.doc_version += 1;
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: language_id.to_owned(),
                version,
                text: text.to_owned(),
            },
        };
        self.notify("textDocument/didOpen", serde_json::to_value(params)?)?;
        Ok(uri)
    }

    /// Close a document (`textDocument/didClose`).
    pub fn close_document(&mut self, uri: &Uri) -> Result<(), LspError> {
        let params = DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
        };
        self.notify("textDocument/didClose", serde_json::to_value(params)?)?;
        Ok(())
    }

    /// `textDocument/hover` at a 1-based line/column.
    pub fn hover(&mut self, uri: &Uri, line: u32, column: u32) -> Result<Option<Hover>, LspError> {
        let params = json!({
            "textDocument": { "uri": uri.as_str() },
            "position": position_1based(line, column),
        });
        self.request("textDocument/hover", params)
    }

    /// `textDocument/definition` at a 1-based line/column.
    pub fn definition(
        &mut self,
        uri: &Uri,
        line: u32,
        column: u32,
    ) -> Result<GotoDefinitionResponse, LspError> {
        let params = json!({
            "textDocument": { "uri": uri.as_str() },
            "position": position_1based(line, column),
        });
        self.request("textDocument/definition", params)
    }

    /// `textDocument/documentSymbol` — hierarchical or flat, depending on
    /// what the server returned.
    pub fn document_symbols(&mut self, uri: &Uri) -> Result<DocumentSymbolResponse, LspError> {
        let params = json!({ "textDocument": { "uri": uri.as_str() } });
        self.request("textDocument/documentSymbol", params)
    }

    /// Consume queued `textDocument/publishDiagnostics` notifications for
    /// `uri` (all other queued notifications are returned to the caller via
    /// [`LspClient::drain_notifications`] and are not consumed here).
    pub fn diagnostics(&mut self, uri: &Uri) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        let drained = self.drain_notifications();
        for n in drained {
            if n.method != "textDocument/publishDiagnostics" {
                continue;
            }
            if let Ok(params) = serde_json::from_value::<PublishDiagnosticsParams>(n.params) {
                if &params.uri == uri {
                    out.extend(params.diagnostics);
                }
            }
        }
        out
    }
}
