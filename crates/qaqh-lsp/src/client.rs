//! LSP client runtime: spawn a language server over stdio JSON-RPC, perform
//! the initialize handshake, dispatch requests, and manage the process
//! lifecycle.
//!
//! Transport framing (`Content-Length` headers) is handled by
//! `lsp_server::Message::read` / `Message::write`; only the message types are
//! reused from `lsp-server` — the dispatch loop and handshake are client-side
//! here.

use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus};
use std::str::FromStr;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lsp_server::{Message, Notification, Request, RequestId, Response, ResponseError};
use gen_lsp_types::{
    ClientCapabilities, ClientInfo, DefinitionClientCapabilities, DiagnosticsCapabilities,
    DocumentSymbolClientCapabilities, HoverClientCapabilities, InitializeParams, InitializeResult,
    MarkupKind, PublishDiagnosticsClientCapabilities, ServerCapabilities,
    TextDocumentClientCapabilities, TextDocumentSyncClientCapabilities, Uri,
    WorkspaceClientCapabilities, WorkspaceFolder, WorkspaceFolders,
    WorkspaceFoldersInitializeParams,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;

use crate::uri::path_to_uri;

/// Errors raised by the LSP client runtime.
#[derive(Debug, Error)]
pub enum LspError {
    #[error("failed to spawn language server `{0}`: {1}")]
    Spawn(String, std::io::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("timeout waiting for `{method}` response after {secs}s")]
    Timeout { method: String, secs: u64 },
    #[error("server error for `{method}`: {message} (code {code})")]
    ServerError {
        method: String,
        code: i32,
        message: String,
    },
    #[error("language server exited unexpectedly: {status:?}")]
    ServerExited { status: Option<ExitStatus> },
    #[error("connection closed by server while waiting for `{method}`")]
    Closed { method: String },
    #[error("invalid response for `{method}`: {message}")]
    Deserialize { method: String, message: String },
    #[error("invalid uri `{uri}`: {reason}")]
    BadUri { uri: String, reason: String },
    #[error("internal mutex poisoned: {0}")]
    Poisoned(&'static str),
}

/// How to launch and configure a language server.
#[derive(Clone)]
pub struct ServerConfig {
    /// Server executable (resolved via `PATH`).
    pub command: String,
    /// Extra CLI arguments passed to the server.
    pub args: Vec<String>,
    /// Workspace root; becomes `rootUri` in the initialize handshake.
    pub root: PathBuf,
    /// Client name reported in `clientInfo`.
    pub client_name: String,
    /// Client version reported in `clientInfo`.
    pub client_version: String,
    /// Per-request timeout.
    pub timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            command: "rust-analyzer".to_owned(),
            args: Vec::new(),
            root: PathBuf::from("."),
            client_name: "QAQ-Harness".to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            timeout: Duration::from_secs(30),
        }
    }
}

type PendingMap = Arc<Mutex<HashMap<RequestId, Sender<Result<Value, ResponseError>>>>>;
type NotificationQueue = Arc<Mutex<VecDeque<Notification>>>;
type SharedWriter = Arc<Mutex<BufWriter<ChildStdin>>>;

/// A connected language server.
pub struct LspClient {
    child: Child,
    writer: SharedWriter,
    pending: PendingMap,
    notifications: NotificationQueue,
    next_id: i32,
    pub(crate) doc_version: i32,
    timeout: Duration,
    server_capabilities: ServerCapabilities,
    shutdown_sent: bool,
}

