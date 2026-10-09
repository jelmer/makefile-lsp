//! Makefile Language Server Protocol implementation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_lsp_server::jsonrpc::{Error, Result};
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

mod builtins;
mod call_hierarchy;
mod check;
mod code_actions;
mod completion;
mod dep_graph;
mod diagnostics;
mod document_links;
mod fmt;
mod folding;
mod formatting;
mod goto;
mod highlights;
mod hover;
mod inlay_hints;
mod position;
mod references;
mod rename;
#[cfg(feature = "scip")]
mod scip;
mod selection_ranges;
mod semantic;
mod shell_check;
mod signature_help;
mod symbols;
mod targets;
mod workspace;

use position::try_lsp_range_to_text_range;
use workspace::{Document, FileSet, Workspace};

/// The diagnostics last published for an open document.
struct PublishedDiagnostics {
    /// The document they were computed for.
    doc: Arc<Document>,
    /// Diagnostics other than shell syntax ones.
    file: Vec<Diagnostic>,
    /// Shell syntax diagnostics, which are only computed on open and save.
    shell: Vec<Diagnostic>,
}

impl PublishedDiagnostics {
    fn all(&self) -> Vec<Diagnostic> {
        self.file.iter().chain(&self.shell).cloned().collect()
    }
}

struct Backend {
    client: Client,
    workspace: Arc<Mutex<Workspace>>,
    diagnostics: Arc<Mutex<HashMap<Uri, PublishedDiagnostics>>>,
    /// Whether the client can watch files for us.
    watch_files: AtomicBool,
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client,
            workspace: Arc::new(Mutex::new(Workspace::new())),
            diagnostics: Arc::new(Mutex::new(HashMap::new())),
            watch_files: AtomicBool::new(false),
        }
    }

    async fn document(&self, uri: &Uri) -> Option<Arc<Document>> {
        self.workspace.lock().await.document(uri)
    }

    async fn file_set(&self, uri: &Uri) -> Option<FileSet> {
        self.workspace.lock().await.file_set(uri)
    }

    /// The file set for `uri`, which need not be open, as for a call
    /// hierarchy item in an included makefile.
    async fn file_set_for_uri(&self, uri: &Uri) -> Result<FileSet> {
        let mut workspace = self.workspace.lock().await;
        if let Some(files) = workspace.file_set(uri) {
            return Ok(files);
        }
        let path = workspace::file_path(uri)
            .ok_or_else(|| Error::invalid_params(format!("not a file URI: {}", uri.as_str())))?;
        workspace.file_set_for_path(&path).map_err(|e| Error {
            code: tower_lsp_server::jsonrpc::ErrorCode::InternalError,
            message: format!("unable to load {}: {e}", path.display()).into(),
            data: None,
        })
    }

    async fn update_file(&self, uri: Uri, text: String) {
        let (doc, files, dependents) = {
            let mut workspace = self.workspace.lock().await;
            let doc = workspace.open(uri.clone(), text);
            let files = workspace.file_set(&uri).expect("document was just opened");
            (doc, files, workspace.dependents(&uri))
        };
        let diagnostics = diagnostics::get_file_set_diagnostics(&files);

        self.diagnostics.lock().await.insert(
            uri.clone(),
            PublishedDiagnostics {
                doc,
                file: diagnostics.clone(),
                shell: Vec::new(),
            },
        );

        self.client
            .publish_diagnostics(uri, diagnostics, None)
            .await;

        self.republish(dependents).await;
    }

    /// Recompute and publish diagnostics for open documents.
    async fn republish(&self, uris: Vec<Uri>) {
        for uri in uris {
            let Some(files) = self.file_set(&uri).await else {
                continue;
            };
            let diagnostics = diagnostics::get_file_set_diagnostics(&files);
            let all = {
                let mut published = self.diagnostics.lock().await;
                // If the text changed, update_file is about to publish.
                let Some(entry) = published
                    .get_mut(&uri)
                    .filter(|p| p.doc.text() == files.current().text())
                else {
                    continue;
                };
                entry.file = diagnostics;
                entry.all()
            };
            self.client.publish_diagnostics(uri, all, None).await;
        }
    }

    /// Ask the client to tell us about changes to makefiles on disk, so
    /// diagnostics of open documents that include them can be refreshed.
    ///
    /// Only common makefile names are watched; requests re-check every
    /// included file's mtime anyway, so other included files are picked up
    /// on the next edit.
    async fn register_file_watchers(&self) {
        let watchers = [
            "**/*.mk",
            "**/*.make",
            "**/Makefile",
            "**/makefile",
            "**/GNUmakefile",
        ]
        .iter()
        .map(|glob| FileSystemWatcher {
            glob_pattern: GlobPattern::String(glob.to_string()),
            kind: None,
        })
        .collect();
        let options =
            match serde_json::to_value(DidChangeWatchedFilesRegistrationOptions { watchers }) {
                Ok(options) => options,
                Err(e) => {
                    tracing::error!("unable to serialize file watcher options: {e}");
                    return;
                }
            };
        let registration = Registration {
            id: "makefile-watcher".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: Some(options),
        };
        if let Err(e) = self.client.register_capability(vec![registration]).await {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("unable to register file watchers: {e}"),
                )
                .await;
        }
    }

    /// Check the recipes of an open file for shell syntax errors in the
    /// background, as this spawns processes, and publish the results
    /// alongside its other diagnostics.
    fn spawn_shell_syntax_check(&self, uri: Uri) {
        let client = self.client.clone();
        let published = self.diagnostics.clone();
        tokio::spawn(async move {
            let Some(doc) = published.lock().await.get(&uri).map(|p| p.doc.clone()) else {
                return;
            };

            let checked = doc.clone();
            let shell_diagnostics = match tokio::task::spawn_blocking(move || {
                shell_check::check_shell_syntax(
                    checked.text(),
                    &checked.makefile(),
                    checked.variant(),
                )
            })
            .await
            {
                Ok(diagnostics) => diagnostics,
                Err(e) => {
                    tracing::error!("shell syntax check failed: {}", e);
                    return;
                }
            };

            let mut published = published.lock().await;
            // If the file changed while checking, the next save checks again.
            let Some(entry) = published
                .get_mut(&uri)
                .filter(|p| p.doc.text() == doc.text())
            else {
                return;
            };
            entry.shell = shell_diagnostics;
            let all = entry.all();
            drop(published);

            client.publish_diagnostics(uri, all, None).await;
        });
    }
}

