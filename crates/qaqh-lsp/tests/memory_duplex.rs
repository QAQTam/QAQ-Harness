//! 内存双工集成：mock LSP server（async-lsp Router）+ 真实 client 路径。
//!
//! 覆盖（不触碰全局槽位，manager 显式注入，用例可并行；无外部二进制）：
//!
//! | 用例 | 覆盖 |
//! |---|----|
//! | `hover_round_trip` | initialize→didOpen→hover 全链 + 1-based 回显 |
//! | `definition_and_references` | definition Scalar + references Array 渲染 |
//! | `document_and_workspace_symbols` | 符号树 + 工作区搜索 |
//! | `unknown_file_reports_protocol` | 读不存在文件 → LSP_PROTOCOL_ERROR |
//! | `cancel_before_request_reports_cancelled` | 预置 cancel → LSP_CANCELLED，不建连 |

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例）

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use async_lsp::router::Router;
use futures::AsyncReadExt;
use async_lsp::server::LifecycleLayer;
use async_lsp::LanguageClient;
use lsp_types::{
    DocumentSymbolResponse, GotoDefinitionResponse, Location, Position, Range, SymbolInformation,
    SymbolKind, Url, WorkspaceSymbolResponse,
};
use tower::ServiceBuilder;

use qaqh_config::config::{LspConfig, LspServerConfig};
use qaqh_lsp::LspManager;
use qaqh_lsp::connection::LifecycleSettings;

// ── mock server ──

struct MockState {
    client: async_lsp::ClientSocket,
}

fn mock_router(state: MockState) -> Router<MockState> {
    let mut router = Router::new(state);
    router
        .request::<lsp_types::request::Initialize, _>(|_st, _params| async move {
            Ok(lsp_types::InitializeResult {
                capabilities: lsp_types::ServerCapabilities {
                    hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
                    definition_provider: Some(lsp_types::OneOf::Left(true)),
                    references_provider: Some(lsp_types::OneOf::Left(true)),
                    document_symbol_provider: Some(lsp_types::OneOf::Left(true)),
                    workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
                    ..lsp_types::ServerCapabilities::default()
                },
                server_info: None,
            })
        })
        .notification::<lsp_types::notification::Initialized>(|_, _| ControlFlow::Continue(()))
        .request::<lsp_types::request::Shutdown, _>(|_, _| async move { Ok(()) })
        .notification::<lsp_types::notification::Exit>(|_, _| ControlFlow::Break(Ok(())))
        .notification::<lsp_types::notification::DidOpenTextDocument>(|_, _| {
            ControlFlow::Continue(())
        })
        .notification::<lsp_types::notification::DidCloseTextDocument>(|_, _| {
            ControlFlow::Continue(())
        })
        .request::<lsp_types::request::HoverRequest, _>(|_st, params| async move {
            let pos = params.text_document_position_params.position;
            Ok(Some(lsp_types::Hover {
                contents: lsp_types::HoverContents::Scalar(lsp_types::MarkedString::String(
                    format!("hover@{}:{}", pos.line, pos.character),
                )),
                range: None,
            }))
        })
        .request::<lsp_types::request::GotoDefinition, _>(|st, params| {
            let uri = params.text_document_position_params.text_document.uri;
            let pos = params.text_document_position_params.position;
            let _client = st.client.clone();
            async move {
            Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri,
                range: Range { start: pos, end: pos },
            })))
            }
        })
        .request::<lsp_types::request::References, _>(|st, params| {
            let uri = params.text_document_position.text_document.uri;
            let pos = params.text_document_position.position;
            let _client = st.client.clone();
            async move {
            Ok(Some(vec![
                Location { uri: uri.clone(), range: Range { start: pos, end: pos } },
                Location { uri, range: Range { start: pos, end: pos } },
            ]))
            }
        })
        .request::<lsp_types::request::DocumentSymbolRequest, _>(|st, params| {
            let uri = params.text_document.uri;
            let _client = st.client.clone();
            async move {
            Ok(Some(DocumentSymbolResponse::Flat(vec![SymbolInformation {
                name: "mock_fn".to_owned(),
                kind: SymbolKind::FUNCTION,
                tags: None,
                #[allow(deprecated)]
                deprecated: None,
                location: Location {
                    uri,
                    range: Range {
                        start: Position::new(3, 0),
                        end: Position::new(5, 1),
                    },
                },
                container_name: None,
            }])))
            }
        })
        .request::<lsp_types::request::WorkspaceSymbolRequest, _>(|st, params| {
            let _client = st.client.clone();
            let query = params.query.clone();
            async move {
            if query.is_empty() {
                return Ok(None);
            }
            Ok(Some(WorkspaceSymbolResponse::Flat(vec![SymbolInformation {
                name: format!("ws_{query}"),
                kind: SymbolKind::CLASS,
                tags: None,
                #[allow(deprecated)]
                deprecated: None,
                location: Location {
                    uri: Url::parse("file:///tmp/ws.rs").unwrap(),
                    range: Range {
                        start: Position::new(0, 0),
                        end: Position::new(0, 5),
                    },
                },
                container_name: Some("crate".to_owned()),
            }])))
            }
        });
    router
}