impl LspClient {
    /// Spawn the server and perform the initialize / initialized handshake.
    ///
    /// On success the server is ready for document sync and semantic
    /// queries. The child process is reaped on `shutdown()` or `Drop`.
    pub fn start(config: ServerConfig) -> Result<Self, LspError> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|e| LspError::Spawn(config.command.clone(), e))?;

        let stdin = child.stdin.take().ok_or_else(|| {
            LspError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                format!("no stdin on `{}`", config.command),
            ))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            LspError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                format!("no stdout on `{}`", config.command),
            ))
        })?;

        let writer = Arc::new(Mutex::new(BufWriter::new(stdin)));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let notifications: NotificationQueue = Arc::new(Mutex::new(VecDeque::new()));
        spawn_reader(
            stdout,
            writer.clone(),
            pending.clone(),
            notifications.clone(),
        );

        let mut client = Self {
            child,
            writer,
            pending,
            notifications,
            next_id: 1,
            doc_version: 1,
            timeout: config.timeout,
            server_capabilities: ServerCapabilities::default(),
            shutdown_sent: false,
        };

        let uri_str = path_to_uri(&config.root);
        let root_uri = Uri::from_str(&uri_str).map_err(|e| LspError::BadUri {
            uri: uri_str,
            reason: e.to_string(),
        })?;
        let params = InitializeParams {
            process_id: Some(std::process::id() as i32),
            #[allow(deprecated)]
            root_uri: Some(root_uri.clone()),
            capabilities: client_capabilities(),
            client_info: Some(ClientInfo {
                name: config.client_name,
                version: Some(config.client_version),
            }),
            workspace_folders_initialize_params: WorkspaceFoldersInitializeParams {
                workspace_folders: Some(WorkspaceFolders::WorkspaceFolderList(vec![
                    WorkspaceFolder {
                        uri: root_uri,
                        name: config
                            .root
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "workspace".to_owned()),
                    },
                ])),
            },
            ..Default::default()
        };
        let result: InitializeResult =
            client.request("initialize", serde_json::to_value(params)?)?;
        client.server_capabilities = result.capabilities;
        client.notify("initialized", serde_json::json!({}))?;
        Ok(client)
    }

    /// Server capabilities announced in the initialize response.
    pub fn server_capabilities(&self) -> &ServerCapabilities {
        &self.server_capabilities
    }

    /// The per-request timeout in effect.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Send a request and block until the matching response arrives.
    ///
    /// The response is deserialized into `R`; server error responses and
    /// timeouts are reported as `LspError` variants. The pending entry is
    /// always cleaned up, including on timeout.
    pub fn request<R: DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<R, LspError> {
        let id = RequestId::from(self.next_id);
        self.next_id += 1;
        let (tx, rx): (Sender<Result<Value, ResponseError>>, _) = mpsc::channel();
        {
            let Ok(mut pending) = self.pending.lock() else {
                return Err(LspError::Poisoned("pending"));
            };
            pending.insert(id.clone(), tx);
        }
        let msg = Message::Request(Request::new(id.clone(), method.to_owned(), params));
        {
            let Ok(mut w) = self.writer.lock() else {
                return Err(LspError::Poisoned("writer"));
            };
            msg.write(&mut *w)?;
            w.flush()?;
        }
        match rx.recv_timeout(self.timeout) {
            Ok(Ok(value)) => serde_json::from_value(value).map_err(|e| LspError::Deserialize {
                method: method.to_owned(),
                message: e.to_string(),
            }),
            Ok(Err(err)) => Err(LspError::ServerError {
                method: method.to_owned(),
                code: err.code,
                message: err.message,
            }),
            Err(RecvTimeoutError::Timeout) => {
                drop_pending(&self.pending, &id);
                Err(LspError::Timeout {
                    method: method.to_owned(),
                    secs: self.timeout.as_secs(),
                })
            }
            Err(RecvTimeoutError::Disconnected) => {
                drop_pending(&self.pending, &id);
                Err(LspError::Closed {
                    method: method.to_owned(),
                })
            }
        }
    }

    /// Send a fire-and-forget notification.
    pub fn notify(&mut self, method: &str, params: Value) -> Result<(), LspError> {
        let msg = Message::Notification(Notification::new(method.to_owned(), params));
        let Ok(mut w) = self.writer.lock() else {
            return Err(LspError::Poisoned("writer"));
        };
        msg.write(&mut *w)?;
        w.flush()?;
        Ok(())
    }

    /// Take all notifications received so far (publishDiagnostics,
    /// window/logMessage, ...). Consumed in FIFO order.
    pub fn drain_notifications(&mut self) -> Vec<Notification> {
        let Ok(mut q) = self.notifications.lock() else {
            return Vec::new();
        };
        q.drain(..).collect()
    }

    /// Graceful shutdown: `shutdown` request, `exit` notification, then wait
    /// up to ~5s before force-killing the child.
    pub fn shutdown(&mut self) -> Result<(), LspError> {
        if self.shutdown_sent {
            return Ok(());
        }
        self.shutdown_sent = true;
        let _: Value = self.request("shutdown", Value::Null)?;
        let _ = self.notify("exit", Value::Null);
        for _ in 0..50 {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        Ok(())
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        if !self.shutdown_sent {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn drop_pending(pending: &PendingMap, id: &RequestId) {
    if let Ok(mut p) = pending.lock() {
        p.remove(id);
    }
}

/// Reader thread: decodes frames, routes responses to their pending channel,
/// queues notifications, and answers server-initiated requests with
/// `MethodNotFound` (we never serve them).
fn spawn_reader(
    stdout: ChildStdout,
    writer: SharedWriter,
    pending: PendingMap,
    notifications: NotificationQueue,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let msg = match Message::read(&mut reader) {
                Ok(Some(msg)) => msg,
                Ok(None) => break,
                Err(err) => {
                    log::debug!("lsp reader error: {err}");
                    break;
                }
            };
            match msg {
                Message::Response(resp) => {
                    if let Ok(mut p) = pending.lock() {
                        if let Some(tx) = p.remove(&resp.id) {
                            let _ = tx.send(resp.response_result);
                        }
                    }
                }
                Message::Notification(n) => {
                    if let Ok(mut q) = notifications.lock() {
                        q.push_back(n);
                    }
                }
                Message::Request(req) => {
                    let resp = Response::new_err(
                        req.id,
                        -32601,
                        format!("method not found: {}", req.method),
                    );
                    if let Ok(mut w) = writer.lock() {
                        let _ = Message::Response(resp).write(&mut *w);
                        let _ = w.flush();
                    }
                }
            }
        }
    });
}

/// Client capabilities: full document sync, hover (markdown+plain), go-to
/// definition with links, hierarchical document symbols, push diagnostics.
fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        text_document: Some(TextDocumentClientCapabilities {
            synchronization: Some(TextDocumentSyncClientCapabilities {
                dynamic_registration: Some(false),
                will_save: Some(false),
                will_save_wait_until: Some(false),
                did_save: Some(false),
            }),
            hover: Some(HoverClientCapabilities {
                dynamic_registration: Some(false),
                content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
            }),
            definition: Some(DefinitionClientCapabilities {
                dynamic_registration: Some(false),
                link_support: Some(true),
            }),
            document_symbol: Some(DocumentSymbolClientCapabilities {
                dynamic_registration: Some(false),
                symbol_kind: None,
                hierarchical_document_symbol_support: Some(true),
                tag_support: None,
                label_support: None,
            }),
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                version_support: Some(false),
                diagnostics_capabilities: DiagnosticsCapabilities {
                    related_information: Some(false),
                    tag_support: None,
                    code_description_support: None,
                    data_support: None,
                },
            }),
            ..Default::default()
        }),
        workspace: Some(WorkspaceClientCapabilities {
            workspace_folders: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    }
}
