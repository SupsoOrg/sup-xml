//! Pluggable schema resolution for `<xs:import>` / `<xs:include>` /
//! `<xs:redefine>`.
//!
//! When the parser hits one of these directives, it asks the configured
//! [`SchemaResolver`] to fetch the referenced schema bytes.  The default
//! resolver looks for files relative to a base directory; users with
//! catalogs, network sources, or embedded schemas plug in their own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Resolves a schema-location hint to the XSD bytes for that schema.
///
/// Two conventions matter for implementations:
///
/// * Returning `Ok(None)` declines the resolution (the parser then
///   produces a clear "could not resolve" compile error).
/// * Returning `Err(_)` propagates an I/O failure; the parser wraps it
///   in a [`SchemaCompileError`](super::error::SchemaCompileError).
///
/// Implementations should be cheap to clone — the parser recursively
/// invokes them while processing nested includes.
///
/// The resolver is consumed synchronously by
/// [`Schema::compile_with`](super::Schema::compile_with) and never
/// stored or sent across threads, so no `Send`/`Sync` bound is
/// required — letting a resolver borrow a non-`Sync` loader for the
/// duration of one compile.
pub trait SchemaResolver {
    fn resolve(
        &self,
        location: &str,
        target_namespace: Option<&str>,
    ) -> Result<Option<Vec<u8>>, std::io::Error>;
}

/// A resolver that always declines.  Used by
/// [`Schema::compile_str`](super::Schema::compile_str) for single-file
/// schemas — any `<xs:import>` or `<xs:include>` becomes a compile error.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoResolver;

impl SchemaResolver for NoResolver {
    fn resolve(&self, _location: &str, _target_ns: Option<&str>)
        -> Result<Option<Vec<u8>>, std::io::Error>
    {
        Ok(None)
    }
}

/// Look for schema files on the filesystem relative to a base directory.
///
/// Refuses to traverse upward past the base (any `..` that would escape
/// is rejected) — schemas are an attack-surface input, and we don't want
/// `<xs:import schemaLocation="../../../etc/passwd"/>` to read arbitrary
/// files.  Absolute paths are likewise rejected.
#[derive(Debug, Clone)]
pub struct FsResolver {
    base_dir: PathBuf,
}

impl FsResolver {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self { base_dir: base_dir.into() }
    }
}

impl SchemaResolver for FsResolver {
    fn resolve(&self, location: &str, _target_ns: Option<&str>)
        -> Result<Option<Vec<u8>>, std::io::Error>
    {
        let p = Path::new(location);
        if p.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("absolute schemaLocation rejected: {location:?}"),
            ));
        }
        // Resolve and canonicalise to verify it stays under base_dir.
        let candidate = self.base_dir.join(p);
        // Walk components; reject any `..` that would escape `base_dir`.
        // We don't use canonicalize() because it requires the file to
        // exist *and* dereferences symlinks (which can themselves
        // traverse).  Stick to syntactic containment.
        let base_components: Vec<_> = self.base_dir.components().collect();
        let cand_components: Vec<_> = candidate.components().collect();
        if cand_components.len() < base_components.len()
            || cand_components[..base_components.len()] != base_components[..]
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("schemaLocation escapes base directory: {location:?}"),
            ));
        }
        // Reject parent traversal anywhere in the tail.
        let tail = &cand_components[base_components.len()..];
        if tail.iter().any(|c| matches!(c, std::path::Component::ParentDir)) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("schemaLocation contains '..': {location:?}"),
            ));
        }
        match std::fs::read(&candidate) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Map literal `schemaLocation` strings to schema bytes held in memory.
/// Useful for embedding schemas in a binary, for tests, or for proxying
/// to an in-process schema cache.
#[derive(Debug, Default, Clone)]
pub struct InMemoryResolver {
    map: HashMap<String, Vec<u8>>,
}

impl InMemoryResolver {
    pub fn new() -> Self { Self::default() }

    pub fn insert(&mut self, location: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        self.map.insert(location.into(), bytes.into());
    }

    /// Builder-style — same as [`insert`] but consumes/returns `self`
    /// so resolvers can be constructed fluently.
    pub fn with(mut self, location: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        self.insert(location, bytes);
        self
    }
}

impl SchemaResolver for InMemoryResolver {
    fn resolve(&self, location: &str, _target_ns: Option<&str>)
        -> Result<Option<Vec<u8>>, std::io::Error>
    {
        Ok(self.map.get(location).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_resolver_always_declines() {
        let r = NoResolver;
        assert!(r.resolve("anything.xsd", None).unwrap().is_none());
    }

    #[test]
    fn in_memory_resolves_known_locations() {
        let r = InMemoryResolver::new()
            .with("a.xsd", b"<a/>".to_vec())
            .with("b.xsd", b"<b/>".to_vec());
        assert_eq!(r.resolve("a.xsd", None).unwrap().as_deref(), Some(b"<a/>".as_slice()));
        assert!(r.resolve("c.xsd", None).unwrap().is_none());
    }

    #[test]
    fn fs_resolver_rejects_absolute() {
        let r = FsResolver::new("/tmp");
        let err = r.resolve("/etc/passwd", None).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn fs_resolver_rejects_parent_traversal() {
        let r = FsResolver::new("/tmp/schemas");
        let err = r.resolve("../etc/passwd", None).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }
}
