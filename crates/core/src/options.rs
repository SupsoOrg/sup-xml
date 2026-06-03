#![forbid(unsafe_code)]  // see CONTRIBUTING.md § "Unsafe policy"

use std::sync::Arc;

use crate::encoding::Encoding;
use crate::entity_resolver::EntityResolver;

/// Security limits and feature flags for the XML parser.
///
/// Construct with `ParseOptions::default()` for safe defaults, then override
/// individual fields as needed.  All external-loading features are **off** by
/// default to prevent XXE and SSRF attacks.  Limits mirror libxml2's defaults
/// where applicable.
///
/// # Example
/// ```
/// use sup_xml_core::{ParseOptions, parse_str};
///
/// let opts = ParseOptions {
///     max_element_depth: 64, // tighten from the default of 256
///     ..ParseOptions::default()
/// };
/// let doc = parse_str("<root/>", &opts).unwrap();
/// ```
///
/// # `Copy` removal
///
/// `ParseOptions` was `Copy` before adding `external_resolver`,
/// which holds an `Arc<dyn EntityResolver>` (not `Copy`).  The
/// derive is now `Clone` only.  In practice almost no caller
/// needs `Copy` for an options struct — construct once, clone
/// when you need a second instance.
#[derive(Debug, Clone)]
pub struct ParseOptions {
    /// Maximum total bytes produced across all entity expansions in a single
    /// document.  Prevents "billion laughs" (CVE-2003-1564) and similar
    /// amplification attacks.  Default: 1,000,000 (1 MB).
    pub max_entity_expansion_bytes: u64,
    /// Maximum element nesting depth.  Documents deeper than this limit are
    /// rejected with a fatal error.  Default: 256.
    pub max_element_depth: u32,
    /// External resource resolver.  `None` (default) = the parser
    /// refuses to load any externally-referenced DTD or entity —
    /// this is the XXE-safe stance and what every untrusted-input
    /// pipeline should keep.
    ///
    /// To opt into external loading, set this to one of:
    ///
    /// - [`FilesystemResolver`](crate::entity_resolver::FilesystemResolver) —
    ///   loads from a configured allowlist of local directories,
    ///   optionally consulting an OASIS catalog first.
    /// - `NetworkResolver` (behind the `network-resolver` feature) —
    ///   fetches over HTTPS from a configured host allowlist with
    ///   SSRF defenses.
    /// - [`ChainedResolver`](crate::entity_resolver::ChainedResolver) —
    ///   composes multiple resolvers; tries each in order.
    /// - A custom impl of the [`EntityResolver`] trait for bespoke
    ///   setups (in-memory bundles, S3, audit-logging, …).
    ///
    /// Replaces the `allow_external_entities` and `allow_external_dtd`
    /// boolean flags from earlier versions; the presence of a
    /// resolver IS the opt-in.
    pub external_resolver: Option<Arc<dyn EntityResolver>>,
    /// Use XML 1.0 4th-edition (pre-2008) character class tables — BaseChar,
    /// CombiningChar, Digit, Extender — instead of the simplified 5th-edition
    /// Unicode-category rules.  Equivalent to libxml2's `XML_PARSE_OLD10`.
    /// Default: `false` (5th edition, same as libxml2's default).
    pub xml10_fourth_edition: bool,
    /// Enforce XML Namespaces 1.0 constraints during parsing.
    ///
    /// When `true`, the parser rejects colons in PI targets, entity names, and
    /// notation names (NCName requirement).  Element and attribute QName checks
    /// are applied by [`resolve_namespaces`][crate::resolve_namespaces].
    /// Default: `false`.
    pub namespace_aware: bool,
    /// Enable DTD-based validation.
    ///
    /// When `true`, the parser collects element and attribute declarations from
    /// the internal DTD subset and verifies the document against them.  This
    /// has a small performance cost because declaration names must be interned
    /// rather than skipped.  Default: `false`.
    pub validating: bool,
    /// Skip the XML Name production validation on element / attribute / PI
    /// names.  Names are still scanned (we need to know where they end) but
    /// non-ASCII bytes are accepted without checking the Unicode-range rules
    /// that the XML 1.0 spec specifies.
    ///
    /// Trade-off: a small speed-up on non-ASCII-heavy XML, at the cost of
    /// silently accepting malformed names like `<3foo>` or names containing
    /// disallowed Unicode characters.  Use only with trusted input.
    ///
    /// Default: `false` (validate per the spec).
    pub skip_name_validation: bool,
    /// Skip end-tag-matches-start-tag verification.  Mirrors quick-xml's
    /// `check_end_names: false`.  With this flag set the parser does not
    /// maintain an element stack, does not check that `</bar>` closes the
    /// matching `<bar>`, and does not enforce the max-depth limit.
    ///
    /// Trade-off: noticeable speed-up on element-heavy XML (no per-element
    /// Vec push / pop / name compare), at the cost of accepting malformed
    /// documents like `<a><b></a></b>` silently.  Use only with trusted
    /// input — specifically, when an earlier pass has already verified
    /// structural well-formedness.
    ///
    /// Default: `false` (verify each end tag).
    pub skip_end_tag_check: bool,
    /// Skip the XML 1.0 § 2.2 Char-production validation that runs once over
    /// the whole input before parsing begins.  When `false` (the default),
    /// documents containing illegal control characters (NUL, form-feed, etc.)
    /// or U+FFFE / U+FFFF are rejected with a fatal error.
    ///
    /// Trade-off: ~5–10% speed-up by skipping one O(n) scan over the input,
    /// at the cost of accepting documents that violate XML 1.0 § 2.2.  Use
    /// only with trusted input.
    ///
    /// Default: `false` (validate per the spec).
    pub skip_xml_char_validation: bool,
    /// Internal flag used by the streaming reader wrapper
    /// ([`crate::streaming_reader::XmlByteStreamReader`]).  When `true`,
    /// the reader stores element-stack names as owned `String`s
    /// instead of byte ranges into the source buffer.  This is
    /// required when the source buffer is rolling (compacted /
    /// reallocated between events) because byte ranges captured at
    /// start-tag time would point at stale bytes by end-tag time.
    ///
    /// Costs a small allocation per start tag — only set this when
    /// running under the streaming wrapper.  Slurped callers should
    /// leave it `false` (the default) for zero-copy name storage.
    pub stream_owned_names: bool,
    /// Skip entity expansion in [`Event::Text`][crate::reader::Event::Text]
    /// payloads.  When `false` (the default), text events contain fully
    /// decoded content — `&amp;` is expanded to `&`, user-declared entities
    /// have their replacement text included, etc.  When `true`, text
    /// events carry the raw source slice with entity references *left in
    /// place* (`&amp;` stays `&amp;`), and the caller is responsible for
    /// decoding them later — typically via
    /// [`unescape`][crate::reader::unescape].
    ///
    /// Trade-off: dramatic speed-up on entity-heavy text (HTML / wiki
    /// exports, RSS, Atom — anywhere `&amp;` appears repeatedly inside
    /// `<text>` bodies) because the text-content fast path becomes a
    /// single `memchr(b'<', …)` over the entire body instead of a
    /// `memchr3(b'<', b'&', b']', …)` that stops at each entity.  Cost: the
    /// caller must call `unescape` (or do the equivalent themselves)
    /// before treating the text as decoded content.
    ///
    /// Only applies to SAX text events; DOM text nodes are always decoded.
    /// Attribute values are also always decoded — attributes are typically
    /// inspected eagerly so the gain wouldn't outweigh the API split.
    ///
    /// Default: `false` (expand per the spec).
    pub skip_entity_expansion: bool,

