#![forbid(unsafe_code)]  // see CONTRIBUTING.md § "Unsafe policy"

//! Pluggable resolver for external XML resources (DTDs, parsed
//! entities) referenced by SYSTEM / PUBLIC identifier.
//!
//! # Why this exists
//!
//! XML 1.0 lets DOCTYPE declarations and `<!ENTITY>` declarations
//! reference external resources by URL.  Loading those resources
//! is *the* primary XXE attack vector — a malicious document can
//! cause a parser to fetch arbitrary URLs, leak local files, or
//! trigger SSRF against internal services.
//!
//! SupXML's default behaviour is to refuse all external loading.
//! Callers who genuinely need it (DocBook publishing, XHTML 1.0
//! processing, JATS, etc.) opt in by setting
//! `ParseOptions::external_resolver` to either:
//!
//! - The recommended [`FilesystemResolver`] — loads from a
//!   configured allowlist of local directories, optionally
//!   consulting an OASIS catalog first.
//! - A [`NetworkResolver`](crate::entity_resolver::NetworkResolver)
//!   (behind the `network-resolver` feature) — fetches over HTTPS
//!   from a configured host allowlist with SSRF defenses.
//! - A [`ChainedResolver`] composing the two — try local first,
//!   fall back to network for anything not pre-cached.
//! - A custom [`EntityResolver`] impl for bespoke setups
//!   (in-memory bundles, S3, audit-logging, etc.).
//!
//! The presence of a resolver IS the opt-in.  Without one, every
//! external reference is rejected.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::catalog::Catalog;

/// Resolves external XML resources by their public/system
/// identifier.  Implementors decide what URLs are loadable, where
/// the bytes come from, and what security checks apply.
///
/// Implementations must be `Send + Sync` so the resolver can be
/// shared across threads or used from async contexts.
pub trait EntityResolver: Send + Sync + std::fmt::Debug {
    /// Resolve an external entity reference.
    ///
    /// - `public_id`: the FPI (formal public identifier) from a
    ///   PUBLIC declaration, if present.  Catalog-based resolvers
    ///   try this first per OASIS § 7.1.1.
    /// - `system_id`: an *already-absolute* SYSTEM URL.  The parser
    ///   performs base-URI resolution (XML 1.0 § 4.2.2 + errata
    ///   E18) before calling — relative literals in the DTD are
    ///   joined against the document URL for general-entity
    ///   declarations and against the containing entity's URL for
    ///   parameter-entity declarations.  Resolvers that don't
    ///   consult a catalog use this to locate the bytes; no URI
    ///   joining is required from the implementation.
    /// - `base_uri`: the base URI the parser used when pre-joining
    ///   the SYSTEM identifier.  Informational — most resolvers
    ///   can ignore it.  Catalog-aware resolvers may consult it
    ///   when the catalog lookup falls back to filesystem (e.g.
    ///   to scope a security check), and resolvers that want to
    ///   log or report the *original* document context can use it.
    ///   Correctness does **not** require consuming this parameter.
    ///
    /// Returns the entity's bytes on success.  Use
    /// [`ResolveError::Refused`] when the resolver chose not to
    /// load (security policy denied) versus
    /// [`ResolveError::Io`] when loading failed for an external
    /// reason.
    fn resolve(
        &self,
        public_id: Option<&str>,
        system_id: &str,
        base_uri: Option<&str>,
    ) -> Result<Vec<u8>, ResolveError>;
}

/// Why a resolver couldn't deliver the requested bytes.
#[derive(Debug)]
pub enum ResolveError {
    /// The resolver refused the request — security policy denied
    /// it (URL not in allowlist, scheme not allowed, host blocked,
    /// etc.).  Distinguished from `Io` so [`ChainedResolver`] can
    /// fall through to the next resolver in the chain.
    Refused(String),
    /// Loading was attempted but failed — file not found, network
    /// error, TLS failure, response too large, etc.
    Io(String),
    /// Resolver-specific error not covered above.
    Other(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Refused(s) => write!(f, "resolver refused: {s}"),
            ResolveError::Io(s)      => write!(f, "resolver I/O error: {s}"),
            ResolveError::Other(s)   => write!(f, "resolver error: {s}"),
        }
    }
}

impl std::error::Error for ResolveError {}

// ── FilesystemResolver ─────────────────────────────────────────

/// Recommended default resolver: loads from filesystem only,
/// refuses anything outside the configured allowed-root
/// directories, optionally consults an OASIS catalog first.
///
/// The allowed-roots list is the *security boundary*.  Empty list
/// = nothing will resolve.  Pass specific directories like
/// `/usr/share/xml`; never pass `/`.
#[derive(Debug, Clone)]
pub struct FilesystemResolver {
    catalog: Option<Catalog>,
    allowed_roots: Vec<PathBuf>,
}

