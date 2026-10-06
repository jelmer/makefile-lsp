//! Makefile Language Server Protocol implementation.

use std::sync::Arc;
use tokio::sync::Mutex;
use tower_lsp_server::jsonrpc::{Error, Result};
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

mod builtins;
mod code_actions;
mod completion;
mod dep_graph;
mod diagnostics;
mod document_links;
mod folding;
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
mod signature_help;
mod symbols;
mod workspace;

use position::try_lsp_range_to_text_range;
use workspace::{Document, FileSet, Workspace};

struct Backend {
    client: Client,
    workspace: Arc<Mutex<Workspace>>,
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client,
            workspace: Arc::new(Mutex::new(Workspace::new())),
        }
    }

    async fn document(&self, uri: &Uri) -> Option<Arc<Document>> {
        self.workspace.lock().await.document(uri)
    }

    async fn file_set(&self, uri: &Uri) -> Option<FileSet> {
        self.workspace.lock().await.file_set(uri)
    }

    async fn update_file(&self, uri: Uri, text: String) {
        let files = {
            let mut workspace = self.workspace.lock().await;
            workspace.open(uri.clone(), text);
            workspace.file_set(&uri).expect("document was just opened")
        };
        let diagnostics = diagnostics::get_file_set_diagnostics(&files);

        self.client
            .publish_diagnostics(uri, diagnostics, None)
            .await;
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
                definition_provider: Some(OneOf::Left(true)),
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

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.workspace.lock().await.close(&params.text_document.uri);
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

        let Some(doc) = self.document(uri).await else {
            return Ok(None);
        };

        let actions = code_actions::get_code_actions(doc.parsed(), doc.text(), range, uri);

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

        let prev_line_idx = (position.line - 1) as usize;
        let prev_line = doc.text().lines().nth(prev_line_idx).unwrap_or("");

        // If the previous line is a rule header (has : but not =, and doesn't start with tab),
        // insert a tab at the cursor position
        let is_rule_header =
            !prev_line.starts_with('\t') && prev_line.contains(':') && !prev_line.contains('=');

        if is_rule_header {
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
         makefile-lsp scip [FILE...]   Generate a SCIP index for the given Makefiles\n  \
         makefile-lsp --version        Print the version\n\n\
         Run 'makefile-lsp scip --help' for SCIP options.",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(not(feature = "scip"))]
fn print_usage() {
    eprintln!(
        "makefile-lsp {}\n\n\
         Usage:\n  \
         makefile-lsp                  Run the language server over stdin/stdout\n  \
         makefile-lsp --version        Print the version",
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

        let mut files = Vec::with_capacity(inputs.len());
        for input in &inputs {
            let text =
                std::fs::read_to_string(input).map_err(|e| format!("{}: {e}", input.display()))?;
            let relative = relative_path(&root, input);
            let base_dir = input
                .canonicalize()
                .unwrap_or_else(|_| root.join(input))
                .parent()
                .map(Path::to_path_buf);
            files.push(crate::scip::SourceFile {
                relative_path: relative,
                text,
                base_dir,
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
             With no FILE, 'Makefile' in the current directory is used."
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