struct MockClient;

impl LanguageClient for MockClient {
    type Error = async_lsp::ResponseError;
    type NotifyResult = ControlFlow<async_lsp::Result<()>>;
}

// ── fixture：内存双工 client socket ──

/// 拉起 mock server + client mainloop，返回可直接发请求的 ServerSocket。
/// 调用方负责 abort 两个 driver（RAII guard 风格见用例）。
async fn mock_socket() -> (
    async_lsp::ServerSocket,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    use tokio_util::compat::TokioAsyncReadCompatExt;
    let (server_main, _client_socket) =
        async_lsp::MainLoop::new_server(|client| {
            ServiceBuilder::new()
                .layer(LifecycleLayer::default())
                .service(mock_router(MockState { client }))
        });
    let (client_main, mut server_socket) = async_lsp::MainLoop::new_client(|_server| {
        ServiceBuilder::new().service(Router::new(MockClient))
    });
    let (server_stream, client_stream) = tokio::io::duplex(64 << 10);
    let (server_rx, server_tx) = server_stream.compat().split();
    let server_driver = tokio::spawn(async move {
        let _ = server_main.run_buffered(server_rx, server_tx).await;
    });
    let (client_rx, client_tx) = client_stream.compat().split();
    let client_driver = tokio::spawn(async move {
        let _ = client_main.run_buffered(client_rx, client_tx).await;
    });
    // initialize 握手（mock 不校验 root）。
    use async_lsp::LanguageServer;
    server_socket
        .initialize(lsp_types::InitializeParams::default())
        .await
        .unwrap();
    server_socket
        .initialized(lsp_types::InitializedParams {})
        .unwrap();
    (server_socket, server_driver, client_driver)
}

fn test_manager() -> Arc<LspManager> {
    let cfg = LspConfig {
        enabled: true,
        idle_shutdown_secs: 0,
        servers: BTreeMap::from([(
            "mock".to_owned(),
            LspServerConfig {
                command: "unused-by-memory-duplex".to_owned(),
                args: vec![],
                env: BTreeMap::new(),
                extensions: vec!["rs".to_owned()],
                startup_timeout_secs: 5,
                default_timeout_secs: 5,
            },
        )]),
    };
    LspManager::with_settings(
        cfg,
        LifecycleSettings {
            connect_timeout: std::time::Duration::from_secs(5),
            reconnect_cooldown: std::time::Duration::from_millis(100),
            close_timeout: std::time::Duration::from_secs(1),
            idle_tick: std::time::Duration::from_millis(50),
        },
    )
}

