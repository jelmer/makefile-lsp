//! Source ranges of rule targets and prerequisites.

use makefile_lossless::{Rule, SyntaxKind, TextRange};
use rowan::ast::AstNode;

/// The targets of `rule` together with their source ranges.
pub fn targets_with_ranges(rule: &Rule) -> Vec<(String, TextRange)> {
    let targets: Vec<String> = rule.targets().collect();
    let ranges: Vec<TextRange> = rule.target_ranges().collect();
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

/// The normal and order-only prerequisites of `rule` together with their
/// source ranges, in source order.
pub fn prerequisites_with_ranges(rule: &Rule) -> Vec<(String, TextRange)> {
    let prereqs: Vec<String> = rule
        .prerequisites()
        .chain(rule.order_only_prerequisites())
        .collect();
    let ranges: Vec<TextRange> = rule
        .syntax()
        .children_with_tokens()
        .skip_while(|e| e.kind() != SyntaxKind::OPERATOR)
        .find_map(|e| {
            e.into_node()
                .filter(|n| n.kind() == SyntaxKind::PREREQUISITES)
        })
        .into_iter()
        .flat_map(|n| n.children())
        .filter(|n| n.kind() == SyntaxKind::PREREQUISITE)
        .map(|n| n.text_range())
        .collect();
    assert_eq!(
        prereqs.len(),
        ranges.len(),
        "prerequisite ranges out of sync with prerequisites in {:?}",
        rule.syntax().text()
    );
    prereqs.into_iter().zip(ranges).collect()
}

/// The prerequisite of `rule` whose source range contains `offset`.
pub fn prerequisite_at_offset(rule: &Rule, offset: usize) -> Option<(String, TextRange)> {
    prerequisites_with_ranges(rule)
        .into_iter()
        .find(|(_, range)| {
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
            targets_with_ranges(&rule),
            vec![
                ("x".to_string(), TextRange::new(0.into(), 1.into())),
                ("a".to_string(), TextRange::new(2.into(), 3.into())),
                ("b".to_string(), TextRange::new(4.into(), 5.into())),
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

    fn prereq_texts(text: &str) -> Vec<(String, String)> {
        let makefile = Makefile::parse(text).tree();
        let rule = makefile.rules().next().unwrap();
        prerequisites_with_ranges(&rule)
            .into_iter()
            .map(|(name, range)| (name, text[range].to_string()))
            .collect()
    }

    #[test]
    fn test_prerequisite_ranges() {
        assert_eq!(
            prereq_texts("x: a\\#b $(Y) lib.a(m.o) c \\\n  d | e ; echo\n"),
            pairs(&[
                ("a#b", "a\\#b"),
                ("$(Y)", "$(Y)"),
                ("lib.a(m.o)", "lib.a(m.o)"),
                ("c", "c"),
                ("d", "d"),
                ("e", "e"),
            ])
        );
    }

    #[test]
    fn test_prerequisite_ranges_target_specific_variable() {
        assert_eq!(prereq_texts("x: CFLAGS = -O2\n"), pairs(&[]));
    }

    #[test]
    fn test_prerequisite_at_offset() {
        let makefile = Makefile::parse("a: b cc\n").tree();
        let rule = makefile.rules().next().unwrap();
        assert_eq!(
            prerequisite_at_offset(&rule, 6),
            Some(("cc".to_string(), TextRange::new(5.into(), 7.into())))
        );
        assert_eq!(prerequisite_at_offset(&rule, 4), None);
        assert_eq!(prerequisite_at_offset(&rule, 0), None);
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
