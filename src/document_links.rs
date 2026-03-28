//! Document links for Makefiles.
//!
//! Makes `include`/`-include`/`sinclude` paths clickable.

use makefile_lossless::{Makefile, MakefileItem};
use tower_lsp_server::ls_types::{DocumentLink, Uri};

use crate::position::text_range_to_lsp_range;

/// Generate document links for include directives.
///
/// Resolves include paths relative to the directory of the current file.
pub fn get_document_links(
    makefile: &Makefile,
    source_text: &str,
    document_uri: &Uri,
) -> Vec<DocumentLink> {
    let mut links = Vec::new();

    let base_dir = document_uri
        .path()
        .as_str()
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or(".");

    for item in makefile.items() {
        if let MakefileItem::Include(inc) = item {
            let Some(path) = inc.path() else {
                continue;
            };
            let path = path.trim().to_string();
            if path.is_empty() || path.contains('$') {
                continue;
            }

            let Some(path_text_range) = inc.path_range() else {
                continue;
            };
            let range = text_range_to_lsp_range(source_text, path_text_range);

            let target_path = if path.starts_with('/') {
                path.clone()
            } else {
                format!("{}/{}", base_dir, path)
            };

            let Ok(target_uri) = format!("file://{}", target_path).parse::<Uri>() else {
                continue;
            };

            links.push(DocumentLink {
                range,
                target: Some(target_uri),
                tooltip: Some(format!("Open {}", path)),
                data: None,
            });
        }
    }

    links
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_links(text: &str) -> Vec<DocumentLink> {
        let parsed = Makefile::parse(text);
        let makefile = parsed.tree();
        let uri: Uri = "file:///home/user/project/Makefile".parse().unwrap();
        get_document_links(&makefile, text, &uri)
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
}