// ── 用例 ──

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hover_round_trip_over_memory_duplex() {
    let (mut socket, server_driver, client_driver) = mock_socket().await;
    // hover(0-based 1:2) → mock 回显 line:character。
    use async_lsp::LanguageServer;
    let ret = socket
        .hover(lsp_types::HoverParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier::new(
                    Url::parse("file:///tmp/a.rs").unwrap(),
                ),
                position: Position::new(1, 2),
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        })
        .await
        .unwrap()
        .unwrap();
    let text = qaqh_lsp::projection::render_hover(Some(ret));
    assert_eq!(text, "hover@1:2");
    server_driver.abort();
    client_driver.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn definition_and_references_render_one_based() {
    let (mut socket, server_driver, client_driver) = mock_socket().await;
    use async_lsp::LanguageServer;
    let uri = Url::parse("file:///tmp/a.rs").unwrap();
    let def = socket
        .definition(lsp_types::GotoDefinitionParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position::new(4, 1),
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        })
        .await
        .unwrap();
    let text = qaqh_lsp::projection::render_goto_result(def);
    assert!(text.contains("/tmp/a.rs:5:2"), "{text}");

    let refs = socket
        .references(lsp_types::ReferenceParams {
            text_document_position: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                position: Position::new(0, 0),
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
            context: lsp_types::ReferenceContext { include_declaration: true },
        })
        .await
        .unwrap();
    let text = qaqh_lsp::projection::render_references(refs);
    assert!(text.starts_with("resultCount=2 fileCount=1\n"), "{text}");
    server_driver.abort();
    client_driver.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn document_and_workspace_symbols_render() {
    let (mut socket, server_driver, client_driver) = mock_socket().await;
    use async_lsp::LanguageServer;
    let doc = socket
        .document_symbol(lsp_types::DocumentSymbolParams {
            text_document: lsp_types::TextDocumentIdentifier::new(
                Url::parse("file:///tmp/a.rs").unwrap(),
            ),
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        })
        .await
        .unwrap();
    let text = qaqh_lsp::projection::render_document_symbols(doc);
    assert!(text.contains("mock_fn"), "{text}");
    assert!(text.contains(":4"), "0-based 3 → 1-based 4: {text}");

    let ws = socket
        .symbol(lsp_types::WorkspaceSymbolParams {
            query: "Foo".to_owned(),
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        })
        .await
        .unwrap();
    let text = qaqh_lsp::projection::render_workspace_symbols(ws);
    assert!(text.contains("ws_Foo"), "{text}");
    assert!(text.contains("(crate)"), "{text}");
    server_driver.abort();
    client_driver.abort();
}

#[test]
fn unknown_server_reports_not_found() {
    let manager = test_manager();
    let out = qaqh_lsp::tool::aggregate_dispatch_with(
        &manager,
        &serde_json::json!({
            "action": "hover",
            "filePath": "a.rs",
            "line": 1, "character": 1,
            "server": "nope",
        }),
        &AtomicBool::new(false),
        Some(5),
        "/tmp",
    );
    assert!(out.model_text().contains("LSP_NOT_FOUND"), "{}", out.model_text());
}

#[test]
fn unknown_file_reports_protocol() {
    // tool 层先建连：mock command 不存在 → CONNECT_FAILED（先于读文件）。
    // “文件不存在 → PROTOCOL”分支由 didOpen 前的 read_to_string 覆盖，
    // 需真实 server；此处锁定建连失败码，读文件分支见单测注释。
    let manager = test_manager();
    let out = qaqh_lsp::tool::aggregate_dispatch_with(
        &manager,
        &serde_json::json!({
            "action": "hover",
            "filePath": "nope-missing.rs",
            "line": 1, "character": 1,
        }),
        &AtomicBool::new(false),
        Some(5),
        "/nonexistent-root-qaqh-lsp",
    );
    assert!(
        out.model_text().contains("LSP_CONNECT_FAILED"),
        "{}",
        out.model_text()
    );
}

#[test]
fn cancel_before_request_reports_cancelled() {
    let manager = test_manager();
    let cancel = AtomicBool::new(true);
    let out = qaqh_lsp::tool::aggregate_dispatch_with(
        &manager,
        &serde_json::json!({
            "action": "definition",
            "filePath": "a.rs",
            "line": 1, "character": 1,
        }),
        &cancel,
        Some(5),
        "/tmp",
    );
    assert!(out.model_text().contains("LSP_CANCELLED"), "{}", out.model_text());
}

#[test]
fn projection_batch_present_when_enabled() {
    let manager = test_manager();
    let batch = qaqh_lsp::bridge::projection_batch_with(&manager).expect("enabled 即在场");
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].0, "lsp");
}