impl FilesystemResolver {
    /// Construct a resolver allowed to read from the given
    /// directories.  Files outside these directories will be
    /// refused even if the catalog or the system_id points at
    /// them.
    pub fn new(allowed_roots: Vec<PathBuf>) -> Self {
        Self { catalog: None, allowed_roots }
    }

    /// Builder: consult this catalog before falling back to the
    /// raw filesystem.  PUBLIC IDs in the catalog map to local
    /// `file://` URIs.
    pub fn with_catalog(mut self, catalog: Catalog) -> Self {
        self.catalog = Some(catalog);
        self
    }

}

impl FilesystemResolver {
    /// Catalog lookup + scheme normalisation + canonicalise +
    /// allowlist check.  Returns the **canonical** path on
    /// success — symlinks resolved, ready to hand to
    /// [`Self::read_validated`].  Crate-internal so tests can
    /// exercise the canonicalize-then-read boundary that the
    /// TOCTOU mitigation hinges on.
    pub(crate) fn validate_path(
        &self,
        public_id: Option<&str>,
        system_id: &str,
    ) -> Result<PathBuf, ResolveError> {
        // Catalog lookup first (PUBLIC takes precedence, then
        // SYSTEM per OASIS § 7.1.1).  If the catalog returns a
        // mapping, use that path; otherwise fall back to system_id.
        // Catalog returns a `Cow` — direct PUBLIC/SYSTEM matches
        // borrow from the catalog (no alloc); rewrite entries own
        // the synthesised URI.  We always need an owned `String`
        // here (it's handed to `Path::new` and eventually to
        // `canonicalize`), so `.into_owned()` either reuses the
        // owned variant or allocates once for the borrowed case.
        let target = self.catalog.as_ref()
            .and_then(|c| c.resolve(public_id, Some(system_id)))
            .map(|c| c.into_owned())
            .unwrap_or_else(|| system_id.to_string());

        // Strip the file:// scheme if present.  Refuse any other
        // scheme — this resolver does filesystem only.
        let path_str = if let Some(rest) = target.strip_prefix("file://") {
            rest
        } else if target.contains("://") {
            return Err(ResolveError::Refused(format!(
                "FilesystemResolver only handles file:// URIs, got: {target}"
            )));
        } else {
            &target
        };
        let path = PathBuf::from(path_str);

        // Canonicalise once; reject if not within allowed roots.
        // The canonical path is what we'll pass to read_validated
        // — it has all symlinks already resolved, so a follow-up
        // symlink swap at the *original* path can't redirect the
        // read.
        let canonical = path.canonicalize().map_err(|_| {
            ResolveError::Refused(format!(
                "path {} is outside the configured allowed roots",
                path.display()
            ))
        })?;
        if !self.allowed_roots.iter().any(|root| {
            root.canonicalize()
                .map(|cr| canonical.starts_with(&cr))
                .unwrap_or(false)
        }) {
            return Err(ResolveError::Refused(format!(
                "path {} is outside the configured allowed roots",
                path.display()
            )));
        }
        Ok(canonical)
    }

    /// Read bytes from a path previously validated by
    /// [`Self::validate_path`].  On Unix the open refuses to
    /// follow a symlink at the final path component — closing
    /// the canonicalize→read TOCTOU window where an attacker
    /// with write access in the allowed root swaps the file to
    /// a symlink between the validate and read steps.
    pub(crate) fn read_validated(
        &self,
        canonical: &std::path::Path,
    ) -> Result<Vec<u8>, ResolveError> {
        use std::io::Read;
        let file = open_no_follow(canonical).map_err(|e| ResolveError::Io(format!(
            "reading {}: {e}", canonical.display()
        )))?;
        let mut buf = Vec::new();
        let mut reader = file;
        reader.read_to_end(&mut buf).map_err(|e| ResolveError::Io(format!(
            "reading {}: {e}", canonical.display()
        )))?;
        Ok(buf)
    }
}

/// Open a file for reading, refusing to follow a symlink at the
/// final path component on Unix.  On non-Unix platforms this is
/// a plain open of the canonical path; the canonicalize-step in
/// [`FilesystemResolver::validate_path`] still resolves
/// pre-existing symlinks, leaving only a small residual window
/// for an attacker who can swap the file at the canonical
/// location between validate and read.
fn open_no_follow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    // POSIX-stable `O_NOFOLLOW` value, inlined to avoid pulling
    // in `libc` for a single constant.  Linux/Android use the
    // glibc value; the BSD-derived family (Apple + the actual
    // BSDs) share the historical 0x100.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const O_NOFOLLOW: i32 = 0o400000;
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd",
              target_os = "netbsd", target_os = "openbsd", target_os = "dragonfly"))]
    const O_NOFOLLOW: i32 = 0x0100;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::File::open(path)
    }
}

