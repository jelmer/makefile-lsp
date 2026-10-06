//! Which branches of which conditionals a node is in.

use makefile_lossless::SyntaxKind;
use text_size::TextRange;

/// For each conditional enclosing a node, its range and the index of the
/// branch the node is in.
pub type Branches = Vec<(TextRange, usize)>;

pub fn conditional_branches(node: &rowan::SyntaxNode<makefile_lossless::Lang>) -> Branches {
    node.ancestors()
        .zip(node.ancestors().skip(1))
        .filter(|(_, parent)| parent.kind() == SyntaxKind::CONDITIONAL)
        .map(|(child, conditional)| {
            let branch = conditional
                .children()
                .take_while(|c| c != &child)
                .filter(|c| c.kind() == SyntaxKind::CONDITIONAL_ELSE)
                .count();
            (conditional.text_range(), branch)
        })
        .collect()
}

/// Whether two sets of conditional branches can never be taken together.
pub fn mutually_exclusive(a: &[(TextRange, usize)], b: &[(TextRange, usize)]) -> bool {
    a.iter()
        .any(|(cond, branch)| b.iter().any(|(c, br)| c == cond && br != branch))
}

/// The branches taken when both `a` and `b` are, sorted so that equal sets
/// compare equal, or `None` if they are mutually exclusive.
pub fn combine(a: &[(TextRange, usize)], b: &[(TextRange, usize)]) -> Option<Branches> {
    if mutually_exclusive(a, b) {
        return None;
    }
    let mut combined: Branches = a.iter().chain(b).copied().collect();
    combined.sort_by_key(|(range, branch)| (range.start(), range.end(), *branch));
    combined.dedup();
    Some(combined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use makefile_lossless::{Makefile, Parse};
    use rowan::ast::AstNode;

    fn rule_branches(text: &str) -> Vec<Branches> {
        let parsed: Parse<Makefile> = Makefile::parse(text);
        parsed
            .tree()
            .rules()
            .map(|r| conditional_branches(r.syntax()))
            .collect()
    }

    #[test]
    fn combine_rejects_other_branch() {
        let b = rule_branches("ifdef X\na:\nelse\nb:\nendif\nc:\n");
        assert_eq!(combine(&b[0], &b[1]), None);
        assert_eq!(combine(&b[0], &b[2]), Some(b[0].clone()));
        assert_eq!(combine(&b[0], &b[0]), Some(b[0].clone()));
    }

    #[test]
    fn combine_merges_unrelated_conditionals() {
        let b = rule_branches("ifdef X\na:\nendif\nifdef Y\nb:\nendif\n");
        let combined = vec![b[0][0], b[1][0]];
        assert_eq!(combine(&b[0], &b[1]), Some(combined.clone()));
        assert_eq!(combine(&b[1], &b[0]), Some(combined));
    }
}
