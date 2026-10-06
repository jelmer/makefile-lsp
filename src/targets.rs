//! Source ranges of rule targets.

use makefile_lossless::{Lang, Rule, SyntaxKind, TextRange};
use rowan::ast::AstNode;

type SyntaxToken = rowan::SyntaxToken<Lang>;
type SyntaxElement = rowan::SyntaxElement<Lang>;

/// Whether `token` is the backslash of a line continuation, i.e. a backslash
/// before a newline that is not itself escaped.
fn is_continuation_backslash(token: &SyntaxToken) -> bool {
    token.kind() == SyntaxKind::BACKSLASH
        && token
            .next_token()
            .is_some_and(|t| t.kind() == SyntaxKind::NEWLINE)
        && std::iter::successors(token.prev_token(), |t| t.prev_token())
            .take_while(|t| t.kind() == SyntaxKind::BACKSLASH)
            .count()
            % 2
            == 0
}

/// Whether `element` separates targets: whitespace, or part of a line
/// continuation (backslash, newline or the continued line's indent).
fn is_separator(element: &SyntaxElement) -> bool {
    let Some(token) = element.as_token() else {
        return false;
    };
    match token.kind() {
        SyntaxKind::WHITESPACE => true,
        SyntaxKind::BACKSLASH => is_continuation_backslash(token),
        SyntaxKind::NEWLINE => token
            .prev_token()
            .is_some_and(|t| is_continuation_backslash(&t)),
        SyntaxKind::INDENT => token.prev_token().is_some_and(|t| is_separator(&t.into())),
        _ => false,
    }
}

/// The source range of each target of `rule`, in the order of
/// [`Rule::targets`].
// TODO: use Rule::target_ranges once makefile-lossless > 0.4.0 is released
pub fn target_ranges(rule: &Rule) -> Vec<TextRange> {
    let Some(node) = rule
        .syntax()
        .children()
        .find(|n| n.kind() == SyntaxKind::TARGETS)
    else {
        return vec![];
    };
    let mut ranges = Vec::new();
    let mut current: Option<TextRange> = None;
    for child in node.children_with_tokens() {
        if is_separator(&child) {
            ranges.extend(current.take());
            continue;
        }
        let range = child.text_range();
        current = Some(current.map_or(range, |c| c.cover(range)));
    }
    ranges.extend(current);
    ranges
}

/// The targets of `rule` together with their source ranges.
pub fn targets_with_ranges(rule: &Rule) -> Vec<(String, TextRange)> {
    let targets: Vec<String> = rule.targets().collect();
    let ranges = target_ranges(rule);
    assert_eq!(
        targets.len(),
        ranges.len(),
        "target ranges out of sync with targets in {:?}",
        rule.syntax().text()
    );
    targets.into_iter().zip(ranges).collect()
}

/// The target of `rule` whose source range contains `offset`.
pub fn target_at_offset(rule: &Rule, offset: usize) -> Option<(String, TextRange)> {
    targets_with_ranges(rule).into_iter().find(|(_, range)| {
        usize::from(range.start()) <= offset && offset < usize::from(range.end())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use makefile_lossless::Makefile;

    fn texts(text: &str) -> Vec<(String, String)> {
        let makefile = Makefile::parse(text).tree();
        let rule = makefile.rules().next().unwrap();
        targets_with_ranges(&rule)
            .into_iter()
            .map(|(name, range)| (name, text[range].to_string()))
            .collect()
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn test_multiple_targets() {
        assert_eq!(
            texts("a bb  ccc: d\n"),
            pairs(&[("a", "a"), ("bb", "bb"), ("ccc", "ccc")])
        );
    }

    #[test]
    fn test_ranges_are_offsets() {
        let makefile = Makefile::parse("x a b: c\n").tree();
        let rule = makefile.rules().next().unwrap();
        assert_eq!(
            target_ranges(&rule),
            vec![
                TextRange::new(0.into(), 1.into()),
                TextRange::new(2.into(), 3.into()),
                TextRange::new(4.into(), 5.into()),
            ]
        );
    }

    #[test]
    fn test_variable_references_and_archives() {
        assert_eq!(
            texts("$(OUT) x$(Y)z lib.a(a.o b.o): c\n"),
            pairs(&[
                ("$(OUT)", "$(OUT)"),
                ("x$(Y)z", "x$(Y)z"),
                ("lib.a(a.o b.o)", "lib.a(a.o b.o)"),
            ])
        );
    }

    #[test]
    fn test_continuation() {
        assert_eq!(texts("a \\\n  b: c\n"), pairs(&[("a", "a"), ("b", "b")]));
    }

    #[test]
    fn test_escaped_hash() {
        assert_eq!(
            texts("a\\#b c: d\n"),
            pairs(&[("a#b", "a\\#b"), ("c", "c")])
        );
    }

    #[test]
    fn test_target_at_offset() {
        let makefile = Makefile::parse("a b: c\n").tree();
        let rule = makefile.rules().next().unwrap();
        assert_eq!(
            target_at_offset(&rule, 2),
            Some(("b".to_string(), TextRange::new(2.into(), 3.into())))
        );
        assert_eq!(target_at_offset(&rule, 1), None);
        assert_eq!(target_at_offset(&rule, 5), None);
    }
}