impl EntityResolver for FilesystemResolver {
    fn resolve(
        &self,
        public_id: Option<&str>,
        system_id: &str,
        _base_uri: Option<&str>,
    ) -> Result<Vec<u8>, ResolveError> {
        let canonical = self.validate_path(public_id, system_id)?;
        self.read_validated(&canonical)
    }
}

// ── ChainedResolver ────────────────────────────────────────────

/// Composes multiple resolvers, trying each in order.  The first
/// resolver to return either `Ok` or a non-`Refused` error wins;
/// `Refused` falls through to the next resolver.
///
/// Typical use: filesystem first (cheap, deterministic), network
/// second (slower, requires network access).
#[derive(Debug)]
pub struct ChainedResolver {
    resolvers: Vec<Arc<dyn EntityResolver>>,
}

impl ChainedResolver {
    pub fn new(resolvers: Vec<Arc<dyn EntityResolver>>) -> Self {
        Self { resolvers }
    }
}

impl EntityResolver for ChainedResolver {
    fn resolve(
        &self,
        public_id: Option<&str>,
        system_id: &str,
        base_uri: Option<&str>,
    ) -> Result<Vec<u8>, ResolveError> {
        let mut last_refused: Option<ResolveError> = None;
        for r in &self.resolvers {
            match r.resolve(public_id, system_id, base_uri) {
                Ok(bytes) => return Ok(bytes),
                Err(e @ ResolveError::Refused(_)) => {
                    last_refused = Some(e);
                    continue;
                }
                Err(other) => return Err(other),
            }
        }
        // All resolvers refused.  Surface the last one for
        // diagnostic purposes.
        Err(last_refused.unwrap_or_else(|| ResolveError::Refused(
            "no resolvers in chain".to_string()
        )))
    }
}

// ── InMemoryResolver (mainly for tests, occasionally for
//    embedded resources) ────────────────────────────────────────

/// Resolver backed by an in-memory map from system_id to bytes.
/// Refuses anything not in the map.  Useful in tests where you
/// want deterministic resolution without filesystem dependencies.
#[derive(Debug, Default, Clone)]
pub struct InMemoryResolver {
    by_system_id: HashMap<String, Vec<u8>>,
    by_public_id: HashMap<String, Vec<u8>>,
}

impl InMemoryResolver {
    pub fn new() -> Self { Self::default() }

    pub fn with_system(mut self, system_id: &str, bytes: Vec<u8>) -> Self {
        self.by_system_id.insert(system_id.to_string(), bytes);
        self
    }

    pub fn with_public(mut self, public_id: &str, bytes: Vec<u8>) -> Self {
        self.by_public_id.insert(public_id.to_string(), bytes);
        self
    }
}

impl EntityResolver for InMemoryResolver {
    fn resolve(
        &self,
        public_id: Option<&str>,
        system_id: &str,
        _base_uri: Option<&str>,
    ) -> Result<Vec<u8>, ResolveError> {
        if let Some(p) = public_id {
            if let Some(b) = self.by_public_id.get(p) {
                return Ok(b.clone());
            }
        }
        if let Some(b) = self.by_system_id.get(system_id) {
            return Ok(b.clone());
        }
        Err(ResolveError::Refused(format!(
            "InMemoryResolver has no entry for system_id={system_id:?} public_id={public_id:?}"
        )))
    }
}

// ── NetworkResolver (feature-gated) ────────────────────────────

#[cfg(feature = "network-resolver")]
mod network {
    use super::*;
    use std::collections::HashSet;
    use std::net::IpAddr;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Pluggable DNS resolver — used by [`NetworkResolver`] for
    /// the up-front IP check and for pinning the IP into the
    /// ureq agent so the connection can't follow a rebound DNS
    /// answer.  Crate-internal: the only intended consumers are
    /// the default [`StdDnsResolver`] and crate tests that
    /// inject mock impls.  Not part of the public API — promote
    /// to `pub` if/when an external use case appears.
    pub(crate) trait DnsResolver: Send + Sync + std::fmt::Debug {
        fn lookup(&self, host: &str, port: u16) -> Vec<std::net::SocketAddr>;
    }

    /// Default DNS resolver — `getaddrinfo` via std.
    #[derive(Debug, Default)]
    pub(crate) struct StdDnsResolver;

