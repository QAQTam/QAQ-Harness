//! End-to-end smoke test against a real language server (rust-analyzer).
//!
//! Ignored by default (needs `rust-analyzer` on PATH and a few seconds of
//! workspace loading). Run with:
//!
//! ```text
//! cargo test -p qaqh-lsp --test e2e -- --ignored --nocapture
//! ```
//!
//! Skips (does not fail) when the server binary is unavailable.

use std::path::{Path, PathBuf};
use std::time::Duration;

// `MarkedString` is deprecated by LSP 3.18 but still handled as a fallback
// for servers that have not migrated to `MarkupContent`.
#[allow(deprecated)]
use qaqh_lsp::gen_lsp_types::{
    Contents, Definition, DefinitionResponse, DocumentSymbolResponse, Location, MarkedString,
};
use qaqh_lsp::LspClient;
use qaqh_lsp::ServerConfig;

/// Workspace root: `crates/qaqh-lsp/../..`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("manifest dir has parents")
        .to_path_buf()
}

fn find_line(text: &str, needle: &str) -> Option<u32> {
    text.lines()
        .position(|l| l.contains(needle))
        .map(|i| i as u32 + 1)
}

#[test]
#[ignore]
fn rust_analyzer_smoke() {
    // Skip cleanly when the server is not installed.
    if std::process::Command::new("rust-analyzer")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("SKIP: rust-analyzer not on PATH");
        return;
    }

    let root = workspace_root();
    let mut client = LspClient::start(ServerConfig {
        command: "rust-analyzer".to_owned(),
        args: vec!["--no-watch".to_owned()],
        root: root.clone(),
        client_name: "qaqh-lsp-e2e".to_owned(),
        timeout: Duration::from_secs(90),
        ..Default::default()
    })
    .expect("start rust-analyzer");

    // Open a real source file from this crate.
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/uri.rs");
    let text = std::fs::read_to_string(&file).expect("read uri.rs");
    let uri = client
        .open_document(&file, "rust", &text)
        .expect("didOpen uri.rs");

    // Wait for the server to analyse and publish diagnostics (async).
    let mut saw_diagnostics = false;
    for _ in 0..40 {
        if !client.diagnostics(&uri).is_empty() {
            saw_diagnostics = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(
        saw_diagnostics,
        "rust-analyzer should publish diagnostics after didOpen"
    );

    // documentSymbol: expect a hierarchical list containing `path_to_uri`.
    let symbols = client.document_symbols(&uri).expect("documentSymbol");
    match symbols {
        DocumentSymbolResponse::DocumentSymbolList(symbols) => {
            assert!(!symbols.is_empty(), "documentSymbol returned no symbols");
            assert!(
                symbols.iter().any(|s| s.name == "path_to_uri"),
                "documentSymbol should contain `path_to_uri`, got: {:?}",
                symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }
        DocumentSymbolResponse::SymbolInformationList(symbols) => {
            assert!(
                symbols
                    .iter()
                    .any(|s| s.base_symbol_information.name == "path_to_uri"),
                "documentSymbol should contain `path_to_uri`"
            );
        }
    }

    // hover + definition on `pub fn path_to_uri(...)`.
    let line = find_line(&text, "pub fn path_to_uri").expect("find path_to_uri line");
    let hover = client.hover(&uri, line, 8).expect("hover request");
    let hover = hover.expect("hover should resolve for a known function");
    let hover_text = match &hover.contents {
        Contents::MarkupContent(m) => m.value.clone(),
        Contents::MarkedString(s) => marked_string_text(s),
        Contents::MarkedStringList(items) => items
            .iter()
            .map(marked_string_text)
            .collect::<Vec<_>>()
            .join("\n"),
    };
    assert!(
        !hover_text.trim().is_empty(),
        "hover contents should be non-empty"
    );

    let definition = client
        .definition(&uri, line, 8)
        .expect("definition request");
    let location = match definition {
        DefinitionResponse::Definition(Definition::Location(loc)) => loc,
        DefinitionResponse::Definition(Definition::LocationList(mut locs)) => locs.remove(0),
        DefinitionResponse::DefinitionLinkList(mut links) => {
            let link = links.remove(0);
            Location {
                uri: link.target_uri,
                range: link.target_range,
            }
        }
    };
    assert_eq!(
        location.uri, uri,
        "definition should point inside the same file"
    );

    client.shutdown().expect("graceful shutdown");
}

/// Legacy `MarkedString` hover contents: deprecated by 3.18 but still sent
/// by some servers, so the smoke test keeps a fallback path for it.
#[allow(deprecated)]
fn marked_string_text(s: &MarkedString) -> String {
    match s {
        MarkedString::String(t) => t.clone(),
        MarkedString::MarkedStringWithLanguage(ls) => ls.value.clone(),
    }
}
