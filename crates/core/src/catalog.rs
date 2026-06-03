#![forbid(unsafe_code)]  // see CONTRIBUTING.md § "Unsafe policy"

//! OASIS XML Catalogs — local mappings from public/system identifiers
//! to filesystem paths.  See § "XML Catalogs" in `COMPARISON.md` for
//! the rationale.
//!
//! # What this implements
//!
//! Entry types from the OASIS XML Catalog spec § 6:
//!
//! - `<public publicId="…" uri="…"/>`           — PUBLIC-id → URI
//! - `<system systemId="…" uri="…"/>`           — SYSTEM-id → URI
//! - `<uri name="…" uri="…"/>`                  — generic URI alias
//! - `<rewriteSystem systemIdStartString="…" rewritePrefix="…"/>`
//! - `<rewriteUri uriStartString="…" rewritePrefix="…"/>`
//! - `<delegatePublic publicIdStartString="…" catalog="…"/>`
//! - `<delegateSystem systemIdStartString="…" catalog="…"/>`
//! - `<delegateURI uriStartString="…" catalog="…"/>`
//! - `<nextCatalog catalog="…"/>`               — chain to another file
//! - `<group prefer="…">…</group>`              — scoped prefer override
//!
//! Discovery via the `XML_CATALOG_FILES` environment variable
//! (libxml2-compatible) plus a built-in conventional-path list is
//! provided through [`load_default`] and [`discover_catalog_paths`].
//!
//! Resolution follows OASIS § 7: exact matches before prefix
//! rewrites before delegation before catalog chaining; longest
//! matching prefix wins for rewrite / delegate entries.  Cycles
//! between `<nextCatalog>` and `<delegate*>` references are broken
//! by a per-resolution "already visited" set.
//!
//! # Public API
//!
//! [`Catalog::resolve(public_id, system_id)`] is the one entry
//! point most callers need.  It returns `Option<String>` because
//! rewrite entries synthesise a new URI; non-rewrite hits clone
//! the stored mapping value.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::{ErrorDomain, ErrorLevel, Result, XmlError};
use crate::xml_bytes_reader::{BytesEvent, XmlBytesReader};

/// The `prefer` attribute on `<catalog>` / `<group>` — controls
/// whether PUBLIC or SYSTEM identifiers take precedence in
/// resolution.  Defaults to `Public` per OASIS § 7.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prefer {
    Public,
    System,
}

impl Default for Prefer {
    fn default() -> Self { Self::Public }
}

/// One entry in a parsed catalog.  Entries are stored in document
/// order so prefix-rewrite resolution can compare candidates by
/// length (OASIS § 7.2.2: longest matching prefix wins, ties broken
/// by declaration order — first wins).
#[derive(Debug, Clone)]
enum Entry {
    /// `<public publicId="…" uri="…"/>` — `prefer` carried from the
    /// enclosing `<group>` / `<catalog>` so resolution honours the
    /// scope-local override.
    Public  { id: String, uri: String, prefer: Prefer },
    /// `<system systemId="…" uri="…"/>`.
    System  { id: String, uri: String },
    /// `<uri name="…" uri="…"/>` — generic URI alias.
    Uri     { name: String, uri: String },
    /// `<rewriteSystem systemIdStartString="…" rewritePrefix="…"/>`.
    RewriteSystem { start: String, replace: String },
    /// `<rewriteUri uriStartString="…" rewritePrefix="…"/>`.
    RewriteUri    { start: String, replace: String },
    /// `<delegatePublic publicIdStartString="…" catalog="…"/>` —
    /// `catalog` is the resolved sub-catalog (loaded eagerly at
    /// parse time so cycles are detected up front).  `None` means
    /// the referenced file didn't exist or didn't parse, which
    /// causes that delegation arm to be silently skipped at
    /// resolution time (matching libxml2's behaviour).
    DelegatePublic { start: String, sub: Option<Box<Catalog>> },
    /// `<delegateSystem systemIdStartString="…" catalog="…"/>`.
    DelegateSystem { start: String, sub: Option<Box<Catalog>> },
    /// `<delegateURI uriStartString="…" catalog="…"/>`.
    DelegateUri    { start: String, sub: Option<Box<Catalog>> },
    /// `<nextCatalog catalog="…"/>`.  Same semantics as delegation
    /// but unconditional — every `<nextCatalog>` is tried after the
    /// containing catalog's own entries are exhausted.
    NextCatalog    { sub: Option<Box<Catalog>> },
}

/// In-memory representation of one or more parsed XML catalog files.
///
/// Construct with [`load_default`] to use the libxml2-compatible
/// discovery rules, or with [`from_files`] / [`parse`] when you
/// know the catalog locations explicitly.
#[derive(Debug, Default, Clone)]
pub struct Catalog {
    /// Entries in document order.  Resolution semantics — longest
    /// prefix wins for rewrite/delegate entries — is enforced at
    /// [`resolve`](Self::resolve) time, not at construction.
    entries: Vec<Entry>,
    /// `prefer` attribute on the catalog root (`<catalog prefer="…">`).
    /// Inherited by entries declared outside any `<group>`.
    root_prefer: Prefer,
    /// Catalog files loaded into this instance, in load order.
    /// Diagnostic aid — answers "which file did this come from?".
    sources: Vec<PathBuf>,
}

