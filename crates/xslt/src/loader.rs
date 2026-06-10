//! Stylesheet / document loader — pluggable resolver used by
//! `xsl:import`, `xsl:include`, and the `document()` XPath
//! function.
//!
//! The [`Loader`] trait is the abstraction; we ship a
//! [`FilesystemLoader`] for the common case (resolve hrefs as
//! local file paths relative to a base directory) and an
//! [`InMemoryLoader`] for testing (fixed in-memory map).
//!
//! Callers that need network resolution, security policies, or
//! catalog-based lookup can implement [`Loader`] themselves and
//! pass it to `Stylesheet::compile_str_with_loader`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::XsltError;

/// Resolve a stylesheet / document reference to its source text.
///
/// `href` is the user-supplied URI from the stylesheet (e.g. the
/// `href=` attribute of `xsl:import`).  `base` is the URI of the
/// containing stylesheet — used to resolve relative hrefs.  Both
/// follow XML URI handling conventions (RFC 3986 reference
/// resolution); we keep them as opaque strings here so loaders
/// can choose their own scheme.
pub trait Loader {
    /// Load `href`'s contents.  Returns the raw source as a UTF-8
    /// string.  Implementations should resolve `href` against
    /// `base` when `href` is relative.
    fn load(&self, href: &str, base: Option<&str>) -> Result<String, XsltError>;

    /// Load `href`'s contents and return them as a parsed
    /// [`sup_xml_tree::dom::Document`].  Default implementation
    /// calls [`Self::load`] and parses fresh each time; loaders
    /// that expect repeated lookups for the same URI (filesystem,
    /// in-memory) should override to cache the parse result
    /// internally so the runtime `doc()` path doesn't re-parse
    /// the same XML thousands of times across an apply sweep.
    ///
    /// Returns an `Arc` so cache hits share the underlying
    /// document without cloning the arena.
    fn load_parsed(
        &self, href: &str, base: Option<&str>,
    ) -> Result<std::sync::Arc<sup_xml_tree::dom::Document>, XsltError> {
        let text = self.load(href, base)?;
        let opts = sup_xml_core::ParseOptions {
            namespace_aware: true,
            ..Default::default()
        };
        let doc = sup_xml_core::parse_str(&text, &opts).map_err(XsltError::from)?;
        Ok(std::sync::Arc::new(doc))
    }

    /// Compute the base URI to use for files imported FROM the
    /// resource at `href`.  Default: returns `href` itself
    /// (used as the base for the next level of resolution).
    fn resolve(&self, href: &str, base: Option<&str>) -> Result<String, XsltError> {
        let _ = base;
        Ok(href.to_string())
    }

    /// Enumerate every URI the loader can serve.  Returned as
    /// hrefs that, when passed to [`Self::load`] with the same
    /// `base`, succeed.  Called by the XSLT runtime when the
    /// stylesheet uses a dynamic `document()` / `doc()` URI
    /// (computed via concat / variables / @attr) — every
    /// enumerated URI is speculatively pre-loaded so that the
    /// runtime call can resolve against the pre-loaded map.
    ///
    /// Default: empty.  Loaders that can't safely enumerate
    /// (HTTP / arbitrary URI schemes / sandboxed environments)
    /// leave it unimplemented, in which case stylesheets that
    /// build URIs dynamically fall back to "URI not pre-loaded"
    /// errors at runtime for any URI the static analysis missed.
    ///
    /// Implementations should respect the same security boundary
    /// as `load` — e.g. `FilesystemLoader::enumerate` lists files
    /// under `allowed_roots` only.
    fn enumerate(&self, base: Option<&str>) -> Vec<String> {
        let _ = base;
        Vec::new()
    }
}

/// Loader that resolves hrefs as filesystem paths.  Relative
/// `href`s are joined to the directory of the parent `base` (if
/// supplied), or to the current working directory otherwise.
///
/// `allowed_roots` is the **security boundary**.  A load is
/// refused unless the resolved (and canonicalised) target path
/// lies inside one of the listed directories.  Empty list ⇒
/// every load is refused.  Matches the policy of the core crate's
/// `FilesystemResolver` so untrusted stylesheets can't
/// `<xsl:import href="/etc/passwd"/>` their way to local files.
/// Strip a leading `file:` scheme so a `file:` URI can be treated as
/// a local filesystem path.  Handles both the `file:///abs/path`
/// (empty authority) and bare `file:/abs/path` spellings; non-`file:`
/// strings (plain paths, other schemes) pass through unchanged.
fn strip_file_scheme(s: &str) -> &str {
    s.strip_prefix("file://")
        .or_else(|| s.strip_prefix("file:"))
        .unwrap_or(s)
}