    /// Replace user-defined entity references (`&foo;` declared in
    /// the DTD) with their replacement text during parsing.  When
    /// `true` (the default, spec behaviour) `&foo;` is expanded
    /// inline into a `Text` node.  When `false`, a dedicated
    /// `NodeKind::EntityRef` node is emitted instead — the tree
    /// preserves the original `&foo;` source form, the serializer
    /// rewrites it verbatim, and consumers can walk the tree to
    /// see which references appear where.
    ///
    /// Mirrors `lxml.etree.XMLParser(resolve_entities=False)`.
    /// Predefined entities (`&amp;`/`&lt;`/`&gt;`/`&quot;`/`&apos;`)
    /// and numeric character references (`&#65;`/`&#x41;`) are
    /// ALWAYS expanded — those are part of the character data
    /// production, not the entity-reference machinery.
    ///
    /// Default: `true` (resolve / expand).
    pub resolve_entities: bool,

    /// If `true`, suppress `Text` events between elements when their
    /// content is *only* ASCII whitespace (spaces, tabs, CR, LF).
    ///
    /// Useful for **data-XML** workloads (SOAP envelopes, RSS / Atom,
    /// Maven POMs, configuration files, anything where indentation is
    /// purely formatting).  Lets the consumer skip the per-event work
    /// of dispatching on text-event variants only to discard the
    /// payload anyway.  Mirrors `quick-xml`'s `Reader::trim_text` and
    /// `libxml2`'s `XML_PARSE_NOBLANKS`.
    ///
    /// **Don't enable this for document-style XML** — XHTML, DocBook,
    /// any mixed-content format where the spaces between sibling
    /// elements are semantically significant.  In `<p>foo <b>bar</b></p>`,
    /// the space between "foo" and `<b>` is content; skipping it would
    /// silently corrupt rendering.
    ///
    /// Only the *leading* whitespace inside an element is suppressed —
    /// trailing whitespace inside non-whitespace text events (e.g.,
    /// `<p>foo  </p>` → `Text("foo  ")`) is preserved unchanged.  This
    /// matches the XML data model: the `Text` event is the same
    /// content the application would see; we just don't emit a
    /// separate event for the inter-element indent.
    ///
    /// Default: `false` (preserve every text event, correct per spec).
    pub skip_inter_element_whitespace: bool,

