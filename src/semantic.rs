//! Semantic token generation for Makefile syntax highlighting.

use makefile_lossless::{Makefile, SyntaxKind};
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::SemanticToken;

use crate::position::{offset_to_position, utf16_len};

/// Semantic token types used by the makefile LSP.
///
/// The discriminant values must match the order in the legend
/// registered during initialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TokenType {
    /// A target name
    Target = 0,
    /// A variable name
    Variable = 1,
    /// A comment
    Comment = 2,
    /// A prerequisite
    Prerequisite = 3,
    /// A recipe line (reserved for future use in semantic token legend)
    #[allow(dead_code)]
    Recipe = 4,
}

/// Builder that tracks delta positions for semantic tokens.
pub struct SemanticTokensBuilder {
    tokens: Vec<SemanticToken>,
    prev_line: u32,
    prev_start: u32,
}

impl SemanticTokensBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self {
            tokens: Vec::new(),
            prev_line: 0,
            prev_start: 0,
        }
    }

    /// Push a semantic token, computing deltas automatically.
    pub fn push(&mut self, line: u32, start: u32, length: u32, token_type: TokenType) {
        let delta_line = line - self.prev_line;
        let delta_start = if delta_line == 0 {
            start - self.prev_start
        } else {
            start
        };

        self.tokens.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type: token_type as u32,
            token_modifiers_bitset: 0,
        });

        self.prev_line = line;
        self.prev_start = start;
    }

    /// Consume the builder and return the tokens.
    pub fn build(self) -> Vec<SemanticToken> {
        self.tokens
    }
}

/// Generate semantic tokens for a Makefile.
pub fn generate_semantic_tokens(makefile: &Makefile, source_text: &str) -> Vec<SemanticToken> {
    let mut builder = SemanticTokensBuilder::new();

    for element in makefile.syntax().descendants_with_tokens() {
        if let rowan::NodeOrToken::Token(token) = element {
            let range = token.text_range();
            let start_pos = offset_to_position(source_text, range.start());
            let length = utf16_len(token.text());

            match token.kind() {
                SyntaxKind::COMMENT => {
                    builder.push(
                        start_pos.line,
                        start_pos.character,
                        length,
                        TokenType::Comment,
                    );
                }
                SyntaxKind::IDENTIFIER => {
                    if let Some(parent) = token.parent() {
                        match parent.kind() {
                            SyntaxKind::TARGETS => {
                                builder.push(
                                    start_pos.line,
                                    start_pos.character,
                                    length,
                                    TokenType::Target,
                                );
                            }
                            SyntaxKind::VARIABLE => {
                                builder.push(
                                    start_pos.line,
                                    start_pos.character,
                                    length,
                                    TokenType::Variable,
                                );
                            }
                            SyntaxKind::PREREQUISITE => {
                                builder.push(
                                    start_pos.line,
                                    start_pos.character,
                                    length,
                                    TokenType::Prerequisite,
                                );
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_token() {
        let text = "clean:\n\trm -rf build\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let tokens = generate_semantic_tokens(&makefile, text);

        assert!(!tokens.is_empty());
        assert_eq!(tokens[0].token_type, TokenType::Target as u32);
    }

    #[test]
    fn test_variable_token() {
        let text = "CC = gcc\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let tokens = generate_semantic_tokens(&makefile, text);

        assert!(!tokens.is_empty());
        assert_eq!(tokens[0].token_type, TokenType::Variable as u32);
    }

    #[test]
    fn test_comment_token() {
        let text = "# This is a comment\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let tokens = generate_semantic_tokens(&makefile, text);

        assert!(!tokens.is_empty());
        assert_eq!(tokens[0].token_type, TokenType::Comment as u32);
    }

    #[test]
    fn test_empty_file() {
        let text = "";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let tokens = generate_semantic_tokens(&makefile, text);
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_multiple_tokens() {
        let text = "# comment\nCC = gcc\nall:\n\t$(CC) main.c\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let tokens = generate_semantic_tokens(&makefile, text);

        assert!(tokens.len() >= 3);
    }
}
