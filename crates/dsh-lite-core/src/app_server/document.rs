//! Static document lookup for the packaged Web frontend.
//!
//! The rules mirror `apps/desktop/src/web-document.ts`: only the index document
//! is rewritten (the boot script has to exist before the frontend module runs),
//! the media type comes from a fixed table, and every other path is resolved
//! inside the packaged dist directory with `..` rejected. Anything else is not a
//! document and belongs to the Host proxy.

use std::io;
use std::path::{Path, PathBuf};

/// Script the official shell injects right after `<head>`.
///
/// The frontend awaits this promise before its boot handshake, so the shell has
/// to publish the resolver before the module is evaluated.
pub const BOOT_SCRIPT: &str =
    "<script>globalThis.__DSH_BOOT_READY__ = Promise.withResolvers()</script>";

/// Prefix under which shell-owned pages (`loading.html`, `error.html`, …) are
/// served from the injected asset resolver instead of the Host.
const SHELL_PREFIX: &str = "/_shell/";

/// How a request path is answered without reaching the Host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StaticRoute {
    /// `dist/index.html`, with the boot script injected.
    Index,
    /// A file below the packaged dist directory.
    Asset,
    /// A shell-owned page resolved by the injected callback.
    Shell,
}

/// Route for `pathname`, or `None` when the Host owns the path.
///
/// The check runs on the encoded path exactly like the official scheme handler,
/// which tests `url.pathname` before `serveWebDocument` decodes it.
pub(crate) fn route(pathname: &str) -> Option<StaticRoute> {
    match pathname {
        "/" | "/index.html" => Some(StaticRoute::Index),
        "/favicon.svg" | "/favicon-dark.svg" | "/manifest.webmanifest" => Some(StaticRoute::Asset),
        path if path.starts_with("/assets/") => Some(StaticRoute::Asset),
        path if path.starts_with(SHELL_PREFIX) => Some(StaticRoute::Shell),
        _ => None,
    }
}

/// A response body ready to be sent.
pub(crate) struct StaticFile {
    /// Media type for the `content-type` header.
    pub content_type: String,
    /// Complete, already-injected response body.
    pub body: Vec<u8>,
}

/// Reads `pathname` from the dist directory; `Err` is the status to answer with.
pub(crate) async fn read_dist(
    root: &Path,
    pathname: &str,
    boot_script: &str,
) -> Result<StaticFile, u16> {
    let decoded = percent_decode(pathname).ok_or(400u16)?;
    let index = matches!(pathname, "/" | "/index.html");
    let relative = if index {
        "index.html".to_owned()
    } else {
        decoded
    };
    let target = contained_path(root, &relative).ok_or(403u16)?;
    let content = tokio::fs::read(&target)
        .await
        .map_err(|error| status_for(&error))?;
    let content_type = content_type(&target).to_owned();
    let body = if index {
        inject_boot_script(&content, boot_script)
    } else {
        content
    };
    Ok(StaticFile { content_type, body })
}

/// Resolves a shell-owned page through the injected asset callback.
pub(crate) fn read_shell(
    assets: Option<&dyn crate::app_server::ShellAssets>,
    pathname: &str,
) -> Result<StaticFile, u16> {
    let relative = pathname.strip_prefix(SHELL_PREFIX).ok_or(404u16)?;
    let relative = percent_decode(relative).ok_or(400u16)?;
    if relative.is_empty() || relative.contains('/') {
        // Shell pages are a flat set of documents, not a directory tree.
        return Err(404);
    }
    let asset = assets
        .and_then(|assets| assets.resolve(&relative))
        .ok_or(404u16)?;
    if !(200..=299).contains(&asset.status) {
        return Err(asset.status);
    }
    Ok(StaticFile {
        content_type: asset.content_type,
        body: asset.body,
    })
}

/// Status for a failed file read: a missing file is a 404, anything else is the
/// shell's problem and reads as a 500 like the official rethrow.
fn status_for(error: &io::Error) -> u16 {
    if error.kind() == io::ErrorKind::NotFound {
        404
    } else {
        500
    }
}

/// Media type from the extension table of `web-document.ts`.
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// Inserts `boot_script` after the first `<head>` tag.
///
/// This is `body.toString().replace('<head>', '<head>' + BOOT)`; an index
/// without a literal `<head>` is served unchanged rather than failing boot.
fn inject_boot_script(content: &[u8], boot_script: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(content);
    text.replacen("<head>", &format!("<head>{boot_script}"), 1)
        .into_bytes()
}

/// Resolves `decoded` below `root` and rejects everything outside it.
///
/// The official comparison is `target.startsWith(root + sep)`, so the root
/// directory itself is not servable either.
fn contained_path(root: &Path, decoded: &str) -> Option<PathBuf> {
    let target = lexical_resolve(root, decoded);
    match target.strip_prefix(root) {
        Ok(remainder) if !remainder.as_os_str().is_empty() => Some(target),
        _ => None,
    }
}

