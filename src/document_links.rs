//! Document links for Makefiles.
//!
//! Makes `include`/`-include`/`sinclude` paths clickable.

use tower_lsp_server::ls_types::{DocumentLink, Uri};

use crate::position::text_range_to_lsp_range;
use crate::workspace::{FileSet, Resolution};

/// Generate document links for include directives.
///
/// Links point at the file make would read, or would try to read if it
/// doesn't exist. Names that can't be resolved statically get no link.
pub fn get_document_links(files: &FileSet) -> Vec<DocumentLink> {
    let doc = files.current();
    files
        .includes()
        .iter()
        .filter_map(|inc| {
            let path = match &inc.resolution {
                Resolution::Found(p) | Resolution::Missing(p) | Resolution::Unreadable(p, _) => p,
                Resolution::Unresolved => return None,
            };
            let Some(target) = Uri::from_file_path(path) else {
                tracing::warn!("unable to convert {} to a URI", path.display());
                return None;
            };
            Some(DocumentLink {
                range: text_range_to_lsp_range(doc.text(), inc.path.range),
                target: Some(target),
                tooltip: Some(format!("Open {}", inc.path.name)),
                data: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::workspace::Workspace;

    fn get_links(text: &str) -> Vec<DocumentLink> {
        let uri: Uri = "file:///home/user/project/Makefile".parse().unwrap();
        let mut ws = Workspace::new();
        ws.open(uri.clone(), text.to_string());
        get_document_links(&ws.file_set(&uri).unwrap())
    }

    #[test]
    fn test_include_link() {
        let links = get_links("include config.mk\n");
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].target.as_ref().unwrap().to_string(),
            "file:///home/user/project/config.mk"
        );
    }

    #[test]
    fn test_dash_include_link() {
        let links = get_links("-include .env\n");
        assert_eq!(links.len(), 1);
    }

    #[test]
    fn test_absolute_path_include() {
        let links = get_links("include /etc/make.conf\n");
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].target.as_ref().unwrap().to_string(),
            "file:///etc/make.conf"
        );
    }

    #[test]
    fn test_no_links_without_includes() {
        let links = get_links("all:\n\techo done\n");
        assert!(links.is_empty());
    }

    #[test]
    fn test_link_tooltip() {
        let links = get_links("include config.mk\n");
        assert_eq!(links[0].tooltip.as_deref(), Some("Open config.mk"));
    }

    #[test]
    fn test_variable_in_path_skipped() {
        let links = get_links("include $(CONF_DIR)/config.mk\n");
        assert!(links.is_empty());
    }

    #[test]
    fn test_several_paths_in_one_directive() {
        let links = get_links("include a.mk b.mk\n");
        let targets: Vec<(u32, u32, String)> = links
            .iter()
            .map(|l| {
                (
                    l.range.start.character,
                    l.range.end.character,
                    l.target.as_ref().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            targets,
            vec![
                (8, 12, "file:///home/user/project/a.mk".to_string()),
                (13, 17, "file:///home/user/project/b.mk".to_string()),
            ]
        );
    }

    #[test]
    fn test_literal_variable_in_path_expanded() {
        let links = get_links("CONF_DIR = conf\ninclude $(CONF_DIR)/config.mk\n");
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].target.as_ref().unwrap().to_string(),
            "file:///home/user/project/conf/config.mk"
        );
    }
}