    /// Skip the eager attribute-syntax validation pass that runs on
    /// every start tag.  When `false` (the default), the parser
    /// catches all of:
    ///   - bare `<` in attribute values         (§ 3.1 WFC)
    ///   - bare `&` not part of a reference     (§ 4.1)
    ///   - unquoted values                      (§ 3.1 [41])
    ///   - invalid attribute name-start chars   (§ 2.3 [4])
    ///   - missing whitespace between attrs     (§ 3.1 [40])
    ///   - duplicate attribute names            (§ 3.1 WFC)
    ///   - undefined / cyclic / external entity references in
    ///     attribute values                     (§ 4.1 / § 4.4.4)
    /// at parse time, regardless of whether the caller iterates
    /// `BytesAttrs`.  This is what makes sup-xml's compliance
    /// score on the W3C XML Conformance Test Suite (244/257) higher
    /// than libxml2's (237/257).
    ///
    /// Trade-off: validation walks each attribute, so attribute-
    /// heavy fixtures (OSM, RSS feeds, anything with many small
    /// attrs per element) run ~30–50 % slower than they would with
    /// no validation.
    ///
    /// When `true`, the eager pass is skipped — attribute validation
    /// is deferred to `BytesAttrs::next()` and only fires for tags
    /// the caller actually iterates.  Mirrors `quick-xml`'s default
    /// behaviour (their `Attributes` iterator's `with_checks: true`
    /// is also lazy and only catches a subset).  Use only with
    /// trusted input — silently lets through every WFC the eager
    /// path catches when the caller doesn't read attributes.
    ///
    /// Default: `false` (validate per the spec).
    pub skip_attr_validation: bool,

    /// Match libxml2's accept/reject behaviour on edge-case documents
    /// even when our parser would otherwise be stricter than the spec
    /// requires libxml2 to be.  Intended for migrations from
    /// libxml2-using code where existing documents may rely on
    /// libxml2's specific implementation quirks.
    ///
    /// **What this enables:**
    ///
    /// - **External entity references treated as opaque.**  Our default
    ///   rejects `&extName;` when `extName` was declared `SYSTEM` /
    ///   `PUBLIC` and we never loaded the replacement text — the spec
    ///   calls this "undefined entity."  libxml2 silently expands to
    ///   nothing when it can't read the external file (a class of
    ///   silent-failure bug), and existing code may rely on that.
    ///
    /// **What this does NOT enable:**
    ///
    /// - External entity / DTD loading.  XXE protection stays on
    ///   regardless — set [`external_resolver`](Self::external_resolver)
    ///   to opt into external loading.
    /// - Anything else that's a security regression.  Compat mode
    ///   relaxes correctness checks; it does not weaken security.
    ///
    /// Default: `false` (strict — recommended).
    pub libxml2_compat: bool,