/// Report a formatting failure as an LSP RequestFailed error.
fn format_error(e: formatting::FormatError) -> tower_lsp_server::jsonrpc::Error {
    tower_lsp_server::jsonrpc::Error {
        code: tower_lsp_server::jsonrpc::ErrorCode::ServerError(-32803),
        message: e.to_string().into(),
        data: None,
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        #[allow(deprecated)]
        let root_uri = params.root_uri.as_ref();
        let roots = match &params.workspace_folders {
            Some(folders) => folders
                .iter()
                .filter_map(|f| workspace::file_path(&f.uri))
                .collect(),
            None => root_uri
                .and_then(workspace::file_path)
                .into_iter()
                .collect(),
        };
        self.workspace.lock().await.set_roots(roots);
        let watch_files = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.did_change_watched_files.as_ref())
            .and_then(|w| w.dynamic_registration)
            .unwrap_or(false);
        self.watch_files.store(watch_files, Ordering::Relaxed);

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: None,
                    trigger_characters: Some(vec![
                        "$".to_string(),
                        "(".to_string(),
                        ":".to_string(),
                        "/".to_string(),
                    ]),
                    work_done_progress_options: Default::default(),
                    all_commit_characters: None,
                    completion_item: None,
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec![" ".to_string(), ",".to_string()]),
                    retrigger_characters: Some(vec![",".to_string()]),
                    work_done_progress_options: Default::default(),
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                inlay_hint_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                document_formatting_provider: Some(OneOf::Left(true)),
                document_range_formatting_provider: Some(OneOf::Left(true)),
                document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
                    first_trigger_character: "\n".to_string(),
                    more_trigger_character: None,
                }),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: Some(false),
                    work_done_progress_options: Default::default(),
                }),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                            legend: SemanticTokensLegend {
                                token_types: vec![
                                    SemanticTokenType::new("makefileTarget"),
                                    SemanticTokenType::new("makefileVariable"),
                                    SemanticTokenType::COMMENT,
                                    SemanticTokenType::new("makefilePrerequisite"),
                                    SemanticTokenType::new("makefileRecipe"),
                                    SemanticTokenType::KEYWORD,
                                ],
                                token_modifiers: vec![
                                    SemanticTokenModifier::DEFINITION,
                                    SemanticTokenModifier::DEFAULT_LIBRARY,
                                ],
                            },
                            range: Some(false),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                        },
                    ),
                ),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "makefile-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            offset_encoding: None,
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        if self.watch_files.load(Ordering::Relaxed) {
            self.register_file_watchers().await;
        }
        self.client
            .log_message(MessageType::INFO, "Makefile LSP initialized!")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        self.update_file(uri.clone(), params.text_document.text)
            .await;
        self.spawn_shell_syntax_check(uri);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.spawn_shell_syntax_check(params.text_document.uri);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        let dependents = {
            let mut workspace = self.workspace.lock().await;
            let dependents = workspace.dependents(&uri);
            workspace.close(&uri);
            dependents
        };
        self.diagnostics.lock().await.remove(&uri);
        // The file on disk may differ from the unsaved buffer.
        self.republish(dependents).await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let uris = {
            let mut workspace = self.workspace.lock().await;
            let mut uris: Vec<Uri> = Vec::new();
            for change in &params.changes {
                let Some(path) = workspace::file_path(&change.uri) else {
                    continue;
                };
                workspace.invalidate(&path);
                // A new or removed file may change how includes resolve
                // anywhere, so refresh everything.
                if change.typ != FileChangeType::CHANGED {
                    uris = workspace.open_documents();
                    break;
                }
                for uri in workspace.open_documents_seeing(&path) {
                    if !uris.contains(&uri) {
                        uris.push(uri);
                    }
                }
            }
            uris
        };
        self.republish(uris).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;

        if params.content_changes.is_empty() {
            return;
        }

        let mut text = self
            .document(&uri)
            .await
            .map(|d| d.text().to_string())
            .unwrap_or_default();

        for change in &params.content_changes {
            if let Some(range) = &change.range {
                if let Some(text_range) = try_lsp_range_to_text_range(&text, range) {
                    text.replace_range(std::ops::Range::<usize>::from(text_range), &change.text);
                }
            } else {
                text = change.text.clone();
            }
        }

        self.update_file(uri, text).await;
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let Some(files) = self.file_set(uri).await else {
            return Ok(None);
        };
        let doc = files.current();
        let makefiles: Vec<makefile_lossless::Makefile> =
            files.docs().map(|d| d.makefile()).collect();
        let completions = completion::get_completions(&makefiles, doc.text(), position, doc.dir());

        if completions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CompletionResponse::Array(completions)))
        }
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let Some(files) = self.file_set(&params.text_document.uri).await else {
            return Ok(None);
        };
        rename::prepare_rename(&files, params.position)
            .transpose()
            .map_err(|e| Error::invalid_params(e.to_string()))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let Some(files) = self.file_set(uri).await else {
            return Ok(None);
        };
        rename::rename(&files, position, &params.new_name)
            .transpose()
            .map_err(|e| Error::invalid_params(e.to_string()))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let Some(files) = self.file_set(uri).await else {
            return Ok(None);
        };
        let refs =
            references::find_references(&files, position, params.context.include_declaration);

        if refs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(refs))
        }
    }

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(files) = self.file_set(uri).await else {
            return Ok(None);
        };
        Ok(call_hierarchy::prepare(&files, position))
    }

    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let files = self.file_set_for_uri(&params.item.uri).await?;
        Ok(Some(call_hierarchy::incoming_calls(&files, &params.item)))
    }

    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let files = self.file_set_for_uri(&params.item.uri).await?;
        Ok(Some(call_hierarchy::outgoing_calls(&files, &params.item)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(files) = self.file_set(uri).await else {
            return Ok(None);
        };
        Ok(hover::get_hover(&files, position))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(doc) = self.document(uri).await else {
            return Ok(None);
        };

        let makefile = doc.makefile();
        let result = signature_help::get_signature_help(&makefile, doc.text(), position);

        Ok(result)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(files) = self.file_set(uri).await else {
            return Ok(None);
        };
        Ok(goto::goto_definition(&files, position))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = &params.text_document.uri;
        let range = params.range;

        let Some(files) = self.file_set(uri).await else {
            return Ok(None);
        };

        let actions = code_actions::get_code_actions(&files, range, &params.context.diagnostics);

        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(
                actions
                    .into_iter()
                    .map(CodeActionOrCommand::CodeAction)
                    .collect(),
            ))
        }
    }

    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        let Some(files) = self.file_set(&params.text_document.uri).await else {
            return Ok(None);
        };
        let links = document_links::get_document_links(&files);

        if links.is_empty() {
            Ok(None)
        } else {
            Ok(Some(links))
        }
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(doc) = self.document(uri).await else {
            return Ok(None);
        };

        let makefile = doc.makefile();
        let hl = highlights::get_highlights(&makefile, doc.text(), position, uri);

        if hl.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hl))
        }
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = &params.text_document.uri;
        let range = params.range;

        let Some(doc) = self.document(uri).await else {
            return Ok(None);
        };

        let makefile = doc.makefile();
        let hints = inlay_hints::get_inlay_hints(&makefile, doc.text(), range);

        if hints.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hints))
        }
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = &params.text_document.uri;

        let Some(doc) = self.document(uri).await else {
            return Ok(None);
        };

        let makefile = doc.makefile();
        let tokens = semantic::generate_semantic_tokens(&makefile, doc.text());

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: tokens,
        })))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = &params.text_document.uri;

        let Some(doc) = self.document(uri).await else {
            return Ok(None);
        };

        let makefile = doc.makefile();
        let symbols = symbols::generate_document_symbols(&makefile, doc.text());

        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let uri = &params.text_document.uri;

        let Some(doc) = self.document(uri).await else {
            return Ok(None);
        };

        let makefile = doc.makefile();
        let ranges = folding::generate_folding_ranges(&makefile, doc.text());

        Ok(Some(ranges))
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let uri = &params.text_document.uri;

        let Some(doc) = self.document(uri).await else {
            return Ok(None);
        };

        let makefile = doc.makefile();
        let ranges =
            selection_ranges::get_selection_ranges(&makefile, doc.text(), &params.positions);

        Ok(Some(ranges))
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let Some(doc) = self.document(&params.text_document.uri).await else {
            return Ok(None);
        };
        formatting::format_document(doc.parsed(), doc.text())
            .map(Some)
            .map_err(format_error)
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let Some(doc) = self.document(&params.text_document.uri).await else {
            return Ok(None);
        };
        formatting::format_range(doc.parsed(), doc.text(), params.range)
            .map(Some)
            .map_err(format_error)
    }

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        // Only handle newline
        if params.ch != "\n" {
            return Ok(None);
        }

        // Check the previous line
        if position.line == 0 {
            return Ok(None);
        }

        let Some(doc) = self.document(uri).await else {
            return Ok(None);
        };

        // After a rule header, start a recipe line
        let prev_line = (position.line - 1) as usize;
        if formatting::is_rule_header(&doc.makefile(), prev_line) {
            Ok(Some(vec![TextEdit {
                range: Range::new(position, position),
                new_text: "\t".to_string(),
            }]))
        } else {
            Ok(None)
        }
    }
}

