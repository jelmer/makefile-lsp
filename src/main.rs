//! Makefile Language Server Protocol implementation.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

mod code_actions;
mod completion;
mod diagnostics;
mod document_links;
mod folding;
mod goto;
mod highlights;
mod hover;
mod position;
mod references;
mod rename;
mod semantic;
mod symbols;

use position::try_lsp_range_to_text_range;

/// Information about an open file.
struct FileInfo {
    /// The current source text.
    text: String,
    /// The parsed makefile (green node for thread safety).
    parsed: makefile_lossless::Parse<makefile_lossless::Makefile>,
}

struct Backend {
    client: Client,
    files: Arc<Mutex<HashMap<Uri, FileInfo>>>,
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client,
            files: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn update_file(&self, uri: Uri, text: String) {
        let parsed = makefile_lossless::Makefile::parse(&text);
        let diagnostics = diagnostics::get_diagnostics(&text, &parsed);

        let mut files = self.files.lock().await;
        files.insert(uri.clone(), FileInfo { text, parsed });
        drop(files);

        self.client
            .publish_diagnostics(uri, diagnostics, None)
            .await;
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: None,
                    trigger_characters: Some(vec![
                        "$".to_string(),
                        "(".to_string(),
                        ":".to_string(),
                    ]),
                    work_done_progress_options: Default::default(),
                    all_commit_characters: None,
                    completion_item: None,
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                definition_provider: Some(OneOf::Left(true)),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: Some(false),
                    work_done_progress_options: Default::default(),
                }),
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
                                ],
                                token_modifiers: vec![],
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
        self.client
            .log_message(MessageType::INFO, "Makefile LSP initialized!")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.update_file(params.text_document.uri, params.text_document.text)
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;

        if params.content_changes.is_empty() {
            return;
        }

        let files = self.files.lock().await;
        let mut text = files.get(&uri).map(|f| f.text.clone()).unwrap_or_default();
        drop(files);

        let mut _changed_range: Option<text_size::TextRange> = None;

        for change in &params.content_changes {
            if let Some(range) = &change.range {
                if let Some(text_range) = try_lsp_range_to_text_range(&text, range) {
                    let start: usize = text_range.start().into();
                    let end: usize = text_range.end().into();
                    let new_end = start + change.text.len();
                    let new_range = text_size::TextRange::new(
                        text_size::TextSize::from(start as u32),
                        text_size::TextSize::from(new_end as u32),
                    );
                    _changed_range = Some(match _changed_range {
                        Some(existing) => existing.cover(new_range),
                        None => new_range,
                    });
                    text.replace_range(start..end, &change.text);
                }
            } else {
                text = change.text.clone();
                _changed_range = None;
            }
        }

        self.update_file(uri, text).await;
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let files = self.files.lock().await;
        let Some(file_info) = files.get(uri) else {
            return Ok(None);
        };

        let makefile = file_info.parsed.tree();
        let completions = completion::get_completions(&makefile, &file_info.text, position);
        drop(files);

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
        let uri = &params.text_document.uri;
        let position = params.position;

        let files = self.files.lock().await;
        let Some(file_info) = files.get(uri) else {
            return Ok(None);
        };

        let makefile = file_info.parsed.tree();
        let result = rename::prepare_rename(&makefile, &file_info.text, position);
        drop(files);

        Ok(result)
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let files = self.files.lock().await;
        let Some(file_info) = files.get(uri) else {
            return Ok(None);
        };

        let makefile = file_info.parsed.tree();
        let result =
            rename::rename(&makefile, &file_info.text, position, &params.new_name, uri);
        drop(files);

        Ok(result)
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let files = self.files.lock().await;
        let Some(file_info) = files.get(uri) else {
            return Ok(None);
        };

        let makefile = file_info.parsed.tree();
        let refs = references::find_references(
            &makefile,
            &file_info.text,
            position,
            uri,
            params.context.include_declaration,
        );
        drop(files);

        if refs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(refs))
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let files = self.files.lock().await;
        let Some(file_info) = files.get(uri) else {
            return Ok(None);
        };

        let makefile = file_info.parsed.tree();
        let result = hover::get_hover(&makefile, &file_info.text, position);
        drop(files);

        Ok(result)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let files = self.files.lock().await;
        let Some(file_info) = files.get(uri) else {
            return Ok(None);
        };

        let makefile = file_info.parsed.tree();
        let result = goto::goto_definition(&makefile, &file_info.text, position, uri);
        drop(files);

        Ok(result)
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = &params.text_document.uri;
        let range = params.range;

        let files = self.files.lock().await;
        let Some(file_info) = files.get(uri) else {
            return Ok(None);
        };

        let makefile = file_info.parsed.tree();
        let actions = code_actions::get_code_actions(&makefile, &file_info.text, range, uri);
        drop(files);

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

    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        let uri = &params.text_document.uri;

        let files = self.files.lock().await;
        let Some(file_info) = files.get(uri) else {
            return Ok(None);
        };

        let makefile = file_info.parsed.tree();
        let links = document_links::get_document_links(&makefile, &file_info.text, uri);
        drop(files);

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

        let files = self.files.lock().await;
        let Some(file_info) = files.get(uri) else {
            return Ok(None);
        };

        let makefile = file_info.parsed.tree();
        let hl = highlights::get_highlights(&makefile, &file_info.text, position, uri);
        drop(files);

        if hl.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hl))
        }
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = &params.text_document.uri;

        let files = self.files.lock().await;
        let Some(file_info) = files.get(uri) else {
            return Ok(None);
        };

        let makefile = file_info.parsed.tree();
        let tokens = semantic::generate_semantic_tokens(&makefile, &file_info.text);
        drop(files);

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

        let files = self.files.lock().await;
        let Some(file_info) = files.get(uri) else {
            return Ok(None);
        };

        let makefile = file_info.parsed.tree();
        let symbols = symbols::generate_document_symbols(&makefile, &file_info.text);
        drop(files);

        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let uri = &params.text_document.uri;

        let files = self.files.lock().await;
        let Some(file_info) = files.get(uri) else {
            return Ok(None);
        };

        let makefile = file_info.parsed.tree();
        let ranges = folding::generate_folding_ranges(&makefile, &file_info.text);
        drop(files);

        Ok(Some(ranges))
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