    /// Continue parsing past non-fatal well-formedness errors,
    /// accumulating them on the reader instead of returning the
    /// first one as a `Result::Err`.  Mirrors libxml2's
    /// `XML_PARSE_RECOVER` flag.
    ///
    /// **Two-tier error model:**
    ///
    /// - **`ErrorLevel::Fatal`** — the input is unrecoverable
    ///   (truncated mid-construct, invalid UTF-8, entity-expansion
    ///   budget exceeded, depth-limit exceeded).  Recovery cannot
    ///   help; `next()` returns `Err` even in recover mode.
    /// - **`ErrorLevel::Error`** — non-fatal well-formedness
    ///   violations (mismatched end tag, unclosed at EOF, bare `&`
    ///   in text, undefined entity, duplicate attribute names,
    ///   etc.).  In recover mode the parser logs the error to
    ///   [`XmlBytesReader::recovered_errors`] and applies a
    ///   heuristic repair so it can continue.  In strict mode
    ///   (default) these are returned as `Err`.
    /// - **`ErrorLevel::Warning`** — informational (rare).  Always
    ///   logged, never stops parsing.
    ///
    /// **Use cases:**
    ///
    /// - Web crawlers / RSS readers / feed aggregators handling
    ///   third-party XML where one bad publisher shouldn't break
    ///   the whole pipeline.
    /// - Diagnostic tools that want to show "here's the partial
    ///   tree we built, here are the problems we found."
    /// - Migration tools converting legacy malformed data into
    ///   something cleaner.
    ///
    /// **Not for adversarial input.**  The existing security limits
    /// (entity-expansion budget, depth limit) still apply — they
    /// are `ErrorLevel::Fatal` and aren't recovered from.  But
    /// recovery itself is a security-sensitive surface; treat the
    /// flag as "trusted-source-but-buggy" semantics, not "accept
    /// arbitrary input."
    ///
    /// Default: `false` (fail on the first non-trivial error).
    pub recovery_mode: bool,

    /// Auto-detect the input's character encoding and transcode to UTF-8
    /// before parsing.
    ///
    /// **On by default** — matches libxml2's behaviour and the XML 1.0
    /// spec's requirement (§ 4.3.3) that processors accept both UTF-8 and
    /// UTF-16.  With this on, [`parse_bytes`](crate::parse_bytes) accepts
    /// any encoding the [`encoding`](crate::encoding) module can detect:
    /// UTF-8, US-ASCII, ISO-8859-1, Windows-1252, UTF-16 LE/BE, UTF-32
    /// LE/BE, IBM037 EBCDIC, and (with the default `full-encodings`
    /// feature) the full WHATWG set — Shift_JIS, GBK, Big5, ISO-8859-2…16,
    /// KOI8-R, Windows-1250…1258, etc.
    ///
    /// Detection follows XML 1.0 Appendix F — BOM first, then the
    /// four-byte autodetect signatures for UTF-32 / UTF-16 / EBCDIC, then
    /// the `<?xml encoding="..."?>` declaration.
    ///
    /// For UTF-8 input this is **zero-copy** — the transcoder returns a
    /// borrow of the original bytes, so the only cost is a ~100-byte
    /// detection scan.  Non-UTF-8 input pays one allocation for the
    /// decoded buffer.
    ///
    /// Set to `false` to require the input to already be UTF-8.  Use
    /// that mode when your inputs are guaranteed-UTF-8 (you control the
    /// producer), when you want to reject non-UTF-8 input as part of a
    /// security posture, or when you want to skip the detection scan.
    ///
    /// Default: `true`.
    pub auto_transcode: bool,

    /// Force a specific input encoding, overriding auto-detection.
    /// When `Some`, the parser transcodes the input *as* this encoding
    /// regardless of any BOM or `<?xml encoding="…"?>` declaration —
    /// the behaviour of libxml2's explicit `encoding` argument to
    /// `xmlReadMemory` / `xmlCtxtReadMemory` and of `xmlSwitchEncoding`.
    /// `None` (the default) auto-detects per [`auto_transcode`](Self::auto_transcode).
    pub forced_encoding: Option<Encoding>,