/// Normalizes `decoded` against `root` the way Node's `path.resolve` does.
///
/// `..` pops one component and clamps at the filesystem root, while empty and
/// `.` segments disappear.
fn lexical_resolve(root: &Path, decoded: &str) -> PathBuf {
    let mut resolved = root.to_path_buf();
    for segment in decoded.split(SEPARATORS) {
        match segment {
            "" | "." => {}
            ".." => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

/// Path separators accepted while resolving a document path.
#[cfg(windows)]
const SEPARATORS: [char; 2] = ['/', '\\'];

/// Path separators accepted while resolving a document path.
#[cfg(not(windows))]
const SEPARATORS: [char; 1] = ['/'];

/// Decodes percent escapes like `decodeURIComponent`.
///
/// Malformed escapes, truncated escapes and sequences that are not valid UTF-8
/// are all rejected, which is what makes the official server answer 400 instead
/// of reading an unexpected file.
pub(crate) fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_value(*bytes.get(index + 1)?)?;
            let low = hex_value(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Value of one hexadecimal digit.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_routes_match_the_official_handler() {
        assert_eq!(route("/"), Some(StaticRoute::Index));
        assert_eq!(route("/index.html"), Some(StaticRoute::Index));
        assert_eq!(route("/assets/index-abc.js"), Some(StaticRoute::Asset));
        assert_eq!(route("/favicon.svg"), Some(StaticRoute::Asset));
        assert_eq!(route("/favicon-dark.svg"), Some(StaticRoute::Asset));
        assert_eq!(route("/manifest.webmanifest"), Some(StaticRoute::Asset));
        assert_eq!(route("/_shell/loading.html"), Some(StaticRoute::Shell));
        assert_eq!(route("/api/sessions"), None);
        assert_eq!(route("/assets"), None);
        assert_eq!(route("/favicon.ico"), None);
    }

    #[test]
    fn index_injection_runs_once_after_the_head_tag() {
        let page = "<!doctype html><html><head><meta charset=\"utf-8\"></head></html>";
        let injected = String::from_utf8(inject_boot_script(page.as_bytes(), BOOT_SCRIPT))
            .expect("utf-8 body");
        assert!(injected.contains(&format!("<head>{BOOT_SCRIPT}")));
        assert_eq!(injected.matches("<head>").count(), 1);
        assert_eq!(injected.matches(BOOT_SCRIPT).count(), 1);
    }

    #[test]
    fn index_without_head_is_served_unchanged() {
        let page = "<html><body>no head</body></html>";
        assert_eq!(
            inject_boot_script(page.as_bytes(), BOOT_SCRIPT),
            page.as_bytes()
        );
    }

    #[test]
    fn mime_table_matches_the_official_one() {
        for (name, expected) in [
            ("index.html", "text/html; charset=utf-8"),
            ("main.js", "text/javascript; charset=utf-8"),
            ("style.css", "text/css; charset=utf-8"),
            ("logo.svg", "image/svg+xml"),
            ("manifest.webmanifest", "application/octet-stream"),
            ("data.json", "application/json"),
            ("font.woff2", "font/woff2"),
            ("icon.png", "image/png"),
            ("favicon.ico", "image/x-icon"),
            ("archive.zip", "application/octet-stream"),
            ("UPPER.JS", "application/octet-stream"),
        ] {
            assert_eq!(content_type(Path::new(name)), expected, "for {name}");
        }
    }

    #[test]
    fn percent_decoding_rejects_malformed_escapes() {
        assert_eq!(percent_decode("/a%20b").as_deref(), Some("/a b"));
        assert_eq!(percent_decode("/%E4%BD%A0").as_deref(), Some("/你"));
        assert_eq!(percent_decode("/a%"), None);
        assert_eq!(percent_decode("/a%2"), None);
        assert_eq!(percent_decode("/a%zz"), None);
        // Lone continuation byte: not valid UTF-8.
        assert_eq!(percent_decode("/%80"), None);
    }

    #[test]
    fn traversal_never_escapes_the_dist_directory() {
        let root = Path::new("/tmp/dist");
        assert_eq!(
            contained_path(root, "/assets/app.js"),
            Some(PathBuf::from("/tmp/dist/assets/app.js"))
        );
        assert_eq!(
            contained_path(root, "/assets/../index.html"),
            Some(PathBuf::from("/tmp/dist/index.html"))
        );
        assert_eq!(contained_path(root, "/../secret"), None);
        assert_eq!(contained_path(root, "/../../etc/passwd"), None);
        assert_eq!(contained_path(root, "/assets/../../secret"), None);
        // The root itself is not servable: the official check requires a separator.
        assert_eq!(contained_path(root, "/"), None);
        assert_eq!(contained_path(root, "/.."), None);
    }

    #[test]
    fn decoded_escapes_cannot_smuggle_traversal() {
        let root = Path::new("/tmp/dist");
        let decoded = percent_decode("/..%2f..%2fetc%2fpasswd").expect("decodes");
        assert_eq!(decoded, "/../../etc/passwd");
        assert_eq!(contained_path(root, &decoded), None);
    }
}
