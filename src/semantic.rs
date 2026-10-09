//! Semantic token generation for Makefile syntax highlighting.

use makefile_lossless::{Makefile, TextRange, TextSize};
use tower_lsp_server::ls_types::SemanticToken;

use crate::builtins;
use crate::position::{offset_to_position, utf16_len};
use crate::targets::targets_with_ranges;

/// Discriminants must match the order of the legend registered in `initialize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TokenType {
    Target = 0,
    Variable = 1,
    Comment = 2,
    Prerequisite = 3,
    #[allow(dead_code)]
    Recipe = 4,
    Keyword = 5,
}

/// Bit positions must match the order of the legend registered in `initialize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TokenModifier {
    Definition = 0,
    DefaultLibrary = 1,
}

impl TokenModifier {
    fn bitmask(self) -> u32 {
        1 << (self as u32)
    }
}

pub struct SemanticTokensBuilder {
    tokens: Vec<SemanticToken>,
    prev_line: u32,
    prev_start: u32,
}

impl SemanticTokensBuilder {
    pub fn new() -> Self {
        Self {
            tokens: Vec::new(),
            prev_line: 0,
            prev_start: 0,
        }
    }

    /// Push a token at an absolute (line, start); deltas are computed against the previous push.
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

    pub fn build(self) -> Vec<SemanticToken> {
        self.tokens
    }
}

