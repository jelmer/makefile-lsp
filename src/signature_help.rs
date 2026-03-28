//! Signature help for Makefile built-in functions.
//!
//! Shows parameter information when the cursor is inside a function call
//! like `$(subst from,to,text)`.

use makefile_lossless::Makefile;
use tower_lsp_server::ls_types::{
    Documentation, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, Position,
    SignatureHelp, SignatureInformation,
};

use crate::builtins;
use crate::position::try_position_to_offset;

/// Get signature help for the cursor position.
pub fn get_signature_help(
    makefile: &Makefile,
    source_text: &str,
    position: Position,
) -> Option<SignatureHelp> {
    let offset = try_position_to_offset(source_text, position)?;
    let byte_offset: usize = offset.into();

    // Find a variable reference containing this offset that is a function call
    for var_ref in makefile.variable_references() {
        let range = var_ref.text_range();
        let start: usize = range.start().into();
        let end: usize = range.end().into();

        if byte_offset < start || byte_offset > end {
            continue;
        }

        if !var_ref.is_function_call() {
            continue;
        }

        let func_name = var_ref.name()?;
        let active_param = var_ref.argument_index_at_offset(byte_offset)?;

        let f = builtins::find_builtin_function(&func_name)?;

        let parameters: Vec<ParameterInformation> = f
            .params
            .iter()
            .map(|label| ParameterInformation {
                label: ParameterLabel::Simple(label.to_string()),
                documentation: None,
            })
            .collect();

        let label = format!("$({} {})", f.name, f.params.join(","));

        return Some(SignatureHelp {
            signatures: vec![SignatureInformation {
                label,
                documentation: Some(Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: f.doc.to_string(),
                })),
                parameters: Some(parameters),
                active_parameter: Some(active_param as u32),
            }],
            active_signature: Some(0),
            active_parameter: Some(active_param as u32),
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_sig(text: &str, pos: Position) -> Option<SignatureHelp> {
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        get_signature_help(&makefile, text, pos)
    }

    #[test]
    fn test_subst_first_arg() {
        let text = "X = $(subst a,b,text)\n";
        // Cursor on 'a' (first arg) — col 12
        let sig = get_sig(text, Position::new(0, 12));
        assert!(sig.is_some());
        let sig = sig.unwrap();
        assert_eq!(sig.active_parameter, Some(0));
        assert!(sig.signatures[0].label.contains("subst"));
    }

    #[test]
    fn test_subst_second_arg() {
        let text = "X = $(subst a,b,text)\n";
        // Cursor on 'b' (second arg) — col 14
        let sig = get_sig(text, Position::new(0, 14));
        assert!(sig.is_some());
        assert_eq!(sig.unwrap().active_parameter, Some(1));
    }

    #[test]
    fn test_subst_third_arg() {
        let text = "X = $(subst a,b,text)\n";
        // Cursor on 't' in 'text' (third arg) — col 16
        let sig = get_sig(text, Position::new(0, 16));
        assert!(sig.is_some());
        assert_eq!(sig.unwrap().active_parameter, Some(2));
    }

    #[test]
    fn test_wildcard_single_arg() {
        let text = "FILES = $(wildcard *.c)\n";
        let sig = get_sig(text, Position::new(0, 19));
        assert!(sig.is_some());
        assert_eq!(sig.unwrap().active_parameter, Some(0));
    }

    #[test]
    fn test_no_sig_for_variable() {
        let text = "X = $(CC)\n";
        let sig = get_sig(text, Position::new(0, 6));
        assert!(sig.is_none());
    }

    #[test]
    fn test_no_sig_outside_function() {
        let text = "CC = gcc\n";
        let sig = get_sig(text, Position::new(0, 5));
        assert!(sig.is_none());
    }
}