#[derive(Debug, Default)]
pub struct FilesystemLoader {
    allowed_roots: Vec<PathBuf>,
    /// Cache of canonical-path → parsed Document for repeated
    /// `load_parsed` lookups.  Populated lazily on first hit.
    /// Mutex because XSLT applies can run on any thread the
    /// caller picks (the cache itself is a private impl detail).
    /// Per-loader scope, so dropping the loader frees the cache.
    parsed_cache: std::sync::Mutex<
        std::collections::HashMap<String, std::sync::Arc<sup_xml_tree::dom::Document>>,
    >,
}

impl FilesystemLoader {
    /// Construct a loader scoped to the given root directories.
    /// Files outside these roots — even when the href is absolute
    /// and points at them directly — will be refused.
    ///
    /// Pass specific directories like the stylesheet's own folder
    /// or `/usr/share/xml`; never pass `/`.  An empty list means
    /// "refuse everything," which is the safest choice for an
    /// untrusted stylesheet that's not expected to import.
    pub fn new(allowed_roots: Vec<PathBuf>) -> Self {
        Self {
            allowed_roots,
            parsed_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn resolve_path(&self, href: &str, base: Option<&str>) -> PathBuf {
        let href = strip_file_scheme(href);
        let href_path = Path::new(href);
        if href_path.is_absolute() {
            return href_path.to_path_buf();
        }
        match base {
            Some(base) => {
                let base = strip_file_scheme(base);
                let base_path = Path::new(base);
                // Strip the filename component if `base` looks like
                // a file (has an extension); otherwise treat as
                // directory.
                let base_dir = if base_path.extension().is_some() {
                    base_path.parent().unwrap_or(Path::new("."))
                } else {
                    base_path
                };
                base_dir.join(href)
            }
            None => href_path.to_path_buf(),
        }
    }

    /// `true` if `path` is under one of the allowed roots.
    /// Symlinks are resolved before the check so that a symlink
    /// inside an allowed root pointing outside can't escape it.
    /// Missing or unreadable paths refuse (returns `false`).
    fn is_within_allowed_root(&self, path: &Path) -> bool {
        let canonical = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => return false,
        };
        self.allowed_roots.iter().any(|root| {
            match root.canonicalize() {
                Ok(canon_root) => canonical.starts_with(&canon_root),
                Err(_) => false,
            }
        })
    }
}

impl Loader for FilesystemLoader {
    fn load(&self, href: &str, base: Option<&str>) -> Result<String, XsltError> {
        let path = self.resolve_path(href, base);
        if !self.is_within_allowed_root(&path) {
            return Err(XsltError::InvalidStylesheet(format!(
                "refusing to load '{href}' (resolved to '{}'): \
                 path is not within the loader's allowed roots",
                path.display()
            )));
        }
        std::fs::read_to_string(&path).map_err(|e| XsltError::InvalidStylesheet(
            format!("failed to load '{href}' (resolved to '{}'): {e}", path.display()),
        ))
    }

    fn resolve(&self, href: &str, base: Option<&str>) -> Result<String, XsltError> {
        let path = self.resolve_path(href, base);
        let resolved = path.to_string_lossy().into_owned();
        // The result is handed back as a URI base for nested imports, so it
        // must be '/'-separated like the hrefs callers pass.  `PathBuf`
        // renders with `\` on Windows; normalise it.  (`load` uses the
        // native `PathBuf` directly for the filesystem read, where the
        // separator doesn't matter.)  Only on Windows, since `\` is a legal
        // filename byte on Unix.
        #[cfg(windows)]
        let resolved = resolved.replace('\\', "/");
        Ok(resolved)
    }

    /// Cached parse for repeated lookups of the same on-disk file.
    /// Keys on the canonical path so `foo/../foo.xml` and
    /// `foo.xml` (resolved against the same base) share a cache
    /// entry.  Misses fall through to `load` + `parse_str` then
    /// populate the cache.
    fn load_parsed(
        &self, href: &str, base: Option<&str>,
    ) -> Result<std::sync::Arc<sup_xml_tree::dom::Document>, XsltError> {
        let path = self.resolve_path(href, base);
        let key = path.canonicalize()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned());
        if let Some(hit) = self.parsed_cache.lock().unwrap().get(&key).cloned() {
            return Ok(hit);
        }
        let text = self.load(href, base)?;
        let opts = sup_xml_core::ParseOptions {
            namespace_aware: true, ..Default::default()
        };
        let doc = sup_xml_core::parse_str(&text, &opts).map_err(XsltError::from)?;
        let arc = std::sync::Arc::new(doc);
        self.parsed_cache.lock().unwrap().insert(key, arc.clone());
        Ok(arc)
    }