impl Catalog {
    /// Look up a `(publicId, systemId)` pair against the catalog.
    /// Resolution follows OASIS § 7: SYSTEM exact → rewriteSystem
    /// → delegateSystem → PUBLIC exact (per `prefer`) →
    /// delegatePublic → nextCatalog chains.  Returns the catalog's
    /// URI for the entity, or `None` if no entry matched anywhere
    /// in the chain.
    ///
    /// The result is a [`Cow`] so direct `<public>` / `<system>`
    /// matches return a borrow into the catalog (zero allocation)
    /// while rewrite entries return the synthesised string owned.
    /// Callers that need to own the URI past the catalog's lifetime
    /// can call `.into_owned()`.
    pub fn resolve<'a>(
        &'a self,
        public_id: Option<&str>, system_id: Option<&str>,
    ) -> Option<Cow<'a, str>> {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        self.resolve_inner(public_id, system_id, &mut seen)
    }

    /// `<uri>` lookup — resolve a generic URI alias.  Distinct from
    /// `<system>`: the OASIS spec keeps the two namespaces
    /// separate.  Used by XInclude / XSLT consumers that want a
    /// catalog-style mapping without going through DTD's
    /// PUBLIC/SYSTEM mechanism.
    ///
    /// As with [`resolve`](Self::resolve), the result is a [`Cow`] —
    /// direct `<uri>` matches borrow; `<rewriteUri>` matches own.
    pub fn resolve_uri<'a>(&'a self, uri: &str) -> Option<Cow<'a, str>> {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        self.resolve_uri_inner(uri, &mut seen)
    }

    /// Number of entries in the catalog.  Counts every entry,
    /// including rewrite/delegate/next ones.  Used in tests and
    /// diagnostics.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if no entries were loaded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Files this catalog instance was loaded from, in order.
    pub fn sources(&self) -> &[PathBuf] {
        &self.sources
    }

    /// Build a catalog by merging the contents of multiple files.
    /// Later entries do NOT override earlier ones — first match
    /// wins, matching libxml2's behaviour for catalog chains.
    pub fn from_files<P: AsRef<Path>>(paths: &[P]) -> Result<Self> {
        let mut loading: HashSet<PathBuf> = HashSet::new();
        let mut out = Catalog::default();
        for path in paths {
            let path = path.as_ref();
            let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            if !loading.insert(canon.clone()) { continue; }
            let bytes = std::fs::read(path).map_err(|e| {
                XmlError::new(
                    ErrorDomain::Io,
                    ErrorLevel::Error,
                    format!("failed to read catalog file {}: {e}", path.display()),
                )
            })?;
            out.merge_from_bytes(&bytes, Some(path), &mut loading)?;
        }
        Ok(out)
    }

    /// Parse a catalog from raw bytes.  Useful for tests or when
    /// the catalog isn't on disk (embedded resources, etc.).
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut out = Catalog::default();
        let mut loading: HashSet<PathBuf> = HashSet::new();
        out.merge_from_bytes(bytes, None, &mut loading)?;
        Ok(out)
    }

    // ── resolution ──────────────────────────────────────────────

    fn resolve_inner<'a>(
        &'a self,
        public_id: Option<&str>, system_id: Option<&str>,
        seen: &mut HashSet<PathBuf>,
    ) -> Option<Cow<'a, str>> {
        // Entry types are consulted in OASIS catalog resolution order
        // (§7); the first match wins, so the sequence below is
        // precedence, not arbitrary.
        //
        // <system> exact match — borrow the stored URI.
        if let Some(sid) = system_id {
            for e in &self.entries {
                if let Entry::System { id, uri } = e {
                    if id == sid { return Some(Cow::Borrowed(uri.as_str())); }
                }
            }
        }
        // <rewriteSystem> — longest matching prefix wins.
        // The synthesised URI is owned (built via `format!`).
        if let Some(sid) = system_id {
            if let Some(rewritten) = longest_rewrite(&self.entries, sid, /*system*/ true) {
                return Some(Cow::Owned(rewritten));
            }
        }
        // <delegateSystem> — longest matching prefix
        // delegates to the sub-catalog; ties broken by declaration
        // order.  If the delegate's own lookup returns None, try
        // the next-longest delegate (OASIS § 7.2.4).  The returned
        // Cow borrows from the sub-catalog (owned by `self` via
        // `Box<Catalog>`) so its lifetime is `'a`.
        if let Some(sid) = system_id {
            if let Some(out) = longest_delegate(
                &self.entries, sid, /*kind*/ DelegateKind::System,
                public_id, system_id, seen,
            ) { return Some(out); }
        }
        // <public> exact match — only when PUBLIC was
        // supplied AND either the entry is not in a `prefer=system`
        // scope, or no SYSTEM identifier was supplied at all
        // (OASIS § 7.1.1).
        if let Some(pid) = public_id {
            let normalised = normalise_public_id(pid);
            for e in &self.entries {
                if let Entry::Public { id, uri, prefer } = e {
                    if id == &normalised
                        && (*prefer == Prefer::Public || system_id.is_none())
                    {
                        return Some(Cow::Borrowed(uri.as_str()));
                    }
                }
            }
        }
        // <delegatePublic> — same shape as delegateSystem.
        if let Some(pid) = public_id {
            let normalised = normalise_public_id(pid);
            if let Some(out) = longest_delegate(
                &self.entries, &normalised, /*kind*/ DelegateKind::Public,
                public_id, system_id, seen,
            ) { return Some(out); }
        }
        // <nextCatalog> chains — each one is consulted in
        // declaration order until something matches.
        for e in &self.entries {
            if let Entry::NextCatalog { sub: Some(sub) } = e {
                if !mark_seen(seen, sub) { continue; }
                if let Some(out) = sub.resolve_inner(public_id, system_id, seen) {
                    return Some(out);
                }
            }
        }
        None
    }

    fn resolve_uri_inner<'a>(&'a self, target: &str, seen: &mut HashSet<PathBuf>)
        -> Option<Cow<'a, str>>
    {
        // <uri> exact match → <rewriteUri> → <delegateURI> →
        // <nextCatalog>.  Mirrors the system-id pipeline but on the
        // uri-namespace.
        for e in &self.entries {
            if let Entry::Uri { name, uri } = e {
                if name == target { return Some(Cow::Borrowed(uri.as_str())); }
            }
        }
        if let Some(out) = longest_rewrite(&self.entries, target, /*system*/ false) {
            return Some(Cow::Owned(out));
        }
        if let Some(out) = longest_delegate(
            &self.entries, target, DelegateKind::Uri,
            /*public*/ None, /*system*/ None, seen,
        ) {
            return Some(out);
        }
        for e in &self.entries {
            if let Entry::NextCatalog { sub: Some(sub) } = e {
                if !mark_seen(seen, sub) { continue; }
                if let Some(out) = sub.resolve_uri_inner(target, seen) {
                    return Some(out);
                }
            }
        }
        None
    }

    // ── parsing ─────────────────────────────────────────────────

    /// Parse catalog bytes from `source` and merge entries into
    /// `self`.  `source` is the on-disk path the bytes came from
    /// (used to anchor relative `catalog="…"` references in
    /// `<nextCatalog>` / `<delegate*>`); `None` when parsing from
    /// memory.
    fn merge_from_bytes(
        &mut self, bytes: &[u8], source: Option<&Path>,
        loading: &mut HashSet<PathBuf>,
    ) -> Result<()> {
        if let Some(p) = source {
            self.sources.push(p.to_path_buf());
        }
        let base_dir: Option<PathBuf> = source
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));

        let mut reader = XmlBytesReader::from_bytes(bytes)?;
        // Stack of `prefer` values pushed by `<catalog>` and
        // `<group>` openings — the top of the stack is the
        // effective `prefer` for any entry seen at this depth.
        // We deliberately use a Vec rather than a single Cell so
        // nested groups (rare but legal) compose correctly.
        let mut prefer_stack: Vec<Prefer> = vec![self.root_prefer];

        loop {
            match reader.next()? {
                BytesEvent::Eof => break,
                BytesEvent::EndElement(tag) => {
                    let local = strip_namespace_prefix(tag.name());
                    if local == b"group" || local == b"catalog" {
                        prefer_stack.pop();
                    }
                }
                BytesEvent::StartElement(tag) => {
                    // Detach the borrow on `tag.name()` by copying the
                    // local-name slice — `tag.attrs()` consumes `tag`
                    // by value, so the borrow has to be gone first.
                    let local: Vec<u8> = strip_namespace_prefix(tag.name()).to_vec();
                    match local.as_slice() {
                        b"catalog" | b"group" => {
                            // Honour an inner `prefer=` attribute; if
                            // absent, inherit the surrounding scope.
                            let parent = prefer_stack.last().copied()
                                .unwrap_or(Prefer::Public);
                            let mut p = parent;
                            for a in tag.attrs() {
                                let a = a?;
                                if strip_namespace_prefix(a.name) == b"prefer" {
                                    p = match a.value.as_ref() {
                                        b"system" => Prefer::System,
                                        _         => Prefer::Public,
                                    };
                                }
                            }
                            if local.as_slice() == b"catalog" {
                                // Root-level prefer is also retained on
                                // the Catalog for any post-parse
                                // introspection.
                                self.root_prefer = p;
                            }
                            prefer_stack.push(p);
                        }
                        b"public" => {
                            let (mut pid, mut uri): (Option<String>, Option<String>)
                                = (None, None);
                            for a in tag.attrs() {
                                let a = a?;
                                match strip_namespace_prefix(a.name) {
                                    b"publicId" => pid = Some(decode_utf8(&a.value)?),
                                    b"uri"      => uri = Some(decode_utf8(&a.value)?),
                                    _ => {}
                                }
                            }
                            if let (Some(p), Some(u)) = (pid, uri) {
                                let prefer = prefer_stack.last().copied()
                                    .unwrap_or(Prefer::Public);
                                self.entries.push(Entry::Public {
                                    id: normalise_public_id(&p), uri: u, prefer,
                                });
                            }
                        }
                        b"system" => {
                            let (mut sid, mut uri): (Option<String>, Option<String>)
                                = (None, None);
                            for a in tag.attrs() {
                                let a = a?;
                                match strip_namespace_prefix(a.name) {
                                    b"systemId" => sid = Some(decode_utf8(&a.value)?),
                                    b"uri"      => uri = Some(decode_utf8(&a.value)?),
                                    _ => {}
                                }
                            }
                            if let (Some(s), Some(u)) = (sid, uri) {
                                self.entries.push(Entry::System { id: s, uri: u });
                            }
                        }
                        b"uri" => {
                            let (mut name, mut uri): (Option<String>, Option<String>)
                                = (None, None);
                            for a in tag.attrs() {
                                let a = a?;
                                match strip_namespace_prefix(a.name) {
                                    b"name" => name = Some(decode_utf8(&a.value)?),
                                    b"uri"  => uri  = Some(decode_utf8(&a.value)?),
                                    _ => {}
                                }
                            }
                            if let (Some(n), Some(u)) = (name, uri) {
                                self.entries.push(Entry::Uri { name: n, uri: u });
                            }
                        }
                        b"rewriteSystem" => {
                            if let Some((start, replace)) =
                                parse_rewrite_attrs(tag, b"systemIdStartString")?
                            {
                                self.entries.push(Entry::RewriteSystem { start, replace });
                            }
                        }
                        b"rewriteURI" | b"rewriteUri" => {
                            if let Some((start, replace)) =
                                parse_rewrite_attrs(tag, b"uriStartString")?
                            {
                                self.entries.push(Entry::RewriteUri { start, replace });
                            }
                        }
                        b"delegatePublic" => {
                            if let Some((start, sub)) = parse_delegate_attrs(
                                tag, b"publicIdStartString",
                                base_dir.as_deref(), loading,
                            )? {
                                let start = normalise_public_id(&start);
                                self.entries.push(Entry::DelegatePublic { start, sub });
                            }
                        }
                        b"delegateSystem" => {
                            if let Some((start, sub)) = parse_delegate_attrs(
                                tag, b"systemIdStartString",
                                base_dir.as_deref(), loading,
                            )? {
                                self.entries.push(Entry::DelegateSystem { start, sub });
                            }
                        }
                        b"delegateURI" | b"delegateUri" => {
                            if let Some((start, sub)) = parse_delegate_attrs(
                                tag, b"uriStartString",
                                base_dir.as_deref(), loading,
                            )? {
                                self.entries.push(Entry::DelegateUri { start, sub });
                            }
                        }
                        b"nextCatalog" => {
                            let mut href: Option<String> = None;
                            for a in tag.attrs() {
                                let a = a?;
                                if strip_namespace_prefix(a.name) == b"catalog" {
                                    href = Some(decode_utf8(&a.value)?);
                                }
                            }
                            if let Some(h) = href {
                                let sub = try_load_subcatalog(
                                    &h, base_dir.as_deref(), loading);
                                self.entries.push(Entry::NextCatalog { sub });
                            }
                        }
                        _ => {} // ignore unknown
                    }
                }
                _ => continue,
            }
        }
        Ok(())
    }
}

