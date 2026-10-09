//! Call hierarchy for targets: the prerequisites of a target are its
//! outgoing calls, and the targets that list it as a prerequisite are its
//! incoming calls.

use tower_lsp_server::ls_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, Position, Range,
    SymbolKind,
};

use crate::dep_graph::is_graph_target;
use crate::position::{text_range_to_lsp_range, try_position_to_offset};
use crate::references::{symbol_at, Symbol};
use crate::targets::{prerequisites_with_ranges, targets_with_ranges};
use crate::workspace::{Document, FileSet};

/// The item for the first rule in `doc` that defines `name`.
fn item_in(doc: &Document, name: &str) -> Option<CallHierarchyItem> {
    doc.makefile().rules().find_map(|rule| {
        let (_, target_range) = targets_with_ranges(&rule)
            .into_iter()
            .find(|(target, _)| target == name)?;
        Some(CallHierarchyItem {
            name: name.to_string(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: doc.uri().clone(),
            range: text_range_to_lsp_range(doc.text(), rule.text_range()),
            selection_range: text_range_to_lsp_range(doc.text(), target_range),
            data: None,
        })
    })
}

/// The item for the first rule defining `name`, in the order of the
/// documents in `files`.
fn item_for(files: &FileSet, name: &str) -> Option<CallHierarchyItem> {
    if !is_graph_target(name) {
        return None;
    }
    files.docs().find_map(|doc| item_in(doc, name))
}

/// Push `range` onto the ranges for `key`, keeping keys in the order they
/// were first seen.
fn add<K: PartialEq>(entries: &mut Vec<(K, Vec<Range>)>, key: K, range: Option<Range>) {
    let index = match entries.iter().position(|(k, _)| *k == key) {
        Some(index) => index,
        None => {
            entries.push((key, vec![]));
            entries.len() - 1
        }
    };
    entries[index].1.extend(range);
}

/// The call hierarchy item for the target at `position` in the current
/// document.
pub fn prepare(files: &FileSet, position: Position) -> Option<Vec<CallHierarchyItem>> {
    let current = files.current();
    let offset = try_position_to_offset(current.text(), position)?;
    let Symbol::Target(name) = symbol_at(&current.makefile(), offset.into())? else {
        return None;
    };
    Some(vec![item_for(files, &name)?])
}

/// The targets that list the target of `item` as a prerequisite, in any
/// rule for them.
///
/// A referring target is reported once for each document with rules
/// referring to it, at its first rule in that document, so that the ranges
/// of the references are in the document of the caller.
pub fn incoming_calls(files: &FileSet, item: &CallHierarchyItem) -> Vec<CallHierarchyIncomingCall> {
    let docs: Vec<&Document> = files.docs().collect();
    let mut entries: Vec<((String, usize), Vec<Range>)> = vec![];
    for (index, doc) in docs.iter().enumerate() {
        for rule in doc.makefile().rules() {
            let ranges: Vec<Range> = prerequisites_with_ranges(&rule)
                .into_iter()
                .filter(|(prereq, _)| *prereq == item.name)
                .map(|(_, range)| text_range_to_lsp_range(doc.text(), range))
                .collect();
            if ranges.is_empty() {
                continue;
            }
            for target in rule.targets() {
                if target == item.name || !is_graph_target(&target) {
                    continue;
                }
                for range in &ranges {
                    add(&mut entries, (target.clone(), index), Some(*range));
                }
            }
        }
    }
    entries
        .into_iter()
        .filter_map(|((name, index), from_ranges)| {
            Some(CallHierarchyIncomingCall {
                from: item_in(docs[index], &name)?,
                from_ranges,
            })
        })
        .collect()
}

/// The normal and order-only prerequisites of the target of `item` that are
/// themselves targets, from all rules for it.
///
/// LSP wants the ranges of the prerequisites relative to `item`, so those
/// in rules in other documents than that of `item` are left out.
pub fn outgoing_calls(files: &FileSet, item: &CallHierarchyItem) -> Vec<CallHierarchyOutgoingCall> {
    let mut entries: Vec<(String, Vec<Range>)> = vec![];
    for doc in files.docs() {
        let same_doc = doc.uri() == &item.uri;
        for rule in doc.makefile().rules().filter(|r| r.has_target(&item.name)) {
            for (prereq, range) in prerequisites_with_ranges(&rule) {
                if prereq == item.name {
                    continue;
                }
                let range = same_doc.then(|| text_range_to_lsp_range(doc.text(), range));
                add(&mut entries, prereq, range);
            }
        }
    }
    entries
        .into_iter()
        .filter_map(|(name, from_ranges)| {
            Some(CallHierarchyOutgoingCall {
                to: item_for(files, &name)?,
                from_ranges,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::tests::Fixture;
    use crate::workspace::Document;
    use tower_lsp_server::ls_types::Uri;

    fn test_uri() -> Uri {
        "file:///test/Makefile".parse().unwrap()
    }

    fn single(text: &str) -> FileSet {
        FileSet::single(Document::new(test_uri(), text.to_string()))
    }

    fn range(line: u32, start: u32, end_line: u32, end: u32) -> Range {
        Range::new(Position::new(line, start), Position::new(end_line, end))
    }

    fn word(line: u32, start: u32, end: u32) -> Range {
        range(line, start, line, end)
    }

    fn item(uri: &Uri, name: &str, range: Range, selection_range: Range) -> CallHierarchyItem {
        CallHierarchyItem {
            name: name.to_string(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: uri.clone(),
            range,
            selection_range,
            data: None,
        }
    }

    fn prepare_one(files: &FileSet, line: u32, character: u32) -> CallHierarchyItem {
        let items = prepare(files, Position::new(line, character)).unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        items.into_iter().next().unwrap()
    }

    /// Outgoing calls as (name, from ranges).
    fn outgoing(files: &FileSet, item: &CallHierarchyItem) -> Vec<(String, Vec<Range>)> {
        outgoing_calls(files, item)
            .into_iter()
            .map(|c| (c.to.name, c.from_ranges))
            .collect()
    }

    /// Incoming calls as (name, uri, from ranges).
    fn incoming(files: &FileSet, item: &CallHierarchyItem) -> Vec<(String, Uri, Vec<Range>)> {
        incoming_calls(files, item)
            .into_iter()
            .map(|c| (c.from.name, c.from.uri, c.from_ranges))
            .collect()
    }

    fn calls(items: &[(&str, Vec<Range>)]) -> Vec<(String, Vec<Range>)> {
        items
            .iter()
            .map(|(n, r)| (n.to_string(), r.clone()))
            .collect()
    }

    #[test]
    fn test_prepare_on_definition() {
        let files = single("all: build\n\nbuild:\n\techo ok\n");
        assert_eq!(
            prepare(&files, Position::new(2, 1)),
            Some(vec![item(
                &test_uri(),
                "build",
                range(2, 0, 4, 0),
                word(2, 0, 5)
            )])
        );
    }

    #[test]
    fn test_prepare_on_prerequisite() {
        let files = single("all: build\n\nbuild:\n\techo ok\n");
        assert_eq!(
            prepare(&files, Position::new(0, 6)),
            Some(vec![item(
                &test_uri(),
                "build",
                range(2, 0, 4, 0),
                word(2, 0, 5)
            )])
        );
    }

    #[test]
    fn test_prepare_nothing() {
        let files = single("X = 1\nall: foo.c\n%.o: %.c\n.PHONY: all\n");
        // A variable.
        assert_eq!(prepare(&files, Position::new(0, 0)), None);
        // A prerequisite that is not a target.
        assert_eq!(prepare(&files, Position::new(1, 6)), None);
        // A pattern rule.
        assert_eq!(prepare(&files, Position::new(2, 1)), None);
        // A special target.
        assert_eq!(prepare(&files, Position::new(3, 1)), None);
    }

    #[test]
    fn test_outgoing() {
        let files = single("all: a b | c\na:\nb:\nc:\n");
        let all = prepare_one(&files, 0, 0);
        assert_eq!(
            outgoing(&files, &all),
            calls(&[
                ("a", vec![word(0, 5, 6)]),
                ("b", vec![word(0, 7, 8)]),
                ("c", vec![word(0, 11, 12)]),
            ])
        );
        let a = outgoing_calls(&files, &all).remove(0).to;
        assert_eq!(a, item(&test_uri(), "a", range(1, 0, 2, 0), word(1, 0, 1)));
    }

    #[test]
    fn test_outgoing_skips_files_and_self() {
        let files = single("prog: main.o prog $(OBJS)\nmain.o: main.c\n");
        let prog = prepare_one(&files, 0, 0);
        assert_eq!(
            outgoing(&files, &prog),
            calls(&[("main.o", vec![word(0, 6, 12)])])
        );
    }

    #[test]
    fn test_outgoing_merges_rules() {
        let files = single("all: a\nall: b a\na:\nb:\n");
        let all = prepare_one(&files, 0, 0);
        assert_eq!(all.selection_range, word(0, 0, 3));
        assert_eq!(
            outgoing(&files, &all),
            calls(&[
                ("a", vec![word(0, 5, 6), word(1, 7, 8)]),
                ("b", vec![word(1, 5, 6)]),
            ])
        );
    }

    #[test]
    fn test_double_colon() {
        let files = single("all:: a\nall:: b\na:\nb:\n");
        let all = prepare_one(&files, 1, 0);
        assert_eq!(
            outgoing(&files, &all),
            calls(&[("a", vec![word(0, 6, 7)]), ("b", vec![word(1, 6, 7)])])
        );
        let b = prepare_one(&files, 3, 0);
        assert_eq!(
            incoming(&files, &b),
            vec![("all".to_string(), test_uri(), vec![word(1, 6, 7)])]
        );
    }

    #[test]
    fn test_grouped_targets() {
        let files = single("all: x\nx y &: gen\ngen:\n");
        let gen = prepare_one(&files, 2, 0);
        assert_eq!(
            incoming(&files, &gen),
            vec![
                ("x".to_string(), test_uri(), vec![word(1, 7, 10)]),
                ("y".to_string(), test_uri(), vec![word(1, 7, 10)]),
            ]
        );
        let y = prepare_one(&files, 1, 2);
        assert_eq!(y, item(&test_uri(), "y", range(1, 0, 2, 0), word(1, 2, 3)));
        assert_eq!(
            outgoing(&files, &y),
            calls(&[("gen", vec![word(1, 7, 10)])])
        );
    }

    #[test]
    fn test_incoming() {
        let files = single("all: lib prog\nprog: lib\nlib:\n.PHONY: lib\n%.o: lib\n");
        let lib = prepare_one(&files, 2, 0);
        assert_eq!(
            incoming(&files, &lib),
            vec![
                ("all".to_string(), test_uri(), vec![word(0, 5, 8)]),
                ("prog".to_string(), test_uri(), vec![word(1, 6, 9)]),
            ]
        );
    }

    #[test]
    fn test_incoming_merges_rules() {
        let files = single("all: a\nall: b a\na:\nb:\n");
        let a = prepare_one(&files, 2, 0);
        let calls = incoming_calls(&files, &a);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].from, prepare_one(&files, 0, 0));
        assert_eq!(calls[0].from_ranges, vec![word(0, 5, 6), word(1, 7, 8)]);
    }

    #[test]
    fn test_cycle() {
        let files = single("a: b\nb: a\n");
        let a = prepare_one(&files, 0, 0);
        let b = outgoing_calls(&files, &a).remove(0).to;
        assert_eq!(b.name, "b");
        let back = outgoing_calls(&files, &b).remove(0).to;
        assert_eq!(back, a);
        assert_eq!(
            incoming(&files, &a),
            vec![("b".to_string(), test_uri(), vec![word(1, 3, 4)])]
        );
    }

    #[test]
    fn test_included_makefile() {
        let fixture = Fixture::new(&[
            ("Makefile", "all: lib\ninclude rules.mk\n"),
            ("rules.mk", "lib: gen\ngen:\nall: gen\n"),
        ]);
        let files = fixture.file_set("Makefile");
        let main = fixture.uri("Makefile");
        let rules = fixture.uri("rules.mk");

        let lib = prepare_one(&files, 0, 6);
        assert_eq!(lib, item(&rules, "lib", range(0, 0, 1, 0), word(0, 0, 3)));

        let all = prepare_one(&files, 0, 0);
        assert_eq!(all.uri, main);
        // The occurrence of gen in rules.mk can't be given relative to the
        // definition of all in the Makefile.
        assert_eq!(
            outgoing(&files, &all),
            calls(&[("lib", vec![word(0, 5, 8)]), ("gen", vec![])])
        );

        // Requests for an item in rules.mk use the file set of rules.mk,
        // which includes the Makefile that includes it.
        let files = fixture.file_set("rules.mk");
        let gen = prepare_one(&files, 1, 0);
        assert_eq!(
            incoming(&files, &gen),
            vec![
                ("lib".to_string(), rules.clone(), vec![word(0, 5, 8)]),
                ("all".to_string(), rules.clone(), vec![word(2, 5, 8)]),
            ]
        );
        assert_eq!(
            incoming(&files, &lib),
            vec![("all".to_string(), main, vec![word(0, 5, 8)])]
        );
    }
}