async fn run_lsp() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        #[cfg(feature = "scip")]
        Some("scip") => {
            if let Err(e) = scip_command::run(&args[2..]) {
                eprintln!("makefile-lsp scip: {e}");
                std::process::exit(1);
            }
        }
        #[cfg(not(feature = "scip"))]
        Some("scip") => {
            eprintln!("makefile-lsp: built without SCIP support (enable the 'scip' feature)");
            std::process::exit(2);
        }
        Some("check") => std::process::exit(check::run(&args[2..])),
        Some("fmt") => std::process::exit(fmt::run(&args[2..])),
        Some("--help" | "-h") => print_usage(),
        // Printed to stdout, unlike the usage message: the version is the
        // result of this invocation, not a diagnostic. Without it, --version
        // falls through to the catch-all arm below and silently starts a
        // language server on stdin.
        Some("--version" | "-V") => println!("makefile-lsp {}", env!("CARGO_PKG_VERSION")),
        Some(other) if !other.starts_with('-') => {
            eprintln!("makefile-lsp: unknown subcommand '{other}'");
            print_usage();
            std::process::exit(2);
        }
        // No subcommand (or only flags): run the language server over stdio.
        _ => {
            let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
            rt.block_on(run_lsp());
        }
    }
}

#[cfg(feature = "scip")]
fn print_usage() {
    eprintln!(
        "makefile-lsp {}\n\n\
         Usage:\n  \
         makefile-lsp                  Run the language server over stdin/stdout\n  \
         makefile-lsp check [PATH...]  Report diagnostics for Makefiles\n  \
         makefile-lsp fmt [PATH...]    Format Makefiles\n  \
         makefile-lsp scip [FILE...]   Generate a SCIP index for the given Makefiles\n  \
         makefile-lsp --version        Print the version\n\n\
         Run 'makefile-lsp check --help', 'makefile-lsp fmt --help' or\n\
         'makefile-lsp scip --help' for options.",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(not(feature = "scip"))]
fn print_usage() {
    eprintln!(
        "makefile-lsp {}\n\n\
         Usage:\n  \
         makefile-lsp                  Run the language server over stdin/stdout\n  \
         makefile-lsp check [PATH...]  Report diagnostics for Makefiles\n  \
         makefile-lsp fmt [PATH...]    Format Makefiles\n  \
         makefile-lsp --version        Print the version\n\n\
         Run 'makefile-lsp check --help' or 'makefile-lsp fmt --help' for options.",
        env!("CARGO_PKG_VERSION")
    );
}

/// Implementation of the `scip` subcommand.
#[cfg(feature = "scip")]
mod scip_command {
    use std::path::{Path, PathBuf};

    /// Generate a SCIP index for the given Makefiles.
    ///
    /// Files default to `Makefile` in the current directory when none are given.
    /// The index is written to `index.scip` unless `-o`/`--output` is passed.
    pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
        let mut output = PathBuf::from("index.scip");
        let mut inputs: Vec<PathBuf> = Vec::new();
        let mut project_root: Option<PathBuf> = None;

        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "-o" | "--output" => {
                    let value = iter.next().ok_or("missing value for --output")?;
                    output = PathBuf::from(value);
                }
                "--project-root" => {
                    let value = iter.next().ok_or("missing value for --project-root")?;
                    project_root = Some(PathBuf::from(value));
                }
                "-h" | "--help" => {
                    print_help();
                    return Ok(());
                }
                other if other.starts_with('-') => {
                    return Err(format!("unknown option '{other}'").into());
                }
                other => inputs.push(PathBuf::from(other)),
            }
        }

        if inputs.is_empty() {
            inputs.push(PathBuf::from("Makefile"));
        }

        let root = match project_root {
            Some(p) => p,
            None => std::env::current_dir()?,
        };
        let root = root.canonicalize().unwrap_or(root);

        let mut workspace = crate::workspace::Workspace::new();
        workspace.set_roots(vec![root.clone()]);
        let mut files = Vec::with_capacity(inputs.len());
        for input in &inputs {
            let absolute =
                std::path::absolute(input).map_err(|e| format!("{}: {e}", input.display()))?;
            let file_set = workspace
                .file_set_for_path(&absolute)
                .map_err(|e| format!("{}: {e}", input.display()))?;
            files.push(crate::scip::SourceFile {
                relative_path: relative_path(&root, input),
                files: file_set,
            });
        }

        let project_root_uri = path_to_file_uri(&root);
        let index = crate::scip::build_index(&project_root_uri, &files);

        scip::write_message_to_file(&output, index)
            .map_err(|e| format!("{}: {e}", output.display()))?;

        eprintln!(
            "Wrote SCIP index for {} file(s) to {}",
            files.len(),
            output.display()
        );
        Ok(())
    }

    fn print_help() {
        eprintln!(
            "Usage: makefile-lsp scip [OPTIONS] [FILE...]\n\n\
             Generate a SCIP code-intelligence index for one or more Makefiles.\n\n\
             Options:\n  \
             -o, --output FILE        Write the index to FILE (default: index.scip)\n      \
             --project-root DIR   Root directory recorded in the index (default: cwd)\n  \
             -h, --help               Show this help\n\n\
             With no FILE, 'Makefile' in the current directory is used. Included\n\
             makefiles, and makefiles including a FILE, are read for definitions\n\
             and diagnostics but only the given files are indexed."
        );
    }

    /// Compute a path relative to `root`, falling back to the input's file name
    /// (or the path as given) when it lies outside the root.
    fn relative_path(root: &Path, input: &Path) -> String {
        let absolute = input.canonicalize().unwrap_or_else(|_| root.join(input));
        let rel = absolute.strip_prefix(root).unwrap_or(&absolute);
        let rel = if rel.as_os_str().is_empty() {
            input
        } else {
            rel
        };
        rel.to_string_lossy().replace('\\', "/")
    }

    /// Render an absolute path as a `file://` URI.
    fn path_to_file_uri(path: &Path) -> String {
        let s = path.to_string_lossy().replace('\\', "/");
        if let Some(stripped) = s.strip_prefix('/') {
            format!("file:///{stripped}")
        } else {
            format!("file:///{s}")
        }
    }
}
