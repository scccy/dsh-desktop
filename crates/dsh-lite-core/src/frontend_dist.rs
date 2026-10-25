//! Resolution of the packaged Web frontend inside a global `dsh` install.
//!
//! The renderer is the official `dsh-web-frontend` build that ships inside the
//! installed `@deepseek-ai/dsh` package; the shell never bundles its own copy
//! (see `docs/desktop-parity.md`, G1/G2). The official Electron shell reads it
//! from `resources.dsh/node_modules/@deepseek-ai/dsh-web-frontend/dist`, and this
//! module resolves the same directory for our install.
//!
//! Both the `dsh-app://` scheme handler and the loopback application server serve
//! that directory, so the resolution lives here instead of in either of them.

use std::path::{Path, PathBuf};

/// Package that carries the built renderer.
pub const FRONTEND_PACKAGE: &str = "@deepseek-ai/dsh-web-frontend";

/// Document that proves a directory really is the renderer build.
pub const INDEX_DOCUMENT: &str = "index.html";

/// `dist` directory of the renderer inside `dsh_package`, when it is usable.
///
/// `dsh_package` is the directory of the installed `@deepseek-ai/dsh` package,
/// for example `/usr/local/lib/node_modules/@deepseek-ai/dsh`. Returns `None`
/// when the frontend is missing or its `index.html` is not a file, so the caller
/// can show a clear error instead of serving an empty document.
pub fn frontend_dist(dsh_package: &Path) -> Option<PathBuf> {
    let dist = dist_dir(dsh_package);
    is_renderer_build(&dist).then_some(dist)
}

/// `<dsh_package>/node_modules/@deepseek-ai/dsh-web-frontend/dist`, unchecked.
pub fn dist_dir(dsh_package: &Path) -> PathBuf {
    dsh_package
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh-web-frontend")
        .join("dist")
}

/// Whether `dist` holds the renderer's index document.
pub fn is_renderer_build(dist: &Path) -> bool {
    dist.join(INDEX_DOCUMENT).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Builds `<root>/node_modules/@deepseek-ai/dsh-web-frontend/dist/index.html`.
    fn fake_install(root: &Path) -> PathBuf {
        let dist = dist_dir(root);
        fs::create_dir_all(dist.join("assets")).expect("dist dir");
        fs::write(dist.join(INDEX_DOCUMENT), "<html><head></head></html>").expect("index");
        dist
    }

    #[test]
    fn a_complete_install_resolves_to_its_dist_directory() {
        let directory = tempfile::tempdir().expect("temp dir");
        let dist = fake_install(directory.path());
        assert_eq!(frontend_dist(directory.path()), Some(dist.clone()));
        assert!(dist.ends_with("node_modules/@deepseek-ai/dsh-web-frontend/dist"));
        assert!(is_renderer_build(&dist));
    }

    #[test]
    fn a_missing_frontend_is_not_a_renderer_build() {
        let directory = tempfile::tempdir().expect("temp dir");
        assert_eq!(frontend_dist(directory.path()), None);
        assert!(!is_renderer_build(&dist_dir(directory.path())));
    }

    #[test]
    fn a_dist_without_an_index_document_is_rejected() {
        let directory = tempfile::tempdir().expect("temp dir");
        let dist = dist_dir(directory.path());
        fs::create_dir_all(&dist).expect("dist dir");
        assert_eq!(frontend_dist(directory.path()), None);

        // A directory where the index document belongs is not a document.
        fs::create_dir_all(dist.join(INDEX_DOCUMENT)).expect("index dir");
        assert_eq!(frontend_dist(directory.path()), None);
    }

    #[test]
    fn the_resolved_directory_is_inside_the_named_package() {
        let directory = tempfile::tempdir().expect("temp dir");
        let dist = fake_install(directory.path());
        let resolved = frontend_dist(directory.path()).expect("dist");
        assert!(resolved.starts_with(directory.path()));
        assert_eq!(resolved, dist);
    }
}