/// Walk `entries` looking for the longest rewrite prefix matching
/// `target`.  `is_system` selects between `RewriteSystem` and
/// `RewriteUri`.  Returns the replaced URI, or `None` if no entry
/// matched.
///
/// Per OASIS § 7.2.2, longest matching prefix wins; ties broken by
/// declaration order (first wins).  We compute the longest match
/// in one pass.
fn longest_rewrite(entries: &[Entry], target: &str, is_system: bool) -> Option<String> {
    let mut best: Option<(&str, &str)> = None;  // (start, replace)
    for e in entries {
        let (start, replace) = match (e, is_system) {
            (Entry::RewriteSystem { start, replace }, true)  => (start.as_str(), replace.as_str()),
            (Entry::RewriteUri    { start, replace }, false) => (start.as_str(), replace.as_str()),
            _ => continue,
        };
        if target.starts_with(start) {
            match best {
                Some((cur_start, _)) if cur_start.len() >= start.len() => {}
                _ => best = Some((start, replace)),
            }
        }
    }
    best.map(|(start, replace)| format!("{}{}", replace, &target[start.len()..]))
}

#[derive(Clone, Copy)]
enum DelegateKind { Public, System, Uri }

/// Walk `entries` looking for delegate entries whose prefix matches
/// `target`, in longest-prefix-first order.  For each match, ask
/// the sub-catalog to resolve `(public_id, system_id)`; the first
/// non-`None` result wins.  Cycles are broken via `seen`.
///
/// Per OASIS § 7.2.4, delegation matches are ordered by *prefix
/// length* (longest first); within a length, by declaration order.
/// If a delegate's sub-catalog can't resolve, the next-longest
/// delegate is tried before giving up.
fn longest_delegate<'a>(
    entries: &'a [Entry],
    target:  &str,
    kind:    DelegateKind,
    public_id: Option<&str>, system_id: Option<&str>,
    seen: &mut HashSet<PathBuf>,
) -> Option<Cow<'a, str>> {
    // Collect every matching delegate, then sort by prefix length
    // descending.  Entry count per catalog is small (typically <50)
    // so the O(n log n) sort is irrelevant.
    let mut candidates: Vec<(&'a str, &'a Catalog)> = Vec::new();
    for e in entries {
        let (start, sub_opt) = match (e, kind) {
            (Entry::DelegatePublic { start, sub }, DelegateKind::Public)
                => (start.as_str(), sub.as_deref()),
            (Entry::DelegateSystem { start, sub }, DelegateKind::System)
                => (start.as_str(), sub.as_deref()),
            (Entry::DelegateUri    { start, sub }, DelegateKind::Uri)
                => (start.as_str(), sub.as_deref()),
            _ => continue,
        };
        let Some(sub) = sub_opt else { continue; };
        if target.starts_with(start) {
            candidates.push((start, sub));
        }
    }
    candidates.sort_by_key(|(s, _)| std::cmp::Reverse(s.len()));
    for (_, sub) in candidates {
        if !mark_seen(seen, sub) { continue; }
        let resolved = match kind {
            DelegateKind::Uri => sub.resolve_uri_inner(target, seen),
            _                 => sub.resolve_inner(public_id, system_id, seen),
        };
        if let Some(uri) = resolved { return Some(uri); }
    }
    None
}

