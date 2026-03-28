//! Semantic token generation for Makefile syntax highlighting.

use makefile_lossless::{Makefile, SyntaxKind};
use rowan::ast::AstNode;
use tower_lsp_server::ls_types::SemanticToken;

use crate::builtins;
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

/// Semantic token modifiers.
///
/// These are bit flags — the discriminant values must match the order
/// in the legend registered during initialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TokenModifier {
    /// The token is a definition site (not a reference)
    Definition = 0,
    /// The token is a built-in (not user-defined)
    DefaultLibrary = 1,
}

impl TokenModifier {
    fn bitmask(self) -> u32 {
        1 << (self as u32)
    }
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
    pub fn push(
        &mut self,
        line: u32,
        start: u32,
        length: u32,
        token_type: TokenType,
        modifiers: u32,
    ) {
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
            token_modifiers_bitset: modifiers,
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
                        0,
                    );
                }
                SyntaxKind::IDENTIFIER => {
                    if let Some(parent) = token.parent() {
                        let text = token.text();
                        match parent.kind() {
                            SyntaxKind::TARGETS => {
                                let mut mods = TokenModifier::Definition.bitmask();
                                if builtins::SPECIAL_TARGETS.iter().any(|(n, _)| *n == text) {
                                    mods |= TokenModifier::DefaultLibrary.bitmask();
                                }
                                builder.push(
                                    start_pos.line,
                                    start_pos.character,
                                    length,
                                    TokenType::Target,
                                    mods,
                                );
                            }
                            SyntaxKind::VARIABLE => {
                                let mut mods = TokenModifier::Definition.bitmask();
                                if builtins::BUILTIN_VARIABLES.contains(&text) {
                                    mods |= TokenModifier::DefaultLibrary.bitmask();
                                }
                                builder.push(
                                    start_pos.line,
                                    start_pos.character,
                                    length,
                                    TokenType::Variable,
                                    mods,
                                );
                            }
                            SyntaxKind::PREREQUISITE => {
                                builder.push(
                                    start_pos.line,
                                    start_pos.character,
                                    length,
                                    TokenType::Prerequisite,
                                    0,
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

    #[test]
    fn test_target_has_definition_modifier() {
        let text = "all:\n\techo done\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let tokens = generate_semantic_tokens(&makefile, text);

        let target_token = tokens
            .iter()
            .find(|t| t.token_type == TokenType::Target as u32)
            .unwrap();
        assert_ne!(
            target_token.token_modifiers_bitset & TokenModifier::Definition.bitmask(),
            0
        );
    }

    #[test]
    fn test_special_target_has_default_library_modifier() {
        let text = ".PHONY: all\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let tokens = generate_semantic_tokens(&makefile, text);

        let target_token = tokens
            .iter()
            .find(|t| t.token_type == TokenType::Target as u32)
            .unwrap();
        assert_ne!(
            target_token.token_modifiers_bitset & TokenModifier::DefaultLibrary.bitmask(),
            0
        );
    }

    #[test]
    fn test_user_target_no_default_library_modifier() {
        let text = "all:\n\techo done\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let tokens = generate_semantic_tokens(&makefile, text);

        let target_token = tokens
            .iter()
            .find(|t| t.token_type == TokenType::Target as u32)
            .unwrap();
        assert_eq!(
            target_token.token_modifiers_bitset & TokenModifier::DefaultLibrary.bitmask(),
            0
        );
    }

    #[test]
    fn test_variable_definition_has_definition_modifier() {
        let text = "CC = gcc\n";
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let tokens = generate_semantic_tokens(&makefile, text);

        let var_token = tokens
            .iter()
            .find(|t| t.token_type == TokenType::Variable as u32)
            .unwrap();
        assert_ne!(
            var_token.token_modifiers_bitset & TokenModifier::Definition.bitmask(),
            0
        );
    }
}