    impl DnsResolver for StdDnsResolver {
        fn lookup(&self, host: &str, port: u16) -> Vec<std::net::SocketAddr> {
            use std::net::ToSocketAddrs;
            (host, port).to_socket_addrs()
                .map(|iter| iter.collect())
                .unwrap_or_default()
        }
    }

    /// HTTPS-fetching resolver.  Hardened by default with multiple
    /// SSRF and amplification defenses; the constructor *requires*
    /// a host allowlist so there's no convenient "any host" mode.
    ///
    /// **Defaults:**
    /// - HTTPS only (use [`with_plaintext_http`] to allow `http://`)
    /// - Refuses URLs whose resolved IP is RFC 1918 / loopback /
    ///   link-local (use [`with_private_ips_allowed`] to disable)
    /// - 10-second per-request timeout
    /// - 1 MB max response size
    /// - 64 MB in-memory LRU cache (per resolver instance)
    pub struct NetworkResolver {
        allowed_hosts:        HashSet<String>,
        block_private_ips:    bool,
        allow_plaintext_http: bool,
        max_response_bytes:   usize,
        timeout:              Duration,
        cache:                Mutex<lru::Cache>,
        /// DNS resolver used both for the up-front IP check and
        /// for pinning the IP that ureq connects to.  Sharing a
        /// single instance prevents a DNS-rebinding TOCTOU problem
        /// because we resolve once, verify, then hand the verified
        /// socket address to ureq so it doesn't re-query.
        dns:                  std::sync::Arc<dyn DnsResolver>,
    }