/// Read `prefix-attr="…"` and `rewritePrefix="…"` off a
/// `<rewriteSystem>` / `<rewriteUri>` start tag.  Returns the pair
/// when both are present; `None` (silent skip) otherwise — matches
/// libxml2's lenient parse stance on malformed catalog entries.
fn parse_rewrite_attrs<'r, 'src>(
    tag: crate::xml_bytes_reader::BytesStartTag<'r, 'src>,
    prefix_attr: &[u8],
) -> Result<Option<(String, String)>> {
    let (mut start, mut replace): (Option<String>, Option<String>) = (None, None);
    for a in tag.attrs() {
        let a = a?;
        let n = strip_namespace_prefix(a.name);
        if n == prefix_attr {
            start = Some(decode_utf8(&a.value)?);
        } else if n == b"rewritePrefix" {
            replace = Some(decode_utf8(&a.value)?);
        }
    }
    Ok(start.zip(replace))
}

/// Read `prefix-attr="…"` and `catalog="…"` off a `<delegate*>`
/// start tag and eagerly load the referenced sub-catalog.  The
/// sub-catalog is loaded relative to `base_dir` (the directory of
/// the catalog file we're parsing).  A failure to load
/// (file-not-found, parse error) is silently absorbed: the entry's
/// `sub` is set to `None` and resolution will skip that delegate.
fn parse_delegate_attrs<'r, 'src>(
    tag: crate::xml_bytes_reader::BytesStartTag<'r, 'src>,
    prefix_attr: &[u8],
    base_dir:    Option<&Path>,
    loading:     &mut HashSet<PathBuf>,
) -> Result<Option<(String, Option<Box<Catalog>>)>> {
    let (mut start, mut href): (Option<String>, Option<String>) = (None, None);
    for a in tag.attrs() {
        let a = a?;
        let n = strip_namespace_prefix(a.name);
        if n == prefix_attr {
            start = Some(decode_utf8(&a.value)?);
        } else if n == b"catalog" {
            href = Some(decode_utf8(&a.value)?);
        }
    }
    Ok(match (start, href) {
        (Some(s), Some(h)) => Some((s, try_load_subcatalog(&h, base_dir, loading))),
        _ => None,
    })
}