/// Generate semantic tokens for a Makefile.
pub fn generate_semantic_tokens(makefile: &Makefile, source_text: &str) -> Vec<SemanticToken> {
    let mut tokens: Vec<(TextRange, TokenType, u32)> = Vec::new();

    tokens.extend(
        makefile
            .comment_ranges()
            .map(|range| (range, TokenType::Comment, 0)),
    );

    for rule in makefile.rules() {
        for (name, range) in targets_with_ranges(&rule) {
            let mut mods = TokenModifier::Definition.bitmask();
            if builtins::SPECIAL_TARGETS.iter().any(|(n, _)| *n == name) {
                mods |= TokenModifier::DefaultLibrary.bitmask();
            }
            tokens.push((range, TokenType::Target, mods));
        }
        tokens.extend(
            rule.prerequisite_ranges()
                .chain(rule.order_only_prerequisite_ranges())
                .map(|range| (range, TokenType::Prerequisite, 0)),
        );
    }

    for def in makefile.variable_definitions() {
        tokens.extend(
            def.keyword_ranges()
                .into_iter()
                .map(|(_, range)| (range, TokenType::Keyword, 0)),
        );
        // TODO: a bare `export A B` only gets a token for its first name;
        // that needs ranges for VariableDefinition::names() upstream.
        if let (Some(name), Some(range)) = (def.name(), def.name_range()) {
            let mut mods = TokenModifier::Definition.bitmask();
            if builtins::find_builtin_variable(&name).is_some() {
                mods |= TokenModifier::DefaultLibrary.bitmask();
            }
            tokens.push((range, TokenType::Variable, mods));
        }
    }

    let keywords = makefile
        .includes()
        .filter_map(|include| include.keyword_range())
        .chain(makefile.all_conditionals().flat_map(|cond| {
            cond.branches()
                .filter_map(|branch| branch.keyword_range())
                .chain(cond.endif_range())
                .collect::<Vec<_>>()
        }))
        .chain(makefile.vpaths().filter_map(|vpath| vpath.keyword_range()))
        .chain(makefile.loads().filter_map(|load| load.keyword_range()));
    tokens.extend(keywords.map(|range| (range, TokenType::Keyword, 0)));

    tokens.sort_by_key(|(range, _, _)| range.start());

    let mut builder = SemanticTokensBuilder::new();
    for (range, token_type, mods) in tokens {
        // Tokens can't span lines, so split them at line breaks.
        let mut offset = range.start();
        for line in source_text[range].split_inclusive('\n') {
            let text = line.trim_end_matches(['\r', '\n']);
            if !text.is_empty() {
                let start = offset_to_position(source_text, offset);
                builder.push(
                    start.line,
                    start.character,
                    utf16_len(text),
                    token_type,
                    mods,
                );
            }
            offset += TextSize::of(line);
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

    /// The variable tokens in `text`, as (line, start, length).
    fn variable_tokens(text: &str) -> Vec<(u32, u32, u32)> {
        let makefile = Makefile::parse(text).tree();
        let mut line = 0;
        let mut start = 0;
        let mut out = Vec::new();
        for token in generate_semantic_tokens(&makefile, text) {
            if token.delta_line > 0 {
                line += token.delta_line;
                start = 0;
            }
            start += token.delta_start;
            if token.token_type == TokenType::Variable as u32 {
                out.push((line, start, token.length));
            }
        }
        out
    }

    #[test]
    fn test_variable_keywords_not_highlighted() {
        assert_eq!(variable_tokens("export CC = gcc\n"), vec![(0, 7, 2)]);
        assert_eq!(
            variable_tokens("override define BODY\nx\nendef\n"),
            vec![(0, 16, 4)]
        );
        assert_eq!(variable_tokens("unexport A B\n"), vec![(0, 9, 1)]);
    }

    /// All tokens in `text`, as (line, start, length, type, modifiers).
    fn all_tokens(text: &str) -> Vec<(u32, u32, u32, TokenType, u32)> {
        let types = [
            TokenType::Target,
            TokenType::Variable,
            TokenType::Comment,
            TokenType::Prerequisite,
            TokenType::Recipe,
            TokenType::Keyword,
        ];
        let makefile = Makefile::parse(text).tree();
        let mut line = 0;
        let mut start = 0;
        let mut out = Vec::new();
        for token in generate_semantic_tokens(&makefile, text) {
            if token.delta_line > 0 {
                line += token.delta_line;
                start = 0;
            }
            start += token.delta_start;
            out.push((
                line,
                start,
                token.length,
                types[token.token_type as usize],
                token.token_modifiers_bitset,
            ));
        }
        out
    }

    const DEF: u32 = 1;
    const DEF_LIB: u32 = 3;

    #[test]
    fn test_directive_keywords() {
        use TokenType::*;
        assert_eq!(
            all_tokens("include a.mk\n-include b.mk\nvpath %.c src\nexport\n"),
            vec![
                (0, 0, 7, Keyword, 0),
                (1, 0, 8, Keyword, 0),
                (2, 0, 5, Keyword, 0),
                (3, 0, 6, Keyword, 0),
            ]
        );
        assert_eq!(
            all_tokens("ifeq ($(A),1)\nifdef B\nX = 1\nendif\nelse ifndef C\nelse\nendif\n"),
            vec![
                (0, 0, 4, Keyword, 0),
                (1, 0, 5, Keyword, 0),
                (2, 0, 1, Variable, DEF),
                (3, 0, 5, Keyword, 0),
                (4, 0, 11, Keyword, 0),
                (5, 0, 4, Keyword, 0),
                (6, 0, 5, Keyword, 0),
            ]
        );
        assert_eq!(
            all_tokens("override private define X =\nbody\nendef\nundefine A B\n"),
            vec![
                (0, 0, 8, Keyword, 0),
                (0, 9, 7, Keyword, 0),
                (0, 17, 6, Keyword, 0),
                (0, 24, 1, Variable, DEF),
                (2, 0, 5, Keyword, 0),
                (3, 0, 8, Keyword, 0),
                (3, 9, 3, Variable, DEF),
            ]
        );
    }

    #[test]
    fn test_continued_comment() {
        use TokenType::*;
        assert_eq!(
            all_tokens("# a \\\n  b\nall: x # c \\\r\n d\n"),
            vec![
                (0, 0, 5, Comment, 0),
                (1, 0, 3, Comment, 0),
                (2, 0, 3, Target, DEF),
                (2, 5, 1, Prerequisite, 0),
                (2, 7, 5, Comment, 0),
                (3, 0, 2, Comment, 0),
            ]
        );
    }

    #[test]
    fn test_comment_tokens() {
        use TokenType::*;
        assert_eq!(
            all_tokens("#!/usr/bin/make -f\nifdef A # g\nendif # h\nall:\n\techo # e\n\t# f\n"),
            vec![
                (0, 0, 18, Comment, 0),
                (1, 0, 5, Keyword, 0),
                (1, 8, 3, Comment, 0),
                (2, 0, 5, Keyword, 0),
                (2, 6, 3, Comment, 0),
                (3, 0, 3, Target, DEF),
                (5, 1, 3, Comment, 0),
            ]
        );
    }

    #[test]
    fn test_load_keyword() {
        assert_eq!(
            all_tokens("-load foo.so\n"),
            vec![(0, 0, 5, TokenType::Keyword, 0)]
        );
        assert_eq!(
            all_tokens("ifdef A\nload foo.so\nendif\n"),
            vec![
                (0, 0, 5, TokenType::Keyword, 0),
                (1, 0, 4, TokenType::Keyword, 0),
                (2, 0, 5, TokenType::Keyword, 0),
            ]
        );
    }

    #[test]
    fn test_rule_tokens() {
        use TokenType::*;
        assert_eq!(
            all_tokens(".PHONY: all\n$(OUT) a\\#b: x$(Y)z lib.a(m.o) \\\n  c | d # c\n\techo\n"),
            vec![
                (0, 0, 6, Target, DEF_LIB),
                (0, 8, 3, Prerequisite, 0),
                (1, 0, 6, Target, DEF),
                (1, 7, 4, Target, DEF),
                (1, 13, 6, Prerequisite, 0),
                (1, 20, 10, Prerequisite, 0),
                (2, 2, 1, Prerequisite, 0),
                (2, 6, 1, Prerequisite, 0),
                (2, 8, 3, Comment, 0),
            ]
        );
    }

    #[test]
    fn test_variable_tokens() {
        use TokenType::*;
        assert_eq!(
            all_tokens("$(X)_Y = 1\nexport CC = gcc # c\nx: CFLAGS = -O2\n"),
            vec![
                (0, 0, 6, Variable, DEF),
                (1, 0, 6, Keyword, 0),
                (1, 7, 2, Variable, DEF_LIB),
                (1, 16, 3, Comment, 0),
                (2, 0, 1, Target, DEF),
                (2, 3, 6, Variable, DEF_LIB),
            ]
        );
    }
}