    impl std::fmt::Debug for NetworkResolver {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("NetworkResolver")
                .field("allowed_hosts",        &self.allowed_hosts)
                .field("block_private_ips",    &self.block_private_ips)
                .field("allow_plaintext_http", &self.allow_plaintext_http)
                .field("max_response_bytes",   &self.max_response_bytes)
                .field("timeout",              &self.timeout)
                .finish_non_exhaustive()
        }
    }

    impl NetworkResolver {
        /// Construct with the required host allowlist.  Hosts are
        /// matched exactly (no wildcards) against the URL's host
        /// component.  All other settings get safe defaults; use
        /// the `with_*` builders to relax them.
        pub fn new<I: IntoIterator<Item = String>>(allowed_hosts: I) -> Self {
            Self {
                allowed_hosts:        allowed_hosts.into_iter().collect(),
                block_private_ips:    true,
                allow_plaintext_http: false,
                max_response_bytes:   1 * 1024 * 1024,    // 1 MB
                timeout:              Duration::from_secs(10),
                cache:                Mutex::new(lru::Cache::new(64 * 1024 * 1024)),
                dns:                  std::sync::Arc::new(StdDnsResolver),
            }
        }

        /// Override the DNS resolver.  Crate-internal test seam —
        /// used by the network_tests module to inject a mock DNS
        /// that maps a synthetic hostname to a local listener.
        /// Not part of the public API; if a real external use
        /// case appears (custom resolvers, hosts overrides),
        /// promote to `pub` and stabilise the [`DnsResolver`]
        /// trait at the same time.
        #[cfg(test)]
        pub(crate) fn with_dns_resolver(mut self, dns: std::sync::Arc<dyn DnsResolver>) -> Self {
            self.dns = dns;
            self
        }

        /// Allow `http://` URLs (default: HTTPS-only).  Almost
        /// always wrong; use only for testing or air-gapped
        /// networks where TLS isn't available.
        pub fn with_plaintext_http(mut self) -> Self {
            self.allow_plaintext_http = true;
            self
        }

        /// Allow URLs whose resolved IP is RFC 1918 private,
        /// loopback, or link-local (default: refused for SSRF
        /// defense).  Disable only if your `allowed_hosts` list
        /// already constrains to trusted internal hosts.
        pub fn with_private_ips_allowed(mut self) -> Self {
            self.block_private_ips = false;
            self
        }

        pub fn with_max_response_bytes(mut self, n: usize) -> Self {
            self.max_response_bytes = n;
            self
        }

        pub fn with_timeout(mut self, d: Duration) -> Self {
            self.timeout = d;
            self
        }

        pub fn with_cache_size(mut self, max_total_bytes: usize) -> Self {
            self.cache = Mutex::new(lru::Cache::new(max_total_bytes));
            self
        }

        /// Validate a URL against our security policy.  Returns
        /// `(host, port, verified_addrs)` — the verified DNS
        /// resolution result is threaded onward to ureq so it
        /// connects to the IP we checked and never re-queries
        /// DNS.  Refuses otherwise.  We extract scheme/host with
        /// cheap manual parsing rather than pulling in the `url`
        /// crate.
        fn check_url(&self, url: &str)
            -> Result<(String, u16, Vec<std::net::SocketAddr>), ResolveError>
        {
            // Split on `://`.
            let (scheme, rest) = url.split_once("://").ok_or_else(|| {
                ResolveError::Refused(format!("URL {url:?} missing scheme://"))
            })?;
            let default_port: u16 = match scheme {
                "https" => 443,
                "http" if self.allow_plaintext_http => 80,
                other => return Err(ResolveError::Refused(format!(
                    "scheme {other:?} not allowed (use with_plaintext_http() to permit http://)"
                ))),
            };
            // Host is bytes up to the first `/`, `?`, `#`, or end.
            // Strip optional port.  We don't support userinfo
            // (`user:pass@`) — if you need it, use a custom resolver.
            let auth = rest.split(|c: char| matches!(c, '/' | '?' | '#'))
                .next().unwrap_or("");
            if auth.is_empty() {
                return Err(ResolveError::Refused(
                    format!("URL {url:?} has no host component")
                ));
            }
            if auth.contains('@') {
                return Err(ResolveError::Refused(format!(
                    "URLs with userinfo (user@host) are not supported by NetworkResolver"
                )));
            }
            let (host, port) = match auth.rsplit_once(':') {
                Some((h, p)) => {
                    // Skip if `:` is inside an IPv6 literal `[…]`.
                    if h.starts_with('[') {
                        (auth, default_port)
                    } else {
                        let port = p.parse::<u16>().map_err(|e| ResolveError::Refused(
                            format!("invalid port in URL {url:?}: {e}")
                        ))?;
                        (h, port)
                    }
                }
                None => (auth, default_port),
            };
            if !self.allowed_hosts.contains(host) {
                return Err(ResolveError::Refused(format!(
                    "host {host:?} is not in the allowed-hosts list"
                )));
            }
            // Resolve DNS ONCE here and (post-fix) pin the result
            // into the ureq agent.  Doing the lookup a second time
            // inside ureq would re-open a DNS-rebinding TOCTOU
            // window where an attacker controlling DNS for an
            // allowlisted host returns a public IP first (passes
            // the private-IP check) then 169.254.169.254 (IMDS).
            let addrs = self.dns.lookup(host, port);
            if self.block_private_ips {
                for sa in &addrs {
                    let ip = sa.ip();
                    if is_private_or_loopback(&ip) {
                        return Err(ResolveError::Refused(format!(
                            "host {host:?} resolves to private/loopback IP {ip} \
                             (use with_private_ips_allowed() to permit)"
                        )));
                    }
                }
            }
            Ok((host.to_string(), port, addrs))
        }
    }

    impl EntityResolver for NetworkResolver {
        fn resolve(
            &self,
            _public_id: Option<&str>,
            system_id: &str,
            _base_uri: Option<&str>,
        ) -> Result<Vec<u8>, ResolveError> {
            // Cache hit?
            if let Some(bytes) = self.cache.lock().unwrap().get(system_id) {
                return Ok(bytes);
            }
            // Validate URL + host + IP and capture the verified
            // socket addresses so we can pin them into ureq.
            let (_host, _port, verified_addrs) = self.check_url(system_id)?;

            // Issue the request.  Pinning DNS: ureq's agent gets
            // a custom resolver that returns the addresses we
            // already verified, so it never performs its own
            // lookup and an attacker controlling DNS can't
            // rebind between our private-IP check and the actual
            // connect.  TLS SNI / Host header stay correct
            // because the URL still carries the hostname.
            use std::io::Read;
            let pinned = verified_addrs.clone();
            let agent = ureq::AgentBuilder::new()
                .timeout(self.timeout)
                .resolver(move |_addr: &str| -> std::io::Result<Vec<std::net::SocketAddr>> {
                    Ok(pinned.clone())
                })
                .build();
            let resp = agent.get(system_id).call()
                .map_err(|e| ResolveError::Io(format!("HTTP request failed: {e}")))?;
            let reader = resp.into_reader();
            let mut limited = reader.take(self.max_response_bytes as u64 + 1);
            let mut bytes = Vec::with_capacity(8 * 1024);
            limited.read_to_end(&mut bytes)
                .map_err(|e| ResolveError::Io(format!("reading response body: {e}")))?;
            if bytes.len() > self.max_response_bytes {
                return Err(ResolveError::Refused(format!(
                    "response body exceeds max_response_bytes ({})",
                    self.max_response_bytes
                )));
            }
            // Cache and return.
            self.cache.lock().unwrap().insert(system_id.to_string(), bytes.clone());
            Ok(bytes)
        }
    }

    fn is_private_or_loopback(ip: &IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => {
                v4.is_private() || v4.is_loopback() || v4.is_link_local()
                    || v4.is_unspecified() || v4.is_broadcast()
            }
            IpAddr::V6(v6) => {
                v6.is_loopback() || v6.is_unspecified()
                    // Unique-local fc00::/7
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    // Link-local fe80::/10
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        }
    }

    /// Tiny LRU byte cache — entries evict in insertion order
    /// once total stored bytes exceed the cap.  Not optimised;
    /// good enough for the few-DTDs-per-process case
    /// `NetworkResolver` is meant for.  ~80 lines vs pulling in
    /// the `lru` crate.
    mod lru {
        use std::collections::VecDeque;

        pub(super) struct Cache {
            cap: usize,
            cur: usize,
            entries: VecDeque<(String, Vec<u8>)>,
        }

        impl Cache {
            pub(super) fn new(cap: usize) -> Self {
                Self { cap, cur: 0, entries: VecDeque::new() }
            }

            pub(super) fn get(&mut self, key: &str) -> Option<Vec<u8>> {
                let pos = self.entries.iter().position(|(k, _)| k == key)?;
                // Move to back (most-recently-used).
                let entry = self.entries.remove(pos)?;
                let bytes = entry.1.clone();
                self.entries.push_back(entry);
                Some(bytes)
            }

            pub(super) fn insert(&mut self, key: String, value: Vec<u8>) {
                self.cur += value.len();
                self.entries.push_back((key, value));
                while self.cur > self.cap {
                    if let Some((_, v)) = self.entries.pop_front() {
                        self.cur -= v.len();
                    } else { break; }
                }
            }
        }
    }

}