/// Best-effort load of `href` (a `catalog="…"` reference) relative
/// to `base_dir`.  Returns `None` on any failure — the OASIS spec
/// treats missing catalog references as "skip that delegation
/// branch" rather than a hard error, since catalog hierarchies
/// often reference files that aren't installed in every consumer's
/// environment.
fn try_load_subcatalog(
    href: &str, base_dir: Option<&Path>,
    loading: &mut HashSet<PathBuf>,
) -> Option<Box<Catalog>> {
    let raw = href.strip_prefix("file://").unwrap_or(href);
    let path = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        match base_dir {
            Some(d) => d.join(raw),
            None    => PathBuf::from(raw),
        }
    };
    if !path.exists() { return None; }
    // Cycle protection: skip if this file is already on the loading
    // stack.  Without this, `<nextCatalog>` chains that loop back to
    // an ancestor catalog (a → b → a) would recurse forever during
    // parse.  Resolution-time cycle detection (`mark_seen`) is a
    // second line of defence for anonymous sub-catalogs that don't
    // have an identifying path.
    let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
    if !loading.insert(canon.clone()) {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let mut sub = Catalog::default();
    sub.sources.push(path.clone());
    let result = sub.merge_from_bytes(&bytes, Some(&path), loading);
    // Whether or not parsing succeeded, pop the canonical path off
    // the loading stack so sibling catalogs can reference the same
    // file at the same depth.
    loading.remove(&canon);
    result.ok()?;
    Some(Box::new(sub))
}