    /// Walk every file under the loader's allowed roots, returning
    /// hrefs relative to `base`'s parent directory.  Only files
    /// whose extension is a typical XML / text resource are
    /// returned, to keep speculative pre-loading from chewing
    /// through every binary the test directory happens to
    /// contain.  Bounded depth (16) so a stray symlink loop can't
    /// hang the runtime.
    fn enumerate(&self, base: Option<&str>) -> Vec<String> {
        let base_dir = match base
            .and_then(|b| Path::new(b).parent().map(|p| p.to_path_buf()))
        {
            Some(d) => d,
            None => return Vec::new(),
        };
        let canon_base = match base_dir.canonicalize() {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };
        // Only walk within an allowed root.
        if !self.allowed_roots.iter().any(|r|
            r.canonicalize().map(|cr| canon_base.starts_with(&cr)).unwrap_or(false))
        {
            return Vec::new();
        }
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>, depth: u32) {
            if depth > 16 { return; }
            let read = match std::fs::read_dir(dir) {
                Ok(r) => r, Err(_) => return,
            };
            for entry in read.flatten() {
                let p = entry.path();
                let ty = match entry.file_type() { Ok(t) => t, Err(_) => continue };
                if ty.is_dir() {
                    walk(&p, base, out, depth + 1);
                } else if ty.is_file() {
                    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if matches!(ext.to_ascii_lowercase().as_str(),
                                "xml" | "xsl" | "xslt" | "txt") {
                        if let Ok(rel) = p.strip_prefix(base) {
                            out.push(rel.to_string_lossy().into_owned());
                        }
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&canon_base, &canon_base, &mut out, 0);
        out
    }
}

/// In-memory loader keyed by href.  Useful for tests and for
/// embedding the ISO Schematron pipeline (where the skeleton
/// stylesheets are bundled into the binary rather than loaded
/// from disk).
#[derive(Debug, Default, Clone)]
pub struct InMemoryLoader {
    map: HashMap<String, String>,
}

impl InMemoryLoader {
    pub fn new() -> Self { Self { map: HashMap::new() } }

    /// Register `text` as the contents of `href`.  Returns `self`
    /// for chained insertion.
    pub fn with(mut self, href: impl Into<String>, text: impl Into<String>) -> Self {
        self.map.insert(href.into(), text.into());
        self
    }

    /// Mutable insertion — same as `.with(...)` but in-place.
    pub fn insert(&mut self, href: impl Into<String>, text: impl Into<String>) {
        self.map.insert(href.into(), text.into());
    }
}

impl Loader for InMemoryLoader {
    fn load(&self, href: &str, _base: Option<&str>) -> Result<String, XsltError> {
        self.map.get(href).cloned().ok_or_else(|| XsltError::InvalidStylesheet(
            format!("InMemoryLoader: no entry for '{href}'"),
        ))
    }
}

/// Null loader — every load fails.  Used as the default when no
/// loader was supplied and the stylesheet doesn't actually need
/// resolution (i.e. has no xsl:import / xsl:include).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullLoader;

impl Loader for NullLoader {
    fn load(&self, href: &str, _base: Option<&str>) -> Result<String, XsltError> {
        Err(XsltError::InvalidStylesheet(format!(
            "no Loader was supplied; cannot resolve '{href}'.  Pass a Loader \
             to Stylesheet::compile_str_with_loader (or use FilesystemLoader / \
             InMemoryLoader)"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_loader_returns_registered_content() {
        let l = InMemoryLoader::new().with("foo.xsl", "<stylesheet/>");
        assert_eq!(l.load("foo.xsl", None).unwrap(), "<stylesheet/>");
    }

    #[test]
    fn in_memory_loader_errors_on_missing() {
        let l = InMemoryLoader::new();
        assert!(l.load("missing", None).is_err());
    }

    #[test]
    fn null_loader_always_errors() {
        assert!(NullLoader.load("anything", None).is_err());
    }

    #[test]
    fn filesystem_loader_resolves_relative_to_base() {
        let l = FilesystemLoader::new(Vec::new());
        let p = l.resolve_path("child.xsl", Some("/abs/parent.xsl"));
        assert_eq!(p, PathBuf::from("/abs/child.xsl"));
    }

    #[test]
    fn filesystem_loader_treats_extensionless_base_as_directory() {
        // base without an extension → treat as directory, join href directly.
        let l = FilesystemLoader::new(Vec::new());
        let p = l.resolve_path("file.xsl", Some("/abs/subdir"));
        assert_eq!(p, PathBuf::from("/abs/subdir/file.xsl"));
    }

    #[test]
    fn filesystem_loader_absolute_href_ignores_base() {
        let l = FilesystemLoader::new(Vec::new());
        let p = l.resolve_path("/etc/host.xsl", Some("/somewhere/else.xsl"));
        assert_eq!(p, PathBuf::from("/etc/host.xsl"));
    }

    #[test]
    fn filesystem_loader_no_base_uses_href_path_directly() {
        let l = FilesystemLoader::new(Vec::new());
        let p = l.resolve_path("foo.xsl", None);
        assert_eq!(p, PathBuf::from("foo.xsl"));
    }

    #[test]
    fn filesystem_loader_load_missing_file_errors() {
        // With an empty allowlist, every load refuses up-front
        // (matches `FilesystemResolver`'s policy in core).  We
        // still get an `InvalidStylesheet` error — the consumer-
        // visible contract is "load failed" — but the specific
        // message identifies the refusal, not an OS-level miss.
        let l = FilesystemLoader::new(Vec::new());
        let r = l.load("/nonexistent/definitely-not-here.xsl", None);
        assert!(r.is_err());
        match r {
            Err(XsltError::InvalidStylesheet(_)) => {}
            other => panic!("expected InvalidStylesheet, got {other:?}"),
        }
    }

    #[test]
    fn filesystem_loader_resolve_returns_resolved_path() {
        let l = FilesystemLoader::new(Vec::new());
        let s = l.resolve("child.xsl", Some("/abs/parent.xsl")).unwrap();
        assert_eq!(s, "/abs/child.xsl");
    }

    /// Security regression: a `FilesystemLoader` scoped to a
    /// specific directory must refuse to read files outside that
    /// directory, even when the stylesheet hands it an absolute
    /// `href`.  Without an allowlist, `<xsl:import href="/etc/
    /// passwd"/>` on an untrusted stylesheet exfiltrates local
    /// files — same threat model as the core FilesystemResolver.
    #[test]
    fn filesystem_loader_refuses_load_outside_allowed_roots() {
        use std::io::Write;
        // Set up an "outside" file and an allowed temp dir.
        let outside = std::env::temp_dir()
            .join(format!("sup-xml-xslt-outside-{}.xsl", std::process::id()));
        {
            let mut f = std::fs::File::create(&outside).unwrap();
            f.write_all(b"<stylesheet xmlns='http://www.w3.org/1999/XSL/Transform' version='1.0'/>").unwrap();
        }
        let allowed_dir = std::env::temp_dir()
            .join(format!("sup-xml-xslt-allowed-{}", std::process::id()));
        std::fs::create_dir_all(&allowed_dir).unwrap();
        // Loader scoped to allowed_dir only.
        let l = FilesystemLoader::new(vec![allowed_dir.clone()]);
        let r = l.load(outside.to_str().unwrap(), None);
        // Cleanup before asserting.
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&allowed_dir);
        assert!(
            r.is_err(),
            "FilesystemLoader should have refused load outside allowed roots, got Ok"
        );
        match r {
            Err(XsltError::InvalidStylesheet(msg)) => {
                assert!(
                    msg.to_lowercase().contains("allowed"),
                    "expected error mentioning allowed roots, got: {msg}"
                );
            }
            other => panic!("expected InvalidStylesheet, got {other:?}"),
        }
    }

    /// Counterpart: a file inside an allowed root loads normally.
    /// Locks in that the allowlist isn't just a blanket refuse.
    #[test]
    fn filesystem_loader_loads_within_allowed_root() {
        use std::io::Write;
        let allowed_dir = std::env::temp_dir()
            .join(format!("sup-xml-xslt-inside-{}", std::process::id()));
        std::fs::create_dir_all(&allowed_dir).unwrap();
        let inside = allowed_dir.join("ok.xsl");
        {
            let mut f = std::fs::File::create(&inside).unwrap();
            f.write_all(b"<stylesheet xmlns='http://www.w3.org/1999/XSL/Transform' version='1.0'/>").unwrap();
        }
        let l = FilesystemLoader::new(vec![allowed_dir.clone()]);
        let r = l.load(inside.to_str().unwrap(), None);
        let _ = std::fs::remove_file(&inside);
        let _ = std::fs::remove_dir_all(&allowed_dir);
        assert!(r.is_ok(), "load inside allowed root should succeed: {r:?}");
    }

    #[test]
    fn in_memory_loader_default_resolve_returns_href_unchanged() {
        // The Loader trait's default resolve() impl is what's tested here.
        let l = InMemoryLoader::new();
        let s = l.resolve("foo.xsl", Some("base.xsl")).unwrap();
        assert_eq!(s, "foo.xsl");
    }

    #[test]
    fn null_loader_default_resolve_returns_href_unchanged() {
        let s = NullLoader.resolve("foo.xsl", None).unwrap();
        assert_eq!(s, "foo.xsl");
    }

    #[test]
    fn in_memory_loader_insert_method() {
        let mut l = InMemoryLoader::new();
        l.insert("a.xsl", "<a/>");
        l.insert("b.xsl", "<b/>");
        assert_eq!(l.load("a.xsl", None).unwrap(), "<a/>");
        assert_eq!(l.load("b.xsl", None).unwrap(), "<b/>");
    }
}