#[cfg(feature = "network-resolver")]
pub use network::NetworkResolver;

// Crate-internal seam reachable by the in-file `network_tests`
// module — kept out of the public API.
#[cfg(all(test, feature = "network-resolver"))]
use network::DnsResolver;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_resolver_refuses_path_outside_roots() {
        let r = FilesystemResolver::new(vec![std::env::temp_dir()]);
        let err = r.resolve(None, "file:///etc/passwd", None).unwrap_err();
        assert!(matches!(err, ResolveError::Refused(_)),
            "expected Refused, got {err:?}");
    }

    #[test]
    fn fs_resolver_refuses_non_file_scheme() {
        let r = FilesystemResolver::new(vec![std::env::temp_dir()]);
        let err = r.resolve(None, "https://example.com/foo.dtd", None).unwrap_err();
        match err {
            ResolveError::Refused(msg) => assert!(msg.contains("file://")),
            other => panic!("expected Refused about file://, got {other:?}"),
        }
    }

    #[test]
    fn fs_resolver_loads_from_allowed_root() {
        let dir = std::env::temp_dir();
        let path = dir.join("sup-xml-fs-resolver-test.dtd");
        std::fs::write(&path, b"<!-- a test DTD -->").unwrap();

        let r = FilesystemResolver::new(vec![dir.clone()]);
        let bytes = r.resolve(None, &format!("file://{}", path.display()), None)
            .expect("should load successfully");
        assert_eq!(bytes, b"<!-- a test DTD -->");

        let _ = std::fs::remove_file(&path);
    }

    /// Security regression for the canonicalize→read TOCTOU:
    /// after [`FilesystemResolver::validate_path`] returns a
    /// canonical path inside the allowed roots, the
    /// corresponding [`FilesystemResolver::read_validated`] call
    /// must NOT follow a symlink that an attacker has just
    /// dropped at that location.
    ///
    /// The split between `validate_path` and `read_validated`
    /// gives us a deterministic stand-in for the race: we
    /// perform the swap from the test thread between the two
    /// calls, simulating the worst-case attacker timing.
    #[test]
    #[cfg(unix)]
    fn fs_resolver_refuses_symlink_swap_between_validate_and_read() {
        use std::os::unix::fs::symlink;
        let allowed = std::env::temp_dir()
            .join(format!("sup-xml-fs-toctou-allowed-{}", std::process::id()));
        std::fs::create_dir_all(&allowed).unwrap();
        let inside = allowed.join("inner.dtd");
        std::fs::write(&inside, b"INSIDE").unwrap();
        let outside = std::env::temp_dir()
            .join(format!("sup-xml-fs-toctou-secret-{}.dtd", std::process::id()));
        std::fs::write(&outside, b"SECRET").unwrap();

        let r = FilesystemResolver::new(vec![allowed.clone()]);
        let system_id = format!("file://{}", inside.display());
        // Time-of-check: regular file → canonical path inside allowed
        // roots → validation passes.
        let canonical = r.validate_path(None, &system_id)
            .expect("validate should pass for legitimate file inside allowed root");

        // The race: attacker replaces `inside` with a symlink to
        // `outside` — what a TOCTOU attacker would do between the
        // resolver's canonicalize and its read.
        std::fs::remove_file(&inside).unwrap();
        symlink(&outside, &inside).unwrap();

        // Time-of-use: the resolver must refuse to follow the
        // newly-planted symlink.  Either an Io error from
        // O_NOFOLLOW, or somehow the safe content — never the
        // secret bytes.
        let result = r.read_validated(&canonical);
        let _ = std::fs::remove_file(&inside);
        let _ = std::fs::remove_dir_all(&allowed);
        let _ = std::fs::remove_file(&outside);
        match result {
            Ok(bytes) => assert_ne!(
                bytes, b"SECRET",
                "TOCTOU: read followed the swapped symlink and returned out-of-root content"
            ),
            Err(_) => {}  // refusal is the safe outcome
        }
    }

    #[test]
    fn fs_resolver_consults_catalog_first() {
        let dir = std::env::temp_dir();
        let real_path = dir.join("sup-xml-fs-resolver-cat-test.dtd");
        std::fs::write(&real_path, b"loaded via catalog").unwrap();

        // Catalog maps a public ID to the local file.
        let cat_xml = format!(r#"<?xml version="1.0"?>
            <catalog xmlns="urn:oasis:names:tc:entity:xmlns:xml:catalog">
                <public publicId="-//Test//DTD//EN" uri="file://{}"/>
            </catalog>"#, real_path.display());
        let cat = Catalog::parse(cat_xml.as_bytes()).unwrap();

        let r = FilesystemResolver::new(vec![dir.clone()]).with_catalog(cat);
        let bytes = r.resolve(
            Some("-//Test//DTD//EN"),
            "http://example.com/never-loaded",  // catalog hijacks; we never read this
            None,
        ).unwrap();
        assert_eq!(bytes, b"loaded via catalog");

        let _ = std::fs::remove_file(&real_path);
    }

    #[test]
    fn in_memory_resolver_round_trip() {
        let r = InMemoryResolver::new()
            .with_public("-//Test//DTD//EN", b"public-bytes".to_vec())
            .with_system("http://example.com/foo.dtd", b"system-bytes".to_vec());

        // PUBLIC match.
        let b = r.resolve(Some("-//Test//DTD//EN"), "http://nope", None).unwrap();
        assert_eq!(b, b"public-bytes");
        // SYSTEM-only match.
        let b = r.resolve(None, "http://example.com/foo.dtd", None).unwrap();
        assert_eq!(b, b"system-bytes");
        // No match.
        let err = r.resolve(None, "http://other", None).unwrap_err();
        assert!(matches!(err, ResolveError::Refused(_)));
    }

    #[test]
    fn chained_falls_through_refused() {
        // First resolver always refuses; second one has the entry.
        let in_mem = InMemoryResolver::new()
            .with_system("foo", b"from-second".to_vec());
        let chain = ChainedResolver::new(vec![
            Arc::new(InMemoryResolver::new()),  // empty → refuses everything
            Arc::new(in_mem),
        ]);
        let bytes = chain.resolve(None, "foo", None).unwrap();
        assert_eq!(bytes, b"from-second");
    }

    #[test]
    fn chained_propagates_io_errors() {
        // A non-Refused error stops the chain — the caller sees
        // the I/O failure rather than us silently trying the next
        // resolver.  Use a custom resolver that returns Io.
        #[derive(Debug)]
        struct Failer;
        impl EntityResolver for Failer {
            fn resolve(&self, _: Option<&str>, _: &str, _: Option<&str>)
                -> Result<Vec<u8>, ResolveError> {
                Err(ResolveError::Io("simulated network failure".into()))
            }
        }
        let chain = ChainedResolver::new(vec![
            Arc::new(Failer),
            Arc::new(InMemoryResolver::new().with_system("foo", b"x".to_vec())),
        ]);
        let err = chain.resolve(None, "foo", None).unwrap_err();
        assert!(matches!(err, ResolveError::Io(_)));
    }

    #[test]
    fn empty_chain_returns_refused() {
        let chain = ChainedResolver::new(vec![]);
        let err = chain.resolve(None, "foo", None).unwrap_err();
        assert!(matches!(err, ResolveError::Refused(_)));
    }

    // ── NetworkResolver tests (security-policy paths only — no
    //    real network calls) ──────────────────────────────────────
    #[cfg(feature = "network-resolver")]
    mod network_tests {
        use super::*;

        #[test]
        fn refuses_non_https_by_default() {
            let r = NetworkResolver::new(["example.com".to_string()]);
            let err = r.resolve(None, "http://example.com/foo.dtd", None).unwrap_err();
            match err {
                ResolveError::Refused(msg) => assert!(msg.contains("scheme")),
                other => panic!("expected Refused about scheme, got {other:?}"),
            }
        }

        #[test]
        fn refuses_unknown_host() {
            let r = NetworkResolver::new(["allowed.example.com".to_string()]);
            let err = r.resolve(None, "https://other.example.com/foo.dtd", None)
                .unwrap_err();
            match err {
                ResolveError::Refused(msg) => assert!(msg.contains("not in the allowed-hosts")),
                other => panic!("expected Refused about host allowlist, got {other:?}"),
            }
        }

        #[test]
        fn refuses_userinfo_in_url() {
            let r = NetworkResolver::new(["example.com".to_string()]);
            let err = r.resolve(None, "https://user:pass@example.com/foo.dtd", None)
                .unwrap_err();
            match err {
                ResolveError::Refused(msg) => assert!(msg.contains("userinfo")),
                other => panic!("expected Refused about userinfo, got {other:?}"),
            }
        }

        #[test]
        fn refuses_malformed_url() {
            let r = NetworkResolver::new(["example.com".to_string()]);
            let err = r.resolve(None, "not-a-url-at-all", None).unwrap_err();
            assert!(matches!(err, ResolveError::Refused(_)));
        }

        /// Security regression for the DNS-rebinding TOCTOU window.
        /// The resolver must hand ureq the IP it already verified
        /// — otherwise ureq performs its own DNS lookup and an
        /// attacker controlling DNS for an allowlisted host can
        /// return a public IP for the private-IP check then a
        /// private IP (e.g. IMDS 169.254.169.254) for the actual
        /// connect.
        ///
        /// Test setup: bind a real TCP listener on 127.0.0.1, then
        /// configure `NetworkResolver` with a custom `DnsResolver`
        /// that maps `pinned.test` → that listener.  If ureq uses
        /// our pinned IP, the connection reaches the listener and
        /// the body comes back.  If ureq does its own DNS lookup
        /// of `pinned.test` (which doesn't resolve in real DNS),
        /// the request fails — that's the TOCTOU-still-open
        /// signal.
        #[test]
        fn pins_verified_ip_into_agent_resolver() {
            use std::io::{Read, Write};
            use std::net::{SocketAddr, TcpListener};
            use std::sync::Arc;

            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();

            // Accept one connection, drain the request, send a
            // minimal HTTP response.  Joined at the end so any
            // panic from the server thread surfaces.
            let server = std::thread::spawn(move || {
                let (mut stream, _peer) = listener.accept().unwrap();
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                stream.write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Length: 5\r\n\
                      Content-Type: application/xml\r\n\
                      \r\n\
                      hello"
                ).unwrap();
            });

            #[derive(Debug)]
            struct FakeDns { port: u16 }
            impl DnsResolver for FakeDns {
                fn lookup(&self, host: &str, _port: u16) -> Vec<SocketAddr> {
                    if host == "pinned.test" {
                        vec![SocketAddr::from(([127, 0, 0, 1], self.port))]
                    } else { vec![] }
                }
            }

            let r = NetworkResolver::new(["pinned.test".to_string()])
                .with_plaintext_http()
                .with_private_ips_allowed()  // listener is on loopback
                .with_dns_resolver(Arc::new(FakeDns { port }));

            let bytes = r.resolve(None, &format!("http://pinned.test:{port}/foo.dtd"), None)
                .expect("ureq should connect to the pinned IP, not re-resolve via real DNS");
            assert_eq!(bytes, b"hello");
            server.join().unwrap();
        }

        #[test]
        fn allows_plaintext_http_when_opted_in() {
            // Build the resolver, request http://; this should
            // pass the scheme + host checks (would attempt a
            // network call so we expect either Refused on the
            // private-IP check OR Io on the actual HTTP failure
            // — never a scheme refusal).
            let r = NetworkResolver::new(["localhost".to_string()])
                .with_plaintext_http()
                .with_private_ips_allowed();   // localhost is loopback
            // Don't actually run the network call — call check_url
            // logic indirectly by verifying no scheme rejection.
            // (Public surface only exposes resolve() which DOES
            // call out; we accept either Refused for non-scheme
            // reasons or Io.)
            let err = r.resolve(None, "http://localhost/foo.dtd", None).unwrap_err();
            // Should NOT be a scheme refusal — that's the bit we
            // want to verify the with_plaintext_http() builder
            // unblocked.
            if let ResolveError::Refused(msg) = &err {
                assert!(!msg.contains("scheme"),
                    "should not refuse on scheme when plaintext is opted in: {msg}");
            }
            // Either Io (couldn't connect) or another Refused is
            // fine — we're only checking the scheme gate.
        }
    }
}