/// Record this sub-catalog as visited.  Returns `false` if it was
/// already in `seen` (i.e. there's a cycle through `<nextCatalog>`
/// / `<delegate*>`).  Cycle detection is purely a safety net;
/// catalog files are typically a few-deep DAG in practice.
fn mark_seen(seen: &mut HashSet<PathBuf>, sub: &Catalog) -> bool {
    // Use the first source path as the identity for cycle detection.
    // Anonymous sub-catalogs (loaded via Catalog::parse with no path)
    // never participate in cycles since they aren't reachable by
    // path; treat them as always-fresh.
    match sub.sources.first() {
        Some(p) => seen.insert(p.clone()),
        None    => true,
    }
}

/// OASIS § 7.1: normalise a PUBLIC ID for matching.  Sequences of
/// XML whitespace (` `, `\t`, `\r`, `\n`) collapse to a single
/// space; leading and trailing whitespace is removed.  Catalogs
/// must store and compare PUBLIC IDs in this normalised form.
fn normalise_public_id(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = true;   // suppress leading whitespace
    for c in s.chars() {
        if matches!(c, ' ' | '\t' | '\r' | '\n') {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Strip a `prefix:` namespace prefix from an element / attribute
/// name.  Catalogs use the `urn:oasis:...:catalog` namespace but
/// the prefix is conventionally absent (default namespace) or
/// `cat` / `c`.  We don't currently do full namespace resolution,
/// so we accept any prefix and look at the local name.
fn strip_namespace_prefix(qname: &[u8]) -> &[u8] {
    qname.iter().position(|&b| b == b':')
        .map(|i| &qname[i + 1..])
        .unwrap_or(qname)
}

fn decode_utf8(bytes: &[u8]) -> Result<String> {
    std::str::from_utf8(bytes)
        .map(|s| s.to_string())
        .map_err(|e| XmlError::new(
            ErrorDomain::Encoding,
            ErrorLevel::Error,
            format!("catalog entry value is not valid UTF-8: {e}"),
        ))
}

/// Conventional catalog paths for the current OS, in priority
/// order.  These are the locations libxml2 (and the wider XML
/// toolchain) historically check when no `XML_CATALOG_FILES`
/// override is present.  Used by [`discover_catalog_paths`] as
/// the fallback list.
///
/// Exposed separately so callers (and tests) can reason about the
/// platform-specific path list without going through the env-var
/// check.
pub fn conventional_paths() -> Vec<PathBuf> {
    let mut out = vec![
        PathBuf::from("/etc/xml/catalog"),
        PathBuf::from("/usr/share/xml/catalog"),
    ];
    if cfg!(target_os = "macos") {
        // Homebrew on Apple Silicon, then Intel; then MacPorts.
        out.push(PathBuf::from("/opt/homebrew/etc/xml/catalog"));
        out.push(PathBuf::from("/usr/local/etc/xml/catalog"));
        out.push(PathBuf::from("/opt/local/etc/xml/catalog"));
    }
    // Per-user catalog at ~/.xmlcatalog (libxml2 doesn't check
    // this by default but it's a common convention).
    if let Some(home) = std::env::var_os("HOME") {
        let mut user = PathBuf::from(home);
        user.push(".xmlcatalog");
        out.push(user);
    }
    out
}

/// Discover XML catalog files using libxml2-compatible rules.
///
/// 1. If the `XML_CATALOG_FILES` environment variable is set, split
///    it on whitespace and use those paths.  Each entry can be a
///    plain path or a `file://` URI.  This overrides the default
///    list entirely.
/// 2. Otherwise, return [`conventional_paths`] for the current OS.
///
/// The returned list contains paths that *might* exist; callers
/// should filter for paths that actually do (or rely on
/// [`load_default`] which silently skips missing files).
pub fn discover_catalog_paths() -> Vec<PathBuf> {
    if let Ok(val) = std::env::var("XML_CATALOG_FILES") {
        return val
            .split_whitespace()
            .map(|s| PathBuf::from(s.strip_prefix("file://").unwrap_or(s)))
            .collect();
    }
    conventional_paths()
}

/// Load a catalog using the default discovery rules — convenience
/// wrapper combining [`discover_catalog_paths`] and
/// [`Catalog::from_files`].  Silently skips paths that don't exist
/// (a fresh macOS install, for example, returns an empty catalog).
pub fn load_default() -> Result<Catalog> {
    let paths: Vec<PathBuf> = discover_catalog_paths()
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    if paths.is_empty() {
        return Ok(Catalog::default());
    }
    Catalog::from_files(&paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CATALOG: &[u8] = br#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <public publicId="-//W3C//DTD XHTML 1.0 Strict//EN"
          uri="file:///usr/share/xml/xhtml/xhtml1-strict.dtd"/>
  <system systemId="http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd"
          uri="file:///usr/share/xml/xhtml/xhtml1-strict.dtd"/>
  <public publicId="-//OASIS//DTD DocBook XML V5.0//EN"
          uri="file:///usr/share/xml/docbook/docbook-5.0.dtd"/>
</catalog>
"#;

    #[test]
    fn parses_simple_catalog() {
        let cat = Catalog::parse(SAMPLE_CATALOG).unwrap();
        assert_eq!(cat.len(), 3, "expected 3 entries");
        assert!(!cat.is_empty());
    }

    #[test]
    fn resolves_public_id() {
        let cat = Catalog::parse(SAMPLE_CATALOG).unwrap();
        let uri = cat.resolve(
            Some("-//W3C//DTD XHTML 1.0 Strict//EN"),
            None,
        );
        assert_eq!(uri.as_deref(),
            Some("file:///usr/share/xml/xhtml/xhtml1-strict.dtd"));
    }

    #[test]
    fn resolves_system_id() {
        let cat = Catalog::parse(SAMPLE_CATALOG).unwrap();
        let uri = cat.resolve(
            None,
            Some("http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd"),
        );
        assert_eq!(uri.as_deref(),
            Some("file:///usr/share/xml/xhtml/xhtml1-strict.dtd"));
    }

    // ── new entry types ─────────────────────────────────────────

    #[test]
    fn rewrite_system_substitutes_longest_prefix() {
        let cat = Catalog::parse(br#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <rewriteSystem systemIdStartString="http://example.com/"
                 rewritePrefix="file:///local/example/"/>
  <rewriteSystem systemIdStartString="http://example.com/specific/"
                 rewritePrefix="file:///mirror/specific/"/>
</catalog>"#).unwrap();
        // Longest match wins — "http://example.com/specific/" beats
        // "http://example.com/" even though both apply.
        let uri = cat.resolve(None, Some("http://example.com/specific/foo.dtd"));
        assert_eq!(uri.as_deref(), Some("file:///mirror/specific/foo.dtd"));
        // Falls back to the shorter prefix when the longer one
        // doesn't apply.
        let uri = cat.resolve(None, Some("http://example.com/other/bar.dtd"));
        assert_eq!(uri.as_deref(), Some("file:///local/example/other/bar.dtd"));
    }

    #[test]
    fn rewrite_uri_handles_uri_namespace() {
        let cat = Catalog::parse(br#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <rewriteURI uriStartString="urn:isbn:" rewritePrefix="file:///isbn/"/>
</catalog>"#).unwrap();
        let uri = cat.resolve_uri("urn:isbn:1234567890");
        assert_eq!(uri.as_deref(), Some("file:///isbn/1234567890"));
    }

    #[test]
    fn next_catalog_chains_to_a_followup_file() {
        let dir  = tempdir();
        let leaf = dir.join("leaf.xml");
        std::fs::write(&leaf, br#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <system systemId="urn:my:thing" uri="file:///x/y.dtd"/>
</catalog>"#).unwrap();
        let root_path = dir.join("root.xml");
        std::fs::write(&root_path, format!(
            r#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <nextCatalog catalog="{}"/>
</catalog>"#,
            leaf.display(),
        )).unwrap();
        let cat = Catalog::from_files(&[root_path]).unwrap();
        let uri = cat.resolve(None, Some("urn:my:thing"));
        assert_eq!(uri.as_deref(), Some("file:///x/y.dtd"));
    }

    #[test]
    fn delegate_public_delegates_matching_prefix_to_subcatalog() {
        let dir = tempdir();
        let sub = dir.join("docbook.xml");
        std::fs::write(&sub, br#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <public publicId="-//OASIS//DTD DocBook XML V5.0//EN"
          uri="file:///mirror/docbook-5.0.dtd"/>
</catalog>"#).unwrap();
        let root_path = dir.join("root.xml");
        std::fs::write(&root_path, format!(
            r#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <delegatePublic publicIdStartString="-//OASIS//"
                  catalog="{}"/>
</catalog>"#,
            sub.display(),
        )).unwrap();
        let cat = Catalog::from_files(&[root_path]).unwrap();
        let uri = cat.resolve(Some("-//OASIS//DTD DocBook XML V5.0//EN"), None);
        assert_eq!(uri.as_deref(), Some("file:///mirror/docbook-5.0.dtd"));
        // Prefix that doesn't match the delegate is not resolved.
        let uri = cat.resolve(Some("-//W3C//something//EN"), None);
        assert_eq!(uri, None);
    }

    #[test]
    fn delegate_system_falls_back_when_first_subcatalog_misses() {
        let dir   = tempdir();
        let miss  = dir.join("miss.xml");
        let hit   = dir.join("hit.xml");
        std::fs::write(&miss, br#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <system systemId="urn:nope" uri="file:///never"/>
</catalog>"#).unwrap();
        std::fs::write(&hit, br#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <system systemId="urn:thing" uri="file:///found.dtd"/>
</catalog>"#).unwrap();
        let root_path = dir.join("root.xml");
        // Both delegates share a prefix that matches `urn:`.  The
        // longer-prefix one is consulted first; on miss, resolution
        // falls through to the shorter one — but here we only need
        // to verify the basic "delegate then try next" path.
        std::fs::write(&root_path, format!(
            r#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <delegateSystem systemIdStartString="urn:" catalog="{}"/>
  <delegateSystem systemIdStartString="urn:" catalog="{}"/>
</catalog>"#,
            miss.display(), hit.display(),
        )).unwrap();
        let cat = Catalog::from_files(&[root_path]).unwrap();
        let uri = cat.resolve(None, Some("urn:thing"));
        assert_eq!(uri.as_deref(), Some("file:///found.dtd"));
    }

    #[test]
    fn group_prefer_system_makes_public_entries_inert_when_system_present() {
        let cat = Catalog::parse(br#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <group prefer="system">
    <public publicId="-//Test//Public//EN"
            uri="file:///public-target"/>
  </group>
</catalog>"#).unwrap();
        // When system_id is provided, PUBLIC in a prefer="system"
        // group is bypassed — the lookup returns None.
        let uri = cat.resolve(
            Some("-//Test//Public//EN"),
            Some("urn:irrelevant-but-present"),
        );
        assert_eq!(uri, None);
        // When system_id is None, the PUBLIC entry can still match
        // (the prefer=system rule only governs the public/system
        // tie-break; there's no tie when system is absent).
        let uri = cat.resolve(Some("-//Test//Public//EN"), None);
        assert_eq!(uri.as_deref(), Some("file:///public-target"));
    }

    #[test]
    fn group_does_not_leak_prefer_to_outer_scope() {
        let cat = Catalog::parse(br#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <group prefer="system">
    <public publicId="inner" uri="file:///inner"/>
  </group>
  <public publicId="outer" uri="file:///outer"/>
</catalog>"#).unwrap();
        // The outer public entry must still be resolvable — the
        // group's prefer="system" only applied to its own scope.
        let uri = cat.resolve(Some("outer"), Some("urn:some-sys"));
        assert_eq!(uri.as_deref(), Some("file:///outer"));
    }

    #[test]
    fn next_catalog_cycle_is_broken_safely() {
        let dir = tempdir();
        let a = dir.join("a.xml");
        let b = dir.join("b.xml");
        std::fs::write(&a, format!(
            r#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <nextCatalog catalog="{}"/>
</catalog>"#, b.display())).unwrap();
        std::fs::write(&b, format!(
            r#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <nextCatalog catalog="{}"/>
</catalog>"#, a.display())).unwrap();
        // Cycle a → b → a; resolution must terminate and return
        // None rather than recurse forever.
        let cat = Catalog::from_files(&[a]).unwrap();
        let uri = cat.resolve(None, Some("urn:does-not-exist"));
        assert_eq!(uri, None);
    }

    #[test]
    fn missing_delegate_file_is_silently_skipped() {
        let cat = Catalog::parse(br#"<?xml version="1.0"?>
<catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
  <delegateSystem systemIdStartString="urn:"
                  catalog="/nowhere/missing-catalog.xml"/>
  <system systemId="urn:thing" uri="file:///fallback.dtd"/>
</catalog>"#).unwrap();
        // The dead delegate must not prevent the later <system>
        // entry from matching.  But OASIS order is: system exact
        // first, so this resolves before delegate is consulted.
        // Either way, the lookup should not error.
        let uri = cat.resolve(None, Some("urn:thing"));
        assert_eq!(uri.as_deref(), Some("file:///fallback.dtd"));
    }

    /// Tempdir helper — creates a per-test directory and returns
    /// its path.  Not auto-cleaned (test isolation matters more
    /// than tidy `/tmp`).  Names combine the per-process pid with
    /// a per-call counter so two tests running in the same
    /// nanosecond never collide.
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("supxml-catalog-test-{}-{n}", std::process::id());
        let dir = std::env::temp_dir().join(name);
        // Fresh start in case a previous run left this name behind.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