    /// Load and parse the external DTD subset when a `<!DOCTYPE r
    /// SYSTEM "path.dtd">` (or `PUBLIC ... "path.dtd"`) declaration
    /// is present.  The external subset's `<!ELEMENT>` and
    /// `<!ATTLIST>` declarations are merged into
    /// [`XmlBytesReader::dtd`](crate::xml_bytes_reader::XmlBytesReader::dtd),
    /// alongside any internal-subset declarations.
    ///
    /// libxml2 calls the same feature `XML_PARSE_DTDLOAD`.  Off by
    /// default because loading arbitrary local files referenced
    /// inside a document is a security-sensitive surface (classic
    /// XXE vector) — turn it on only when you trust the input.
    ///
    /// **Scope**: when on, this enables loading for *both* the
    /// external DTD subset AND external general entities declared
    /// like `<!ENTITY x SYSTEM "file.txt">`.  The latter is the
    /// hotter XXE channel — references such as `&x;` substitute
    /// the file's contents into the parsed tree, letting a
    /// malicious document exfiltrate any file the parser process
    /// can read.  Both load behaviours share this flag because
    /// libxml2 treats them as a single switch and the entity
    /// declarations live inside the DTD anyway.  Need finer
    /// control?  Wire an [`external_resolver`](Self::external_resolver)
    /// — it's consulted first and can whitelist or deny per
    /// request.
    ///
    /// Resolution: the SYSTEM literal is treated as a filesystem
    /// path; relative paths resolve against
    /// [`base_url`](Self::base_url) when set, otherwise against the
    /// process's current working directory.  Network URIs
    /// (`http://...`) are NOT fetched — they're silently treated
    /// as a missing file and ignored.
    ///
    /// Default: `false`.
    pub load_external_dtd: bool,

    /// Whether a declared external *general* entity may be loaded (via
    /// [`external_resolver`](Self::external_resolver)) and inlined when it
    /// is referenced in content.  When `false`, the resolver is still used
    /// for the external DTD subset and parameter entities, but a reference
    /// to an external general entity is treated as undefined — the entity's
    /// content is never fetched.
    ///
    /// This is the second gate libxml2 applies: external general-entity
    /// expansion requires both a loader *and* the caller having opted in
    /// (libxml2's `XML_PARSE_NOENT` together with a non-restricting
    /// `getEntity`). lxml's default (`resolve_entities='internal'`) sets
    /// this `false` to avoid inlining attacker-controlled external content
    /// (XXE/SSRF).  Only consulted when an `external_resolver` is set.
    ///
    /// Default: `true` (the resolver's presence is the opt-in; callers that
    /// want the stricter stance set this `false`).
    pub resolve_external_entities: bool,

    /// Base path used to resolve relative SYSTEM literals during
    /// external-DTD and external-entity loading.  When `Some`, a
    /// relative SYSTEM literal is joined against this path's parent
    /// directory rather than the process's current working
    /// directory.  Has no effect when
    /// [`load_external_dtd`](Self::load_external_dtd) is `false`.
    ///
    /// Typically populated by the parse entry point with the
    /// document's source URL (e.g. `xmlReadFile`'s `filename`
    /// argument), so a file at `/data/doc.xml` referencing
    /// `<!DOCTYPE r SYSTEM "schema.dtd">` finds `/data/schema.dtd`.
    pub base_url: Option<String>,

    /// Drop comment nodes during the parse instead of building them
    /// into the tree.  Mirrors libxml2's effect when a consumer NULLs
    /// the SAX `comment` callback (lxml's `XMLParser(remove_comments=
    /// True)`).  Default: `false`.
    pub remove_comments: bool,

    /// Drop processing-instruction nodes during the parse.  Mirrors
    /// libxml2's effect when a consumer NULLs the SAX
    /// `processingInstruction` callback (lxml's
    /// `XMLParser(remove_pis=True)`).  Default: `false`.
    pub remove_pis: bool,

    /// Deliver CDATA-section content as ordinary text nodes instead of
    /// dedicated CDATA nodes (libxml2's `XML_PARSE_NOCDATA`; lxml's
    /// `XMLParser(strip_cdata=True)`, which is its default).  On
    /// serialization the content round-trips as escaped text rather
    /// than `<![CDATA[…]]>`.  Default: `false` (CDATA preserved).
    pub cdata_as_text: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            max_entity_expansion_bytes: 1_000_000,
            max_element_depth: 256,
            external_resolver: None,
            xml10_fourth_edition: false,
            namespace_aware: false,
            validating: false,
            skip_name_validation: false,
            skip_end_tag_check: false,
            skip_xml_char_validation: false,
            stream_owned_names: false,
            skip_entity_expansion: false,
            resolve_entities: true,
            skip_inter_element_whitespace: false,
            skip_attr_validation: false,
            libxml2_compat: false,
            recovery_mode: false,
            auto_transcode: true,
            forced_encoding: None,
            load_external_dtd: false,
            resolve_external_entities: true,
            base_url: None,
            remove_comments: false,
            remove_pis: false,
            cdata_as_text: false,
        }
    }
}
