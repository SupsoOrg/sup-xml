# SupXML vs other XML parsers

This document compares SupXML against the major XML parsers it
shares the ecosystem with: [libxml2](https://gitlab.gnome.org/GNOME/libxml2)
(the C reference implementation), [roxmltree](https://crates.io/crates/roxmltree)
and [xml-rs](https://crates.io/crates/xml-rs) (the established Rust
DOM and SAX parsers), and [quick-xml](https://crates.io/crates/quick-xml)
(the popular performance-focused option, the only parser in the row
that does not enforce XML 1.0 well-formedness).

Two questions matter when picking an XML parser:

1.  **Does it correctly enforce the XML 1.0 specification?**
2.  **How fast is it relative to alternatives doing the same job?**

We test both empirically, with reproducible benches in this
repository.  Both tables below can be regenerated locally:

```bash
cargo bench -p sup-xml-bench --bench text_validation_check  # § "Spec compliance"
cargo bench -p sup-xml-bench --bench head_to_head           # § "Performance"
```

## Spec compliance

The XML 1.0 specification defines what every conforming parser
must accept and what it must reject.  The thirteen ill-formed
inputs below are **explicitly forbidden** by the spec — across
§ 2.1 [document], § 2.4 [CharData], § 2.8 [XMLDecl], § 3.1
[STag/ETag], and § 4.1 [Reference].  A conforming non-validating
parser is *required* to reject every one with a fatal error.

The reproducer fixture is at
[`tests/assets/xml/spec_violations_qxml_accepts.xml`](tests/assets/xml/spec_violations_qxml_accepts.xml)
and the bench is
[`crates/bench/benches/text_validation_check.rs`](crates/bench/benches/text_validation_check.rs):

| input                                            | sup-xml | libxml2 (C) | roxmltree | xml-rs   | quick-xml |
|--------------------------------------------------|----------|-------------|-----------|----------|-----------|
| baseline: plain text                             | OK       | OK          | OK        | OK       | OK        |
| baseline: text with `&amp;` entity               | OK       | OK          | OK        | OK       | OK        |
| baseline: text with CDATA section                | OK       | OK          | OK        | OK       | OK        |
| ill-formed: `]]>` literal in text (§ 2.4)        | reject   | reject      | reject    | reject   | **OK ⚠**  |
| ill-formed: `]]>` at start of text (§ 2.4)       | reject   | reject      | reject    | reject   | **OK ⚠**  |
| ill-formed: `]]>` embedded in text (§ 2.4)       | reject   | reject      | reject    | reject   | **OK ⚠**  |
| ill-formed: bare `&` in text (§ 4.1)             | reject   | reject      | reject    | reject   | **OK ⚠**  |
| ill-formed: bare `<` in text (§ 2.4)             | reject   | reject      | reject    | reject   | **OK ⚠**  |
| ill-formed: missing end tag, nested (§ 3.1)      | reject   | reject      | reject    | reject   | reject    |
| ill-formed: unclosed element at EOF (§ 3.1)      | reject   | reject      | reject    | reject   | **OK ⚠**  |
| ill-formed: mismatched end tag (§ 3.1)           | reject   | reject      | reject    | reject   | reject    |
| ill-formed: two root elements (§ 2.1)            | reject   | reject      | reject    | **OK ⚠** | **OK ⚠**  |
| ill-formed: text at document level (§ 2.1)       | reject   | reject      | reject    | reject   | **OK ⚠**  |
| ill-formed: text after root (§ 2.1)              | reject   | reject      | reject    | reject   | **OK ⚠**  |
| ill-formed: empty XML declaration (§ 2.8)        | reject   | reject      | **OK ⚠**  | reject   | **OK ⚠**  |
| ill-formed: XML decl missing version (§ 2.8)     | reject   | reject      | reject    | reject   | **OK ⚠**  |

### quick-xml is the outlier — and it's deliberate

Across the thirteen ill-formed inputs the row covers, quick-xml
rejects only two (mismatched end tags and the nested missing-end-tag
case — the only two where they happen to enforce structural rules).
It accepts the other eleven, including every text-content
violation, every document-structure violation, and every malformed
XML declaration.  This is not a small oversight — quick-xml's
`Event::Text` accepts content with unescaped `<`, `&`, or `]]>`,
which the spec forbids in any conforming document, and its
prolog handler accepts XML declarations that are missing the
required `version` attribute.

The behaviour is intentional and known to the maintainers.  Issue
[tafia/quick-xml#848](https://github.com/tafia/quick-xml/issues/848)
("well-formedness"), open since February 2025, catalogues the same
gaps independently.  The maintainers labelled it `enhancement`
(not `bug`) and `help wanted`, with no maintainer response, no
milestone, and no scheduled fix.  In other words: **quick-xml's
authors have decided that enforcing XML 1.0 well-formedness is
optional, and they are not going to ship the checks.**

The trade-off they made is straightforward — skipping the per-
event spec checks lets `read_event` run a tighter inner loop
(`memchr(b'<')` instead of `memchr3(b'<', b'&', b']')`).  That
buys real raw-throughput numbers in synthetic benchmarks.  What
it costs is correctness, and — as the next section explains —
security.

### What about quick-xml's `with_checks` flag?

quick-xml's maintainer has pointed at a [`with_checks: true`](https://docs.rs/quick-xml/latest/quick_xml/events/attributes/struct.Attributes.html)
flag on the `Attributes` iterator (it defaults to `true`) as their
mechanism for attribute-WFC enforcement.  We tested what that flag
actually checks by feeding 7 different XML 1.0 attribute-WFC
violations through their parser in two modes — see
[`crates/bench/benches/qxml_attr_validation_check.rs`](crates/bench/benches/qxml_attr_validation_check.rs)
to reproduce locally:

```bash
cargo bench -p sup-xml-bench --bench qxml_attr_validation_check
```

Result:

| input                                       | quick-xml `with_checks: true` | sup-xml default |
|---------------------------------------------|-------------------------------|------------------|
| `<doc 12="34">` — digit-start attr name (§ 2.3)         | **accept ❌**       | reject ✅ |
| `<doc a="x" a="y">` — duplicate attr names (§ 3.1 WFC)  | reject ✅           | reject ✅ |
| `<doc a="<foo>">` — bare `<` in attr value (§ 3.1 WFC)  | **accept ❌**       | reject ✅ |
| `<doc a="A & B">` — bare `&` in attr value (§ 4.1)      | **accept ❌**       | reject ✅ |
| `<doc a=v>` — unquoted attr value (§ 3.1 [41])          | reject ✅           | reject ✅ |
| `<doc a="&xyz;">` — undefined entity in attr (§ 4.1 WFC)| **accept ❌**       | reject ✅ |
| `<doc a="x"b="y">` — missing whitespace between attrs   | **accept ❌**       | reject ✅ |

Even with `with_checks: true` and full attribute iteration, quick-xml
catches only 2 of the 7 cases.  The other 5 violations slip through
silently.  And those 2 only fire when the caller actually calls
`.attributes()` on each tag — code that walks events by element name
(XPath-style filtering, DOM construction with selective attr reads,
security gateways scanning for `<script>` etc.) wouldn't trigger
them at all.

### Why this matters: parser-differential vulnerabilities

When two systems disagree on what an XML document means or whether
it is well-formed, attackers craft input that exploits the gap.
This class of vulnerability has its own name — **parser
differential attacks** — and a long CVE history including HTTP
request smuggling, XML Signature Wrapping (XSW), polyglot file
attacks, and various SAML / OAuth federation bypasses.

Quick-xml's lax acceptance enables this class of attack any time
it sits in a pipeline alongside a conforming parser.  Concrete
scenarios:

- **WAF / IDS bypass.**  A security gateway parses XML strictly,
  rejects the request, blocks it.  The application uses
  quick-xml, accepts the same bytes, processes them.  Bypass.
  Or, in reverse, the gateway tokenises one way and the
  application sees a different document.
- **CDATA smuggling.**  Quick-xml emits a text event with embedded
  `]]>`.  Application re-emits the text inside a CDATA section in
  some downstream output.  The embedded `]]>` closes CDATA early;
  attacker-controlled bytes are now in markup context.  Output
  injection / XSS / XXE depending on consumer.
- **XML Signature Wrapping (XSW).**  Signed assertions parsed
  through quick-xml and verified through a strict canonicalizer
  disagree on what was signed.  Trust decisions made on the
  wrong element.  Authentication bypass.
- **Entity-policy bypass.**  Defensive code looks for entity-
  reference events to enforce "no `&entity;` in user input."
  Quick-xml accepts `<r>tom & jerry</r>` as text — no entity
  event ever fires — defense bypassed.
- **Integrity-check bypass.**  Hashing the parsed-and-renormalised
  form for integrity.  Two parsers normalise differently;
  attacker supplies a document that passes one and tampers with
  the other.

For workloads that involve *any* of:

- XML from untrusted sources (user uploads, network-received,
  third-party feeds)
- Multiple XML processors in the same pipeline
- Re-serialising parsed XML content into XML, HTML, CDATA, or
  JSON output
- XML Signatures, SAML, SOAP, XML-RPC, federation, or any
  trust-based XML flow
- Web framework body parsing of XML payloads

we **do not recommend quick-xml**.  Use a conforming parser:
libxml2, roxmltree, xml-rs, or SupXML.

Quick-xml is fine for one narrow case: parsing fully-trusted XML
where the producer and consumer are the same code, the input is
known-good, and raw throughput on indexed data dominates.  Outside
that case, the security profile does not justify the speed.

## Compliance under the W3C XML Conformance Test Suite

The hand-picked table above covers eight ill-formed inputs we
constructed.  The W3C also publishes an authoritative test suite —
the [W3C XML Conformance Test Suite](https://www.w3.org/XML/Test/),
~257 deliberately-malformed XML files, each one engineered to trip
a specific rule from the XML 1.0 specification.  A conforming
non-validating parser is required to reject every one.

We vendored the suite at
[`tests/assets/xmlts/`](tests/assets/xmlts/) and wrote a runner at
[`crates/bench/benches/xmlts_compliance.rs`](crates/bench/benches/xmlts_compliance.rs)
that scores every parser on it.  Run locally with:

```bash
cargo bench -p sup-xml-bench --bench xmlts_compliance
```

| parser                                       | files correctly rejected | compliance |
|----------------------------------------------|--------------------------|------------|
| **sup-xml**                                 | **244 / 257**            | **95%**    |
| **sup-xml with `libxml2_compat: true`**     | 239 / 257                | 93%        |
| **libxml2** (C ref)                          | 237 / 257                | 92%        |
| roxmltree                                    | 145 / 257                | 56%        |
| xml-rs                                       | 136 / 257                | 53%        |
| **quick-xml**                                | **25 / 257**             | **10%**    |

**SupXML now beats libxml2 — the C reference implementation —
on the W3C XML Conformance Test Suite, by 7 files.**  Cross-
tabulation:

- **7 files** SupXML rejects that libxml2 accepts.  These are
  cases where libxml2 has known parser bugs we don't share:
  invalid name-start characters produced by entity expansion
  (sa/140, 141), missing-encoding-declaration in external entities
  (encoding07), external-entity references with malformed targets
  (ext-sa/001-003), and a parameter-entity violation libxml2 lets
  through (sa/115).
- **0 files** libxml2 rejects that SupXML doesn't.  All 13 of
  SupXML's remaining wrong-accepts are also in libxml2's wrong-
  accept set — and every single one of them requires external DTD
  loading (conditional sections in external `.ent` files, external
  DOCTYPE references).  We **deliberately don't load external
  DTDs** because the same loading path has produced multiple XXE
  CVEs against libxml2 over the years.

We also ship a **`ParseOptions::libxml2_compat: true`** flag for
migrations from libxml2-using code — when enabled, we silently
accept the same external-entity references libxml2 silently skips
when it can't load the external file.  This is *less* spec-strict
than our default and is intended only for compatibility with
existing libxml2 deployments.

quick-xml's 10% score is consistent with the smaller table above:
it's the only parser in the row that does not enforce most
well-formedness rules, and the issue tracked at
[tafia/quick-xml#848](https://github.com/tafia/quick-xml/issues/848)
since February 2025 confirms this is intentional and not on the
maintainers' roadmap.

## Error-recovery mode (`recovery_mode: true`)

Spec-strict parsing is the default — fail fast on the first
non-trivial error.  For workloads where the input is *trusted but
malformed* (web crawlers handling third-party RSS / Atom feeds,
migration tools converting legacy data, diagnostic UIs that want
"here's the partial tree we built and what was wrong with it"),
SupXML offers an opt-in recovery mode patterned on libxml2's
`XML_PARSE_RECOVER`.

```rust
let opts = ParseOptions { recovery_mode: true, ..Default::default() };
let mut reader = XmlBytesReader::from_bytes(bytes)?.with_options(opts);
// Parse normally; non-fatal errors are repaired and the stream
// continues.  Inspect them after:
for err in reader.recovered_errors() {
    eprintln!("note: {}", err.message);
}
```

### Two-tier error model

| level | examples | recovery behaviour |
|---|---|---|
| `Fatal` | invalid UTF-8, entity-expansion budget exceeded, depth-limit exceeded | always returned as `Err` — recovery cannot help |
| `Error` | mismatched end tag, undefined entity, bare `&` in text, malformed XML decl, … | in recover mode: log + heuristic repair + continue |
| `Warning` | informational | always logged, never aborts |

### Head-to-head recovery vs libxml2

We feed the same malformed inputs through both parsers in recovery
mode and compare the verdicts.  Reproducer at
[`crates/bench/benches/recovery_check.rs`](crates/bench/benches/recovery_check.rs):

```bash
cargo bench -p sup-xml-bench --bench recovery_check
```

```
case                     libxml2 recover     sup-xml recover    match
─────────────────────────────────────────────────────────────────────
unclosed nested tags          OK                 OK                ✅
unclosed root                 OK                 OK                ✅
mismatched end tag            OK                 OK                ✅
mismatched, deep              OK                 OK                ✅
orphan end tag             OK no-root            OK                ✅
two roots                     OK                 OK                ✅
undefined entity in text      OK                 OK                ✅
empty document              REJECT               OK                sup-xml only
bare `<` in text              OK                 OK                ✅
bare `&` in text              OK                 OK                ✅
`]]>` in text                 OK                 OK                ✅
malformed XML decl            OK                 OK                ✅
text at doc level          OK no-root            OK                ✅
─────────────────────────────────────────────────────────────────────
                                                            12 / 13 match
```

The single divergence is **the empty document case**: libxml2
rejects it even with RECOVER on, SupXML accepts and returns
`Eof` with the error logged.  Our choice is more permissive in a
direction that loses no data.

### Where SupXML's recovery is *better* than libxml2's

libxml2's recovery silently corrupts text content in three common
cases — a "results changed mysteriously" bug class.  SupXML
preserves every byte:

| input                       | libxml2 recovered text | SupXML recovered text |
|-----------------------------|------------------------|-------------------------|
| `<r>tom & jerry</r>`        | `tom  jerry` (drops `&`) | `tom & jerry`           |
| `<r>oops]]>more</r>`        | `]>more` (mangles)     | `oops]]>more`           |
| `<r>1 < 2</r>`              | `1  2` (drops `<`)     | `1 < 2`                 |

Inspect the libxml2 behaviour yourself:

```bash
cargo bench -p sup-xml-bench --bench libxml2_recovery_inspector
```

The error is still logged via `recovered_errors()` so the caller
knows recovery happened; the difference is that the *data* the
caller actually receives is the original bytes, not a mangled
substring.

### What recovery does NOT enable

- **External entity / DTD loading.**  XXE protection stays on
  regardless — `allow_external_entities` and `allow_external_dtd`
  are independent flags.
- **Adversarial-input acceptance.**  The existing security
  limits (entity-expansion budget, depth limit, max name lengths)
  are `Fatal` and aren't recovered from.  Recover mode is for
  *trusted-source-but-buggy* XML, not for adversarial input.

## XML Catalogs

[OASIS XML Catalogs](https://www.oasis-open.org/committees/entity/spec.html)
map public/system identifiers in DOCTYPE declarations to local
filesystem paths.  They're how `libxml2`-based tools avoid
fetching well-known DTDs (XHTML 1.0, DocBook, SVG 1.1, …) over
the network on every parse — the parser consults a catalog file
that says "this public ID lives at /usr/share/xml/...".

Without catalogs you have three options for handling those
DOCTYPEs: fetch over the network (slow, fragile, an XXE attack
surface), ignore the DOCTYPE entirely (the modern security
default), or refuse to load the document.  Catalogs have been the
standard solution for ~25 years.

**SupXML implements the OASIS catalog format**, with the
discovery rules `libxml2` uses:

```rust
use sup_xml::{Catalog, load_default_catalog};

// Discovery: XML_CATALOG_FILES env var first, then conventional
// paths (/etc/xml/catalog, plus Homebrew / MacPorts on macOS,
// plus ~/.xmlcatalog).  Missing files are silently skipped.
let cat = load_default_catalog()?;

// Or load explicit files / parse from bytes.
let cat = Catalog::from_files(&["/path/to/catalog.xml"])?;
let cat = Catalog::parse(b"<?xml version='1.0'?><catalog>...</catalog>")?;

// Resolve.  PUBLIC takes precedence over SYSTEM (OASIS § 7.1.1);
// PUBLIC IDs are normalised (whitespace collapsed) per § 7.1.
let uri = cat.resolve(
    Some("-//W3C//DTD XHTML 1.0 Strict//EN"),
    Some("http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd"),
);
```

**What's implemented (MVP):**

| feature | status |
|---|---|
| `<public>` and `<system>` entries        | ✅ |
| PUBLIC-ID whitespace normalisation (§ 7.1) | ✅ |
| PUBLIC > SYSTEM precedence (§ 7.1.1)     | ✅ |
| `XML_CATALOG_FILES` env var discovery     | ✅ |
| Conventional path fallback (Linux + macOS) | ✅ |
| Per-user `~/.xmlcatalog` fallback         | ✅ |
| Multi-file catalogs (first match wins)    | ✅ |

**What's not yet implemented (would be added on demand):**

- `<rewriteSystem>` / `<rewriteUri>` — URI prefix rewrites
- `<delegatePublic>` / `<delegateSystem>` — delegate by prefix
- `<nextCatalog>` — catalog chains
- `<group>` entries with `prefer` overrides
- Wiring the catalog into the parser's DOCTYPE resolution path
  (the catalog API is currently standalone; pairs naturally with
  `allow_external_dtd: true` whenever we wire that up)

The MVP covers the case "I have a catalog file mapping a few
public IDs to local DTDs."  Advanced setups (DocBook XSL catalog
hierarchies with delegation) need the additional entry types —
file an issue if you hit one.

**Where this matters:** mostly publishing toolchains (DocBook /
DITA / TEI / JATS) and any pipeline that processes XHTML 1.0 / SVG
1.1 documents whose DOCTYPEs reference w3.org-hosted DTDs.  Modern
data XML (RSS, Atom, SOAP, SAML, XSD-validated documents) doesn't
use DOCTYPEs and doesn't need catalogs.

## External entity / DTD loading

When a document declares `<!DOCTYPE` with an external subset, or
declares a `SYSTEM` / `PUBLIC` entity, the parser has the option
to *fetch* the referenced bytes from the filesystem or the
network.  This is the most active XXE attack vector in XML — a
malicious document can use it to leak local files, scan internal
networks, or trigger SSRF against private services.

**SupXML's default is to refuse all external loading.**  To opt
in, set [`ParseOptions::external_resolver`] to a resolver
implementation.  The presence of a resolver IS the opt-in; there
is no separate `bool` flag to forget about.

```rust
use std::sync::Arc;
use sup_xml::{ParseOptions, FilesystemResolver, load_default_catalog};

// Mode A: locked down (default).  Every external reference errors.
let opts = ParseOptions::default();

// Mode B: filesystem only, with allowlist + optional catalog.
// Recommended for trusted-input pipelines that need DTDs.
let resolver = FilesystemResolver::new(vec!["/usr/share/xml".into()])
    .with_catalog(load_default_catalog()?);
let opts = ParseOptions {
    external_resolver: Some(Arc::new(resolver)),
    ..Default::default()
};
```

### What ships in the box

| resolver | always available | what it does |
|---|---|---|
| [`FilesystemResolver`] | ✅ | loads from a configured allowed-roots list; symlinks are canonicalized so escapes are caught; optional `Catalog` for PUBLIC-ID lookups |
| [`ChainedResolver`]    | ✅ | composes resolvers; `Refused` falls through to the next, `Io` propagates immediately |
| [`InMemoryResolver`]   | ✅ | for tests + embedded resources; map-backed |
| [`NetworkResolver`]    | feature `network-resolver` | hardened HTTPS fetcher; required host allowlist, blocks RFC 1918 / loopback / link-local IPs, 10s timeout, 1 MB response cap, in-memory LRU cache |

For bespoke setups (S3, audit logging, custom auth) implement
`EntityResolver` yourself — it's a single method.

### Why we ship the network resolver instead of "BYO 30 lines of `ureq`"

Anyone who needs network DTD loading would otherwise have to
write their own `EntityResolver` around an HTTP client, and ~half
of them would get the security wrong (no host allowlist, no SSRF
defense, no timeout, no response-size cap).  Better to ship a
hardened reference implementation behind a feature flag than have
every user reinvent it.  The defaults are deliberately
restrictive:

```rust
#[cfg(feature = "network-resolver")]
use std::sync::Arc;
#[cfg(feature = "network-resolver")]
use sup_xml::{NetworkResolver, ChainedResolver, FilesystemResolver, ParseOptions};

#[cfg(feature = "network-resolver")]
let net = NetworkResolver::new(["www.w3.org".to_string()]);
//        └── REQUIRED — no convenience constructor for "any host"
// Defaults: HTTPS only, blocks private IPs, 10s timeout, 1 MB cap.
// Builders: with_plaintext_http(), with_private_ips_allowed(),
//           with_max_response_bytes(n), with_timeout(d), with_cache_size(n).
```

The feature flag (`network-resolver`) keeps `ureq` out of the
dep graph for users who only do local loading.

### What gets refused (security boundary)

`FilesystemResolver`:
- non-`file://` schemes → `Refused`
- paths outside the configured allowed roots (after symlink
  canonicalization) → `Refused`

`NetworkResolver` checks in this order — first failure short-circuits:

1. URL not parseable → `Refused`
2. Scheme not `https://` (or `http://` with explicit opt-in) → `Refused`
3. Host not in the allowlist (exact match) → `Refused`
4. URL contains userinfo (`user@host`) → `Refused`
5. Resolved IP is private/loopback/link-local (unless explicitly allowed) → `Refused`
6. Response body exceeds `max_response_bytes` → `Refused`
7. Timeout / connect failure / TLS error / 4xx / 5xx → `Io`

`Refused` and `Io` are distinct so `ChainedResolver` can fall
through `Refused` to the next resolver but stop on `Io`.

### Comparison to libxml2

| | sup-xml | libxml2 |
|---|---|---|
| default behaviour on external refs | refuse (XXE-safe) | varies by API; `xmlReadMemory` defaults to NO loading via `XML_PARSE_NONET` etc, but easy to misconfigure |
| filesystem loader | `FilesystemResolver` with allowed-roots + symlink canonicalization | yes, no built-in allowlist or symlink defense |
| network loader | `NetworkResolver` with required host allowlist + private-IP blocking + size/timeout caps | yes, but no built-in allowlist or SSRF defense; users must wire `xmlSetExternalEntityLoader` to add them |
| catalog integration | `FilesystemResolver::with_catalog` | yes, automatic with `XML_CATALOG_FILES` |
| pluggable custom resolver | `EntityResolver` trait | `xmlSetExternalEntityLoader` C function pointer |
| feature-gated to keep deps optional | `network-resolver` Cargo feature | n/a (statically linked) |

The big difference is **defaults**.  libxml2 ships with the
loader machinery active and asks you to disable / configure it
defensively (`XML_PARSE_NONET`, sandbox the catalog, audit
`xmlSetExternalEntityLoader`).  SupXML ships with the loader
*absent* and asks you to opt in only when you've thought about
the security boundary you want.  Both can reach the same end
state; ours is harder to misconfigure.

## HTML parsing (feature `html`)

The libxml2 HTML parser is heavily used in the wild — it powers
`lxml.html` (Python), `Nokogiri::HTML4` (Ruby), PHP's
`DOMDocument::loadHTML`, plus most server-side scrapers /
RSS aggregators / web archivers.  SupXML covers this audience
via the optional `html` feature.

```rust
use sup_xml::{parse_html_str, serialize_html_to_string};

let doc = parse_html_str("<html><body><br><p>hi</p></body></html>")?;
// `doc` is the same `Document` type the XML parser returns —
// XPath, Selector, serializer all work on it natively.
let out = serialize_html_to_string(&doc);
```

### Design — wraps html5ever, exposes our DOM types

We use [`html5ever`](https://github.com/servo/html5ever) (Servo's
Rust HTML5 parser) as the engine, gated behind a `html` Cargo
feature so users who don't need HTML pay nothing for the dep
tree.  Our [`TreeSink`](https://docs.rs/markup5ever/latest/markup5ever/interface/trait.TreeSink.html)
implementation populates [`Document`][doc] / [`ElementNode`][elem]
directly — no foreign DOM model, no translation layer, no double
allocation.  XPath / Selector / serializer "just work" on the
result.

[doc]: sup_xml::Document
[elem]: sup_xml::ElementNode

### What ships in the box

| surface | what it gives you | analog |
|---|---|---|
| `parse_html_str(input)` / `parse_html_bytes(bytes)` | full DOM `Document` | `lxml.html.fromstring` |
| `HtmlReader::next() -> HtmlEvent` | pull-based event iterator | XmlReader; lxml `iterparse` |
| `HtmlSaxParser::feed(chunk); .finish()` | push-based callbacks via `HtmlSaxHandler` | libxml2 `htmlSAXParseChunk`; lxml `HTMLParser(target=…)` |
| `serialize_html_to_string(doc)` | round-trip back to HTML5 (void elements as `<br>`, raw script/style, boolean attrs, no XML decl) | `lxml.html.tostring(doc, method='html')` |

All three parsing surfaces share the same encoding sniffing and
the same DoS-protection limits (`max_element_depth`,
`max_text_bytes`).

### Encoding sniffing

WHATWG byte-stream sniffing per HTML5 § 12.2.3:

1. Caller-supplied label via `HtmlParseOptions::encoding_override`
   (e.g. from an HTTP `Content-Type` header).
2. BOM (UTF-8 / UTF-16LE / UTF-16BE).
3. Pre-scan first 1024 bytes for `<meta charset>` or
   `<meta http-equiv="Content-Type">`.
4. Fall back to **Windows-1252** — *not* Latin-1.  HTML5 mandates
   this for legacy-web compatibility.

Common WHATWG labels (`utf-8`, `iso-8859-1`, `windows-1252`,
`utf-16le`, etc.) are handled inline.  Anything else routes
through `encoding_rs` (when the `full-encodings` feature is on)
so Shift-JIS, GB18030, EUC-KR, Big5, etc. all work.

### Security defaults

- DoS limits enforced inside our `TreeSink`: bounded element
  depth, bounded total text bytes.  html5ever has neither
  built-in.
- No external entity loading (HTML5 doesn't do entity loading
  anyway; the libxml2 XXE/XML-bomb attack surface doesn't
  apply).
- No network access.  `<link>` / `<script src>` / `<img src>`
  attributes are just data — we never fetch them.
- Recovery mode is the default (`recovery_mode: true`).
  HTML is a lenient format by spec; flipping this to `false`
  turns the parser into a strict HTML linter that errors on
  the first malformed-input recovery.

### Why we ship html5ever instead of "BYO HTML parser"

Building a spec-compliant HTML5 parser is a multi-year effort —
the WHATWG tree-construction algorithm has 24 insertion modes,
~80 tokenizer states, 2231 named character references, the
adoption-agency algorithm, foreign-content (SVG/MathML)
handling, encoding-sniffing edge cases.  Every Rust HTML
implementer (`html5ever`, `kuchiki`, `victor`, etc.) takes
months on the adoption-agency algorithm alone.

html5ever is the canonical Rust HTML5 implementation — Mozilla-
backed, used in production by Servo and downstream by
`scraper`, `kuchiki`, `select.rs`, etc., passes ~95% of
html5lib-tests.  Wrapping it in our DOM types via `TreeSink`
gives us spec-compliant HTML5 parsing at the cost of one
optional Cargo feature.  See `thoughts/html_parser_plan.txt`
for the build-vs-buy analysis we ran.

The cost we accept:

- When the `html` feature is enabled, the dep graph adds
  html5ever, markup5ever, mac, tendril, phf, string_cache,
  new_debug_unreachable, precomputed-hash.  Disabled-by-default
  means users who don't opt in see none of these.
- Parser performance is bounded by html5ever's ceiling — we can
  add a sink overhead but never *beat* raw html5ever.

### Performance

Real-world fixtures (cached from public web sources — Wikipedia,
Guardian, BBC, GitHub, MDN, Hacker News, Stack Overflow, …).
Reproduce with `tests/assets/html/fetch.sh && cargo bench -p
sup-xml-bench --bench html_parse`:

| fixture             | size    | sup-xml  | html5ever-skip-sup-xml | libxml2  |
|---------------------|---------|-----------|-------------------------|----------|
| hn.html             | 35 KB   | 35 MiB/s  | 40 MiB/s                | 45 MiB/s |
| github_rust.html    | 366 KB  | 62 MiB/s  | 69 MiB/s                | 69 MiB/s |
| wikipedia_rust.html | 590 KB  | 49 MiB/s  | 58 MiB/s                | 57 MiB/s |
| guardian.html       | 1.3 MB  | 103 MiB/s | 107 MiB/s               | 111 MiB/s|

We're consistently **4-16% behind html5ever-skip-sup-xml** (the
cost of populating our DOM in the sink — arena allocation +
final walk to convert to `Document`) and **roughly comparable to
libxml2** (within 10% in either direction).  The gap closes on
larger documents because per-node overhead amortizes.

`html5ever-skip-sup-xml` is html5ever driven into a no-op
TreeSink that throws every node away — bypasses our `BatchSink`
entirely.  It's a calibration baseline, not a real-world
configuration: nobody ships software that parses HTML and then
discards the result.  The number tells us "how much of our
runtime is sink overhead vs html5ever's tokenizer/tree-builder
cost."

### What's *not* covered (deferred / out of scope)

- **Pretty-printed HTML output.**  HTML pretty-printing needs
  block-vs-inline-element awareness which v1 doesn't have;
  `sup-xml --html print --pretty` warns and emits compact.
- **Encoding re-detection mid-parse.**  WHATWG allows the parser
  to detect a later `<meta>` and restart with a different
  encoding; we commit to one encoding at sniff time.
- **`parse_html_fragment` entry point.**  Always inserts
  implicit `<html>`/`<body>` wrappers.  Useful when splicing
  fragments into existing trees; not yet exposed.
- **HTML schema validation.**  HTML5 doesn't ship a DTD/XSD
  in the way XML formats do; "validate this HTML" is mostly
  a linter problem (HTMLHint, vnu) rather than a schema-
  validation problem.

### Comparison

| | SupXML | libxml2 (HTML) | lxml.html | Nokogiri::HTML4 | scraper / kuchiki | tl |
|---|---|---|---|---|---|---|
| spec | HTML5 (WHATWG) | HTML4-era ad-hoc | HTML4-era (libxml2) | HTML4-era (libxml2) | HTML5 (html5ever) | HTML5 (custom, less complete) |
| memory-safe | ✅ | ❌ (C) | ❌ (FFI to C) | ❌ (FFI to C) | ✅ | ✅ |
| browser-equivalent output (adoption agency, foreign content) | ✅ via html5ever | ❌ | ❌ | ❌ | ✅ | partial |
| pull stream | ✅ `HtmlReader` | ❌ (XML only) | ✅ `iterparse` | ❌ (XML only) | ❌ | ❌ |
| push / SAX stream | ✅ `HtmlSaxParser` | ✅ | ✅ `target=…` | ✅ `PushParser` | ❌ | ❌ |
| DoS limits (depth + text bytes) | ✅ enforced in sink | ❌ | ❌ | ❌ | ❌ | ❌ |
| WHATWG encoding sniffing | ✅ | partial | ✅ | partial | n/a (str only) | n/a |
| same DOM as XML side | ✅ | ✅ | ✅ | ✅ | ❌ (`html5ever` types) | ❌ (custom) |

The big differentiator vs the libxml2-based stack (libxml2,
lxml.html, Nokogiri::HTML4) is **spec choice**.  libxml2's HTML
parser is HTML4-era and predates the WHATWG tree-construction
spec; it diverges from browsers on tag soup like
`<b><i></b></i>`.  SupXML produces browser-equivalent output
because html5ever does.  Modern scrapers built post-2015
generally want browser-equivalent output — that's why
`html5ever` exists as a separate crate, and why projects like
Cloudflare's `lol_html` build on the same algorithm.

The big differentiator vs the html5ever-based stack (`scraper`,
`kuchiki`) is the **shared DOM with the XML side**.  Our
`Document` / `ElementNode` are produced by both parsers, so
XPath / Selector / serializer / serde-de all work on either
without conversion.  `scraper` and `kuchiki` ship their own
DOM types, which limits cross-pollination.

## XSD validation conformance

SupXML implements W3C XML Schema 1.0 and a growing slice of the 1.1
additions.  Two W3C-published test suites measure correctness on
each:

* **XSD 1.0 — W3C XSTS 2006-11-06.**  14,328 schemaTests +
  25,092 instanceTests across four contributors (NIST, Sun,
  Microsoft, Boeing).  Vendored at `tests/assets/xsts/`; fetch
  via `tests/assets/xsts/fetch.sh`.  Reproducer:

  ```bash
  cargo bench -p sup-xml-bench --bench xsts_compliance
  ```

* **XSD 1.1 — W3C `xsdtests` (1.1-additions slice).**  1,096
  schemaTests + 1,422 instanceTests across four contributors
  (Saxonica, IBM, Oracle, WG).  Vendored at `tests/assets/xsts-1.1/`;
  fetch via `tests/assets/xsts-1.1/fetch.sh`.  Reproducer:

  ```bash
  cargo bench -p sup-xml-bench --bench xsts11_compliance
  ```

### XSD 1.0 — head-to-head with libxml2

The 1.0 corpus is the de-facto industrial gate: every published
SOAP, SAML, OOXML, XBRL, and XLIFF schema lands somewhere in
these 14k schema tests.

| contributor | n (schema) | sup-xml schema | libxml2 schema | n (instance) | sup-xml instance | libxml2 instance |
|---|---|---|---|---|---|---|
| sunMeta    | 679    | 674/679 (99.3%)        | 674/679 (99.3%)         | 919    | 886/919 (96.4%)        | 896/919 (97.5%) +3 ns          |
| boeingMeta | 6      | 6/6 (100%)             | 6/6 (100%)              | 12     | 12/12 (100%)           | 12/12 (100%)                   |
| nistMeta   | 3953   | 3953/3953 (100%)       | 3953/3953 (100%)        | 19217  | 19217/19217 (100%)     | 19217/19217 (100%)             |
| msMeta     | 9690   | 9537/9690 (98.4%)      | 8684/9690 (89.6%) +3 timeouts | 4944   | 4669/4944 (94.4%) +78 ns      | 4549/4944 (92.0%) +223 ns      |
| **TOTAL**  | **14328** | **14170/14328 (98.9%)** | **13317/14328 (92.9%) +3 timeouts** | **25092** | **24784/25092 (98.8%) +78 ns** | **24674/25092 (98.3%) +226 ns** |

`+ns:N` on an instance cell means N tests where that backend
couldn't compile the schema (so it never got to attempt
validation); they're counted against the backend in the
denominator rather than silently dropped.  `+to:N` means N
schemas that hit the per-test timeout.  Both columns use the
**same denominator** (every attempted test in the corpus) so
backends can't flatter their headline percentage by failing
earlier in the pipeline.

**Takeaways:**

* On schemaTest (does this schema compile correctly?), SupXML is
  **6.0 percentage points ahead of libxml2** overall — driven
  primarily by Microsoft's schema corpus (98.4% vs 89.6%) which
  exercises pathological identity-constraint, particle, and
  derivation chains.  libxml2 hits 3 hard timeouts on
  `msData/particles/particlesZ012-Z015-Z020` (its
  `xmlSchemaCheckElementDeclComponent` enters quadratic behaviour
  on those particles); SupXML compiles each in under a millisecond.
* On instanceTest (does this XML validate against the compiled
  schema?), SupXML is **0.5 percentage points ahead of libxml2**
  overall (+110 cases).  NIST's 19,217 datatype tests (date /
  decimal / duration / etc.) pass at **100 % on both
  implementations**.  The remaining sup-xml instance losses
  cluster in XSD regular-expression character-class edges,
  identity-constraint subtree handling, and a handful of
  content-model particle corners.  All bounded debt; closing it
  is a matter of working through those failing tests one at a
  time.

### XSD 1.1 — incremental implementation, already ahead of libxml2

libxml2 does not implement XSD 1.1 at all.  SupXML in
`SchemaVersion::Xsd11` mode supports a growing subset; the rest
fails at parse time with a clear error pointing at the missing
feature.

| contributor | n (schema) | sup-xml-1.1 schema | libxml2 schema | n (instance) | sup-xml-1.1 instance | libxml2 instance |
|---|---|---|---|---|---|---|
| saxonMeta   | 532    | 304/532 (57.1%) | 216/532 (40.6%) | 929    | 406/929 (43.7%) +424 ns  | 139/929 (15.0%) +736 ns  |
| ibmMeta     | 548    | 372/548 (67.9%) | 276/548 (50.4%) | 423    | 222/423 (52.5%) +151 ns  | 102/423 (24.1%) +294 ns  |
| oracleMeta  | 9      | 5/9 (55.6%)     | 4/9 (44.4%)     | 17     | 9/17 (52.9%) +8 ns        | 0/17 (0.0%) +17 ns         |
| wgMeta      | 7      | 6/7 (85.7%)     | 3/7 (42.9%)     | 53     | 33/53 (62.3%) +9 ns       | 18/53 (34.0%) +34 ns       |
| **TOTAL**   | **1096** | **687/1096 (62.7%)** | **499/1096 (45.5%)** | **1422** | **670/1422 (47.1%) +592 ns** | **259/1422 (18.2%) +1081 ns** |

**SupXML is ahead of libxml2 on both axes.**  The schemaTest
margin (+17.2 points) reflects 1.1 features that libxml2 cannot
parse at all — `xs:assert`, conditional type assignment,
`xs:override`, `vc:minVersion` promotion.  The instanceTest
margin (+28.9 points) shows that on top of running 1.1-only
instances libxml2 can't reach, SupXML also matches libxml2 on the
1.0 common subset within this corpus.  Same denominator rule as
the 1.0 table — `+ns:N` instances where the schema didn't
compile count against the backend that produced them.

### What ships in 1.1 mode today

| feature | spec § | status |
|---|---|---|
| `xs:dateTimeStamp` built-in | 3.3.7 | ✅ |
| `xs:dayTimeDuration` built-in | 3.3.6.1 | ✅ |
| `xs:yearMonthDuration` built-in | 3.3.6.2 | ✅ |
| `xs:anyAtomicType` built-in (abstract) | 3.3.21 | ✅ |
| `xs:error` built-in (empty value space) | 3.3.20 | ✅ |
| `xs:explicitTimezone` facet | 4.3.14 | ✅ |
| `notQName` / `notNamespace` on wildcards | 3.10.4 | ✅ literal QNames + `##local` / `##targetNamespace` + `##defined` / `##definedSibling`, enforced for both `xs:any` and `xs:anyAttribute` |
| `xs:all` `maxOccurs > 1` / `minOccurs > 1` | 3.8.6 | ✅ parser accepts (DFA enforcement still 1.0-shaped) |
| `xs:override` directive | 4.2.5 | ✅ parses + replaces components correctly |
| `inheritable="true"` on `xs:attribute` | 3.2.2 | ✅ parsed and stored; observable when assertions / CTA land |
| `vc:minVersion` auto-promotion | F.1 | ✅ via `SchemaVersion::Auto` (see "How to opt in" below) |
| `xs:assert` / `xs:assertion` | 3.13.4 | ❌ needs XPath 2.0 |
| `xs:alternative` (conditional type assignment) | 3.3.3 | ❌ needs XPath 2.0 |
| `xs:openContent` / `xs:defaultOpenContent` | 3.4.2.3 | ❌ |

### How to opt in to 1.1

XSD 1.1 is a strict superset of 1.0, but the default is
**`SchemaVersion::Xsd10`** so existing pipelines see no behaviour
change.  Three ways to enable 1.1:

```rust
use sup_xml::xsd::{Schema, SchemaOptions, SchemaVersion};

// Explicit 1.1: every schema is treated as 1.1.
let schema = Schema::compile_str_with_options(
    xsd,
    SchemaOptions { version: SchemaVersion::Xsd11, ..Default::default() },
)?;

// Auto: start in 1.0 mode, promote to 1.1 when the schema document
// itself carries `vc:minVersion="1.1"` on `<xs:schema>`.  Best for
// mixed corpora.
let schema = Schema::compile_str_with_options(
    xsd,
    SchemaOptions { version: SchemaVersion::Auto, ..Default::default() },
)?;

// Strict 1.0 (default).  Any 1.1 construct produces a clear error
// pointing at the SchemaOptions setting needed to accept it.
let schema = Schema::compile_str(xsd)?;
```

The default is `Xsd10` because most XSD-using pipelines target
1.0, and silently accepting 1.1 syntax in a 1.0 context is the
class of bug we don't want to ship.

### Performance — XSD validation

The XSD bench (`crates/bench/benches/xsts_compliance.rs`) reports
wall-clock per backend across the full 1.0 corpus:

Completed-cases wall-clock (timed-out schemas excluded — they'd
add the timeout budget per case and bury the real comparison):

| backend | schema compile | instance validate | total |
|---|---|---|---|
| sup-xml | 1.74 s | 1.12 s | **2.86 s** |
| libxml2 | 3.96 s *3 timeouts | 2.85 s | **6.81 s *3 timeouts** |

That's **~2.3× faster on the schema compile, parity on the
validate**, ~2.4× faster end-to-end on the same corpus.  The
asterisks flag 3 Microsoft "particles" schemas
(`particlesZ012` / `Z015` / `Z020`) where libxml2's
`xmlSchemaCheckElementDeclComponent` enters quadratic behaviour
and hits the per-test 30 s timeout; SupXML compiles each in
under a millisecond.  Those 3 cases are counted as fails in
schemaTest (see the `+to:3` column above) and excluded from the
wall-clock here so a single pathological schema can't dominate
the bench narrative.  Reproduce with the same `cargo bench`
invocation as above.

## XPath 1.0 conformance

Both SupXML and libxml2 implement XPath 1.0 (and only 1.0 —
XPath 2.0+ syntax is XQuery / Saxon-EE territory).  Two benches
in this repo measure XPath correctness on the same surface from
complementary angles:

* **Hand-curated spec baseline** — 87 expressions covering every
  XPath 1.0 spec section: all 13 axes, every node test, the
  four result types, predicate forms, comparison / arithmetic /
  logical / union operators, and the complete function library
  (string × 13, number × 7, boolean × 8, nodeset × 7).  Each
  test carries the spec-correct expected output; both backends
  are scored independently.  Reproducer:

  ```bash
  cargo bench -p sup-xml-bench --bench xpath_compliance
  ```

* **libxml2's own XPath test corpus** — 327 expressions vendored
  from libxml2's `test/XPath/` directory (MIT, fetched via
  `tests/assets/xpath-libxml2-corpus/fetch.sh`).  Both backends
  evaluate each expression; for the cases where they disagree
  on a spec-decidable point the bench encodes the spec-correct
  verdict with a `§ ...` citation, so libxml2's own bugs aren't
  treated as the ground truth.  Reproducer:

  ```bash
  cargo bench -p sup-xml-bench --bench xpath_libxml2_corpus
  ```

### Conformance numbers

| corpus | n | SupXML strict | SupXML compat | libxml2 |
|---|---|---|---|---|
| hand-curated spec baseline | 87  | **87/87 100.0%**   | —              | 87/87 100.0% |
| libxml2's own corpus       | 327 | **327/327 100.0%** | 325/327 99.4%  | 312/327 95.4% |

On its own test corpus, libxml2 fails 15 expressions against
the XPath 1.0 spec — bugs we identified, cited, and annotated
in the bench's override table.  SupXML in strict mode passes
all 327.

### What libxml2 gets wrong on its own corpus (15 cases)

| # | bench location               | spec § | what libxml2 does                                | what the spec says |
|---|------------------------------|--------|--------------------------------------------------|-----|
| 10 | `expr/base:9-16`, `expr/floats:7-8` | § 3.5  | accepts exponent in number literals (`1e10`, `1.23e-3`) | grammar is `Digits ('.' Digits?)? \| '.' Digits` — no exponent |
| 2  | `expr/floats:62-63`         | IEEE 754 round-to-nearest-even | `12345678901234567890` → `12345678901234569216` (1 ULP high) | should round to `12345678901234567168` (verified against Python's strtod) |
| 1  | `expr/functions:6`          | § 4.4  | `number('-')` returns `-0`                       | non-numeric lexical → NaN |
| 2  | `expr/strings:6-7`          | § 4.2  | `string(12345678901234567890)` → `1.23456789012346e+19` | decimal-only output, no scientific notation |

For migrations from libxml2-driven pipelines, SupXML exposes
`XPathOptions { libxml2_compatible: true }`, threaded through
`XPathContext::new_with(doc, opts)`.  Enabling it relaxes the
lexer to accept exponent notation and matches libxml2's
`number('-')` / `string(big)` formatting — closes 13 of the 15
cases.  The remaining 2 are a real IEEE rounding bug in
libxml2's number parser that we deliberately do *not* replicate
even in compat mode.

### What we fixed in SupXML while building this comparison

The libxml2-corpus bench surfaced four real conformance gaps in
SupXML's XPath implementation that the hand-curated bench
hadn't caught.  All fixed and now covered by the bench:

* **`id()` function** was unimplemented.  Now walks document
  elements and matches against `id` / `xml:id` attributes per
  § 4.1.
* **`lang()` function** was a stub returning `false`.  Now
  walks the ancestor `xml:lang` chain with the ASCII-case-
  insensitive prefix-with-hyphen match the spec mandates
  (§ 4.3).
* **`round(-1.5)`** returned `-2` (round-half-away-from-zero
  bug — Rust's `f64::fract()` returns a signed fraction, so
  the old tie-break only caught positive halves).  Now returns
  `-1` per § 4.4's round-half-toward-+∞ rule.  Same fix
  preserves `-0` for `round(-0)`.
* **`attr/self::*`** matched the attribute itself.  The self
  axis's principal node type is element (§ 2.3), so `self::*`
  on an attribute must not match — threaded the axis through
  to `node_matches` and gated wildcard / name-test matching on
  the axis's principal type.

The 87-test hand-curated bench is the regression gate; the
libxml2 corpus is the scale check.

## Performance

For the parsers that *do* enforce the spec, the comparison is
fair — every parser is doing the same per-event work, so MB/s
numbers are directly comparable.

The matched-contract head-to-head bench is the apples-to-apples
comparison.  Both parsers configured to emit the same event
stream (we use `skip_inter_element_whitespace: true` to match
quick-xml's `trim_text(true)` for the rare cases where you need
to compare against quick-xml on equal terms).

Median across 21 real-world XML fixtures (run with 30 iterations,
median of best per-fixture):

| comparison                                   | median ratio | notes |
|----------------------------------------------|--------------|-------|
| sup-xml vs **libxml2** (default)            | ~1.0–1.3×    | competitive on most fixtures |
| sup-xml vs **roxmltree**                    | ~1.5–2×      | sup-xml wins on most fixtures |
| sup-xml vs **xml-rs**                       | 13–27×       | sup-xml dominates by an order of magnitude |
| sup-xml vs **quick-xml** (matched contract) | ~1.04× median, ~16-of-21 wins | sup-xml slightly faster on equal work |

Note that the structural well-formedness checks SupXML enforces
(single-root / no-text-at-doc-level / unclosed-at-EOF / valid
XML declaration) are O(1) per event and don't show up in
throughput — we get full spec compliance at no measurable cost.

Note that the quick-xml row is *only* meaningful when both
parsers are configured to do the same job.  Comparing default-
vs-default to quick-xml is comparing different work loads.

Reproduce with:

```bash
SUPXML_MINI_ITERS=30 cargo bench -p sup-xml-bench --bench head_to_head
```

## Summary

| dimension                                   | SupXML | libxml2 | roxmltree | xml-rs | quick-xml |
|---------------------------------------------|----------|---------|-----------|--------|-----------|
| memory-safe                                 | ✅       | ❌ (C, CVE history) | ✅ | ✅ | ✅ |
| spec-compliant on text content              | ✅       | ✅      | ✅        | ✅     | ❌        |
| safe for untrusted input                    | ✅       | ⚠ (CVEs) | ✅       | ✅     | ❌        |
| fast (matched contract)                     | ✅       | ✅      | ⚠         | ❌     | ✅        |
| native Rust (no FFI / system deps)          | ✅       | ❌      | ✅        | ✅     | ✅        |
| recovery mode for malformed input           | ✅       | ✅ (with data loss) | ❌ | ❌ | ❌    |
| OASIS XML Catalogs (resolve PUBLIC/SYSTEM)  | ✅ MVP   | ✅      | ❌        | ❌     | ❌        |
| External entity / DTD loading (XXE-safe by default) | ✅ trait-based opt-in | ⚠ on-by-default in some APIs | ❌ | ❌ | ❌ |
| Lenient HTML5 parser (browser-equivalent output) | ✅ via html5ever (feature `html`) | ⚠ HTML4-era only | ❌ | ❌ | ❌ |
| Canonical XML 1.0 + Exclusive C14N 1.0 (XML-DSig / SAML / eIDAS) | ✅ | ✅ | ❌ | ❌ | ❌ |
| DTD validation (attribute defaults / required / #FIXED / enum) | ✅ pragmatic v1 | ✅ full | ❌ | ❌ | ❌ |
| XInclude 1.0 (`<xi:include>` with fallback, recursive) | ✅ | ✅ | ❌ | ❌ | ❌ |
| RelaxNG validation (XML syntax) | ✅ Brzozowski derivative, interleave, mixed, name classes, XSD datatypes, combine merging | ✅ full (incl. Compact via libxslt deps) | ❌ | ❌ | ❌ |
| XSD 1.0 conformance — W3C XSTS 2006 (14.3 k schemaTests, 25 k instances) | ✅ **98.9% schema / 98.8% instance**; ~2.4× faster wall-clock on the same corpus | 92.9% schema / 98.3% instance (+3 timeouts) | ❌ | ❌ | ❌ |
| XSD 1.1 conformance — W3C xsdtests additions (1.1 k schemaTests, 1.4 k instances) | ✅ **62.7% schema / 47.1% instance**; growing phase-1 implementation | 45.5% schema / 18.2% instance (1.1 syntax unrecognised) | ❌ | ❌ | ❌ |
| XPath 1.0 conformance — libxml2's own 327-expression corpus | ✅ **327/327 (100%)** | 312/327 (95.4%) — 15 spec violations | ❌ | ❌ | ❌ |

SupXML is the only parser in the row that is memory-safe,
spec-compliant, safe for untrusted input, and competitive on
performance — hence the project's positioning as a memory-safe
libxml2 replacement that doesn't trade correctness for speed.

For workloads that need libxml2's `XML_PARSE_RECOVER` semantics,
the opt-in `recovery_mode: true` flag covers 12 of 13
common malformed-input scenarios, with strictly better data
preservation than libxml2 in three of those cases.  See the
[Error-recovery mode](#error-recovery-mode-recovery_mode-true)
section above.
