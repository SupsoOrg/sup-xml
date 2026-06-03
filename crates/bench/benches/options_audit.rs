//! Audit `XML_PARSE_*` flag translation through the compat layer.
//!
//! Background: `sup-xml`'s native `ParseOptions` is a modern Rust
//! struct with named bool fields (`recovery_mode`, `load_external_dtd`,
//! `auto_transcode`, …) — it deliberately doesn't mirror libxml2's
//! bitmask shape.  The C ABI in `crates/compat` translates a
//! libxml2 `XML_PARSE_*` bitmask onto our `ParseOptions` via
//! [`sup_xml_compat::parse::map_libxml2_options`].
//!
//! That translator currently honours only `XML_PARSE_DTDLOAD`;
//! everything else is silently dropped.  Whether a given drop is
//! "OK" (caller silently gets stricter behaviour than asked) or
//! "BAD" (caller silently gets a different tree than libxml2 would
//! have given them) is per-flag — and the only way to know is to
//! actually run the probe.
//!
//! This bench drives one engineered probe document per flag through:
//!
//!   - **libxml2**: `xmlReadMemory(probe, len, NULL, NULL, FLAG)`
//!   - **sup-xml**: `parse_bytes(probe, &opts)` where `opts` came out
//!     of the *actual* `map_libxml2_options` translator.
//!
//! Each side is parsed twice — flag CLEAR and flag SET — and we
//! observe the within-library delta.  Comparing the two deltas tells
//! us the translator's coverage:
//!
//!   - **HONORED**     — both libraries change behaviour, in the
//!                        same accept/reject direction.
//!   - **IGNORED**     — libxml2 changes, sup-xml does not.  Caller
//!                        passing the flag silently gets sup-xml's
//!                        default behaviour.
//!   - **DIVERGES**    — both change, but in different directions.
//!                        Hard-to-debug class of compat bug.
//!   - **INSENSITIVE** — neither library changes.  Probe is a bench
//!                        bug; refine it.
//!
//! Run:
//!     cargo bench -p sup-xml-bench --bench options_audit

#![allow(clippy::missing_safety_doc)]

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};

use sup_xml::{parse_bytes, Node, NodeKind, ParseOptions};

// ── translator under audit ──────────────────────────────────────────────────
//
// Inlined copy of `sup_xml_compat::parse::map_libxml2_options`.  We
// can't depend on `sup-xml-compat` from the bench: that crate exports
// its own libxml2-shaped `xmlMalloc` / `xmlFree` / `xmlRealloc`
// `#[no_mangle]` symbols (so it can act as a drop-in `libxml2.so`),
// and pulling it in as an rlib here would shadow the real libxml2's
// allocator globals — leading to a SIGBUS the first time we call
// `xmlFree` on a buffer libxml2 itself allocated.
//
// Keeping this in sync with `map_libxml2_options` is the discipline
// the audit relies on.  When the translator gains a new bit, mirror
// it here — the audit's whole point is to spot when it doesn't.
const XML_PARSE_RECOVER:  c_int = 1 << 0;
const XML_PARSE_NOENT:    c_int = 1 << 1;
const XML_PARSE_DTDLOAD:  c_int = 1 << 2;
const XML_PARSE_DTDVALID: c_int = 1 << 4;
const XML_PARSE_NOBLANKS: c_int = 1 << 8;
fn translator(bitmask: c_int, opts: &mut ParseOptions) {
    if (bitmask & XML_PARSE_RECOVER)  != 0 { opts.recovery_mode                 = true; }
    if (bitmask & XML_PARSE_NOENT)    != 0 { opts.resolve_entities              = true; }
    if (bitmask & XML_PARSE_DTDLOAD)  != 0 { opts.load_external_dtd             = true; }
    if (bitmask & XML_PARSE_DTDVALID) != 0 { opts.validating                    = true; }
    if (bitmask & XML_PARSE_NOBLANKS) != 0 { opts.skip_inter_element_whitespace = true; }
}

// ── libxml2 FFI ─────────────────────────────────────────────────────────────

/// First few fields of libxml2's `_xmlNode` — enough to walk the
/// tree (kind/name/children/next) without needing the full layout.
/// Field order verified against
/// `/opt/homebrew/Cellar/libxml2/<version>/include/libxml2/libxml/tree.h`.
#[repr(C)]
struct XmlNode {
    _private: *mut c_void,
    kind:     c_int,              // xmlElementType: 1=Element, 3=Text, 4=CData, …
    name:     *const c_char,      // xmlChar*
    children: *mut XmlNode,
    last:     *mut XmlNode,
    parent:   *mut XmlNode,
    next:     *mut XmlNode,
    prev:     *mut XmlNode,
    // (remaining fields omitted — we don't read them)
}

/// libxml2 `xmlElementType` values (subset we classify).
const LX_ELEMENT_NODE: c_int = 1;
const LX_TEXT_NODE:    c_int = 3;
const LX_CDATA_NODE:   c_int = 4;
const LX_ENTITY_REF:   c_int = 5;

unsafe extern "C" {
    fn xmlReadMemory(
        buffer:   *const c_char,
        size:     c_int,
        url:      *const c_char,
        encoding: *const c_char,
        options:  c_int,
    ) -> *mut c_void;
    fn xmlFreeDoc(doc: *mut c_void);
    fn xmlDocGetRootElement(doc: *mut c_void) -> *mut XmlNode;
    fn xmlNodeGetContent(node: *const XmlNode) -> *mut u8;  // xmlChar*; leaked
    fn xmlSetGenericErrorFunc(
        ctx: *mut c_void,
        handler: Option<unsafe extern "C" fn()>,
    );
    fn xmlSetStructuredErrorFunc(
        ctx: *mut c_void,
        handler: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    );
}

unsafe extern "C" fn swallow() {}
unsafe extern "C" fn swallow_struct(_ctx: *mut c_void, _err: *const c_void) {}

fn install_silencers() {
    // libxml2 has two parallel error channels — silence both so the
    // bench output isn't interleaved with libxml2 yelling to stderr
    // on every probe.  Same pattern as `xsts_compliance.rs`.
    // SAFETY: registering thread-local handlers; no aliasing.
    unsafe {
        xmlSetGenericErrorFunc(std::ptr::null_mut(), Some(swallow));
        xmlSetStructuredErrorFunc(std::ptr::null_mut(), Some(swallow_struct));
    }
}

// ── fingerprints ────────────────────────────────────────────────────────────

/// Structural fingerprint of a parsed document.
///
/// Sensitive to the deltas the flags actually produce:
///   - `accepted`        — RECOVER, DTDVALID, HUGE flip this.
///   - `elem_count`      — XINCLUDE adds/removes element nodes.
///   - `text_node_count` — NOBLANKS strips empty text nodes.
///   - `cdata_count`     — NOCDATA merges CDATA into text (count → 0).
///   - `entityref_count` — NOENT consumes entity-ref nodes (count → 0).
///   - `concat_text`     — NOENT changes the substituted text; NOBLANKS
///                         drops whitespace from the concat.
///
/// Compared only within a library (libxml2 off vs libxml2 on; sup-xml
/// off vs sup-xml on) — cross-library equality isn't expected because
/// the two parsers split text nodes differently around comments etc.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    accepted:        bool,
    elem_count:      u32,
    text_node_count: u32,
    cdata_count:     u32,
    entityref_count: u32,
    /// Concatenated content of all text/cdata descendants of root,
    /// in document order, without entity-ref expansion (we count
    /// EntityRef nodes separately so their presence is visible
    /// even though we don't recurse through their replacement).
    concat_text:     String,
}

impl Fingerprint {
    fn reject() -> Self {
        Self { accepted: false, elem_count: 0, text_node_count: 0,
               cdata_count: 0, entityref_count: 0, concat_text: String::new() }
    }
    fn brief(&self) -> String {
        if !self.accepted { return "REJECT".into(); }
        // One-line summary: counts + a quoted preview of the text.
        // Text preview is escaped so whitespace shows up.
        let t = escape_brief(&self.concat_text);
        let t = if t.chars().count() > 20 {
            let head: String = t.chars().take(19).collect();
            format!("{head}…")
        } else { t };
        format!("E={} T={} C={} R={} \"{t}\"",
                self.elem_count, self.text_node_count,
                self.cdata_count, self.entityref_count)
    }
}

fn escape_brief(s: &str) -> String {
    s.chars().map(|c| match c {
        '\n' => "\\n".into(),
        '\t' => "\\t".into(),
        '"'  => "\\\"".into(),
        c    => c.to_string(),
    }).collect()
}

// ── libxml2 tree walker ─────────────────────────────────────────────────────

fn libxml_parse(bytes: &[u8], flag: c_int) -> Fingerprint {
    // SAFETY: read-only borrowed slice handed to a read-only C call;
    // the doc is freed via xmlFreeDoc before we return.  We walk the
    // returned tree via the partial XmlNode layout above — kind /
    // children / next live at fixed offsets at the head of every
    // libxml2 node type, which is the part of the ABI that has been
    // stable across many libxml2 releases.
    unsafe {
        let doc = xmlReadMemory(
            bytes.as_ptr() as *const c_char, bytes.len() as c_int,
            std::ptr::null(), std::ptr::null(),
            flag,
        );
        if doc.is_null() { return Fingerprint::reject(); }
        let mut fp = Fingerprint { accepted: true, ..Fingerprint::reject() };
        fp.accepted = true;
        let root = xmlDocGetRootElement(doc);
        if !root.is_null() {
            walk_libxml(root, &mut fp);
        }
        xmlFreeDoc(doc);
        fp
    }
}

unsafe fn walk_libxml(node: *const XmlNode, fp: &mut Fingerprint) {
    // SAFETY: every pointer either comes from libxml2 (valid until
    // xmlFreeDoc) or is null-checked before deref.
    if node.is_null() { return; }
    let n = unsafe { &*node };
    match n.kind {
        LX_ELEMENT_NODE => { fp.elem_count += 1; }
        LX_TEXT_NODE    => { fp.text_node_count += 1; unsafe { append_text(node, fp); } }
        LX_CDATA_NODE   => { fp.cdata_count     += 1; unsafe { append_text(node, fp); } }
        LX_ENTITY_REF   => { fp.entityref_count += 1; }
        _ => {}
    }
    let mut child = n.children;
    while !child.is_null() {
        unsafe { walk_libxml(child, fp); }
        child = unsafe { (*child).next };
    }
}

unsafe fn append_text(node: *const XmlNode, fp: &mut Fingerprint) {
    // xmlNodeGetContent returns a malloc'd xmlChar*.  libxml2's
    // `xmlFree` is a function-pointer global, not a function — calling
    // it as a plain extern can segfault — so we intentionally leak
    // the few-byte allocations.  Matches the policy used by
    // `libxml2_recovery_inspector.rs`.
    let buf = unsafe { xmlNodeGetContent(node) };
    if buf.is_null() { return; }
    let cstr = unsafe { CStr::from_ptr(buf as *const c_char) };
    if let Ok(s) = cstr.to_str() {
        fp.concat_text.push_str(s);
    }
}

// ── sup-xml tree walker ─────────────────────────────────────────────────────

fn supxml_parse(bytes: &[u8], flag: c_int) -> Fingerprint {
    let mut opts = ParseOptions::default();
    // The function under audit — every "ignored" verdict below is a
    // bit this translator drops on the floor.  See the comment near
    // the `translator` definition for why it's inlined.
    translator(flag, &mut opts);
    match parse_bytes(bytes, &opts) {
        Ok(doc) => {
            let mut fp = Fingerprint { accepted: true, ..Fingerprint::reject() };
            fp.accepted = true;
            walk_supxml(doc.root(), &mut fp);
            fp
        }
        Err(_) => Fingerprint::reject(),
    }
}

fn walk_supxml<'a>(node: &'a Node<'a>, fp: &mut Fingerprint) {
    match node.kind {
        NodeKind::Element   => { fp.elem_count += 1; }
        NodeKind::Text      => { fp.text_node_count += 1; fp.concat_text.push_str(node.content()); }
        NodeKind::CData     => { fp.cdata_count     += 1; fp.concat_text.push_str(node.content()); }
        NodeKind::EntityRef => { fp.entityref_count += 1; }
        _ => {}
    }
    for c in node.children() {
        walk_supxml(c, fp);
    }
}

// ── flag corpus ─────────────────────────────────────────────────────────────

/// Each entry: the libxml2 flag, a one-line description of what it
/// is supposed to do, and a probe document engineered so that
/// turning the flag on changes the parser's behaviour in a way the
/// fingerprint above can detect.  A probe that doesn't make
/// libxml2's fingerprint move under the flag is a bench bug — the
/// trailing "INSENSITIVE" verdict catches that.
struct FlagCase {
    name:    &'static str,
    bit:     c_int,
    intent:  &'static str,
    probe:   &'static [u8],
}

const FLAGS: &[FlagCase] = &[
    FlagCase {
        name:   "XML_PARSE_RECOVER",
        bit:    1 << 0,
        intent: "accept malformed input and return what could be parsed",
        // Mismatched end tag — strict mode rejects, recovery accepts.
        probe:  b"<r><a></r>",
    },
    FlagCase {
        name:   "XML_PARSE_NOENT",
        bit:    1 << 1,
        intent: "substitute entity references with their replacement text",
        // Internal entity declared in the DOCTYPE.  With NOENT off,
        // libxml2 leaves an entity-ref node in the tree; with it on,
        // the reference is expanded to a text node with "hello".
        // sup-xml's default already expands (resolve_entities=true),
        // so this case may show the *defaults*-side divergence too.
        probe:  b"<!DOCTYPE r [<!ENTITY x \"hello\">]><r>&x;</r>",
    },
    FlagCase {
        name:   "XML_PARSE_DTDLOAD",
        bit:    1 << 2,
        intent: "load the external DTD subset",
        // References an external DTD that doesn't exist on disk —
        // with the flag off, libxml2 doesn't try to load it (parse
        // OK); with the flag on, it tries to load and fails.
        probe:  b"<!DOCTYPE r SYSTEM \"nope.dtd\"><r/>",
    },
    FlagCase {
        name:   "XML_PARSE_DTDVALID",
        bit:    1 << 4,
        intent: "validate document against its DTD",
        // Internal DTD declares <r> as EMPTY, but the doc puts a
        // <junk/> child inside.  Without DTDVALID, libxml2 accepts;
        // with it, libxml2 still returns a doc but emits validation
        // errors — caught indirectly via element_count if it differs.
        probe:  b"<!DOCTYPE r [<!ELEMENT r EMPTY>]><r><junk/></r>",
    },
    FlagCase {
        name:   "XML_PARSE_NOBLANKS",
        bit:    1 << 8,
        intent: "strip ignorable whitespace text nodes from the tree",
        probe:  b"<r>\n  <a/>\n  <a/>\n</r>",
    },
    FlagCase {
        name:   "XML_PARSE_NOCDATA",
        bit:    1 << 14,
        intent: "merge CDATA sections into regular text nodes",
        // With the flag off, libxml2 keeps a CData node; with it on,
        // CDATA is parsed straight into a Text node.  Fingerprint
        // distinguishes cdata_count vs text_node_count.
        probe:  b"<r><![CDATA[hi]]></r>",
    },
    FlagCase {
        name:   "XML_PARSE_NSCLEAN",
        bit:    1 << 13,
        intent: "drop redundant namespace declarations from the tree",
        // The redundant xmlns on <a> doesn't change tree shape from
        // our fingerprint's perspective (it's a namespace, not a
        // node).  Probably reports INSENSITIVE under this fingerprint —
        // requires ns-decl walking to verify, which is a bigger
        // FFI lift; documented limitation.
        probe:  b"<r xmlns=\"u:x\"><a xmlns=\"u:x\"/></r>",
    },
    FlagCase {
        name:   "XML_PARSE_XINCLUDE",
        bit:    1 << 10,
        intent: "process XInclude directives during parse",
        probe:  b"<r xmlns:xi=\"http://www.w3.org/2001/XInclude\"><xi:include href=\"nope.xml\"/></r>",
    },
];

// ── defaults-divergence probes ──────────────────────────────────────────────
//
// Independent of any XML_PARSE_* bit: do sup-xml and libxml2 produce
// the same tree when both are called with their default options
// (`parse_bytes(_, default())` and `xmlReadMemory(_, _, _, _, 0)`)?
//
// Disagreement here is a *defaults divergence*: not a translator gap
// the bench above can flag, but still a place where a C caller doing
// `xmlReadMemory(.., 0)` and walking children sees a different tree
// shape on our cdylib than on real libxml2.  Some of these are
// modern-API design choices we should keep; others might be silent
// behavioural drift worth aligning.

struct DefaultsProbe {
    name:    &'static str,
    /// One-line description of what the probe is testing.
    intent:  &'static str,
    bytes:   &'static [u8],
}

const DEFAULTS: &[DefaultsProbe] = &[
    DefaultsProbe {
        name:   "utf-8 BOM",
        intent: "is the U+FEFF BOM kept in the tree or silently stripped?",
        // BOM bytes followed by `<r/>`.
        bytes:  b"\xEF\xBB\xBF<r/>",
    },
    DefaultsProbe {
        name:   "PI before root",
        intent: "does a processing instruction before <root> survive as a doc-child?",
        bytes:  b"<?xml version=\"1.0\"?><?php hello?><r/>",
    },
    DefaultsProbe {
        name:   "comment before root",
        intent: "does a comment before <root> survive as a doc-child?",
        bytes:  b"<!-- hi --><r/>",
    },
    DefaultsProbe {
        name:   "comment after root",
        intent: "does a comment after </root> survive as a doc-child?",
        bytes:  b"<r/><!-- bye -->",
    },
    DefaultsProbe {
        name:   "whitespace between PIs",
        intent: "whitespace text nodes at document level: preserved or dropped?",
        bytes:  b"<?php a?>\n<?php b?>\n<r/>",
    },
    DefaultsProbe {
        name:   "attribute defaulting (DTD)",
        intent: "DTD declares default; tree should carry the attr without an explicit value",
        // Tests XML_PARSE_DTDATTR-equivalent default behaviour.
        bytes:  b"<!DOCTYPE r [<!ATTLIST r a CDATA \"default\">]><r/>",
    },
    DefaultsProbe {
        name:   "predefined entity",
        intent: "&amp; in text — both should expand to literal '&' character",
        bytes:  b"<r>tom &amp; jerry</r>",
    },
    DefaultsProbe {
        name:   "numeric char ref",
        intent: "&#65; should yield the literal character 'A' in both",
        bytes:  b"<r>&#65;</r>",
    },
    DefaultsProbe {
        name:   "xml 1.1 declaration",
        intent: "<?xml version=\"1.1\"?> — accepted by both?",
        bytes:  b"<?xml version=\"1.1\"?><r/>",
    },
    DefaultsProbe {
        name:   "namespace inheritance",
        intent: "child without xmlns inherits parent's default namespace — element count parity",
        bytes:  b"<r xmlns=\"u:x\"><a><b/></a></r>",
    },
];

// ── presentation ────────────────────────────────────────────────────────────

fn show_bytes(bytes: &[u8]) -> String {
    if bytes.len() > 80 {
        return format!("({} bytes, suppressed)", bytes.len());
    }
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\n' => out.push_str("\\n"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02X}")),
        }
    }
    out
}

fn main() {
    install_silencers();

    println!();
    println!("XML_PARSE_* translation audit");
    println!("=============================");
    println!();
    println!("Compares within-library behaviour under flag CLEAR vs SET.  A flag");
    println!("is HONORED if libxml2 and sup-xml both react; IGNORED if only");
    println!("libxml2 reacts (translator drops the bit); DIVERGES if they react");
    println!("differently; INSENSITIVE if neither reacts (bench bug — refine");
    println!("the probe).");
    println!();

    let mut honored            = 0u32;
    let mut honored_by_default = 0u32;
    let mut ignored            = 0u32;
    let mut diverges           = 0u32;
    let mut insensitive        = 0u32;
    let mut defaults_div       = 0u32;
    let mut translator_ignored:  Vec<&str> = Vec::new();
    let mut defaults_divergence: Vec<&str> = Vec::new();

    for case in FLAGS {
        let bit_n = (case.bit as u32).trailing_zeros();
        println!("── {} (1 << {})", case.name, bit_n);
        println!("   intent: {}", case.intent);
        println!("   probe:  {}", show_bytes(case.probe));

        let lx_off = libxml_parse(case.probe, 0);
        let lx_on  = libxml_parse(case.probe, case.bit);
        let sx_off = supxml_parse(case.probe, 0);
        let sx_on  = supxml_parse(case.probe, case.bit);

        let lx_delta = lx_off != lx_on;
        let sx_delta = sx_off != sx_on;

        let verdict: &str = match (lx_delta, sx_delta) {
            (false, false) => {
                insensitive += 1;
                "INSENSITIVE — neither library's output changed; refine the probe"
            }
            (true, false) => {
                // libxml2 reacted, sup-xml didn't.  Two sub-cases:
                //  (a) sup-xml's default already produces what
                //      libxml2's flag-ON behaviour produces — the
                //      flag is effectively HONORED, just trivially
                //      (sx_off equals lx_on at this fingerprint
                //      resolution).  The translator wiring still
                //      runs; it's just a no-op because the option
                //      was already at the requested value.
                //  (b) sup-xml's default doesn't match either side
                //      of libxml2's behaviour — the translator
                //      really did drop the bit.
                if sx_off == lx_on {
                    honored_by_default += 1;
                    "HONORED-BY-DEFAULT — sup-xml's default already matches libxml2 with the flag set"
                } else {
                    ignored += 1;
                    translator_ignored.push(case.name);
                    "IGNORED — libxml2 changed, sup-xml didn't (translator drops this bit)"
                }
            }
            (false, true) => {
                diverges += 1;
                "DIVERGES — sup-xml changed, libxml2 didn't (translator is over-reaching)"
            }
            (true, true) => {
                // Both reacted; the cheapest cross-library parity
                // check is accept/reject.  Tree shape comparison is
                // unreliable because the two serialisers differ.
                if lx_off.accepted == sx_off.accepted && lx_on.accepted == sx_on.accepted {
                    honored += 1;
                    "HONORED — both react; accept/reject parity matches"
                } else {
                    diverges += 1;
                    "DIVERGES — both react but accept/reject differs"
                }
            }
        };

        println!();
        println!("   libxml2 off: {}", lx_off.brief());
        println!("   libxml2 on:  {}", lx_on.brief());
        println!("   sup-xml off: {}", sx_off.brief());
        println!("   sup-xml on:  {}", sx_on.brief());

        // Defaults divergence: even with both flags clear, the two
        // libraries can disagree because their built-in defaults
        // differ.  NOENT is the canonical example — sup-xml's
        // `resolve_entities=true` default already does what
        // libxml2 needs the NOENT flag to enable.
        if lx_off != sx_off {
            println!("   [note] defaults divergence: sup-xml and libxml2 disagree even with flag clear");
            defaults_div += 1;
            defaults_divergence.push(case.name);
        }
        println!();
        println!("   verdict: {verdict}");
        println!();
    }

    // ── summary ────────────────────────────────────────────────────────────

    let n = FLAGS.len() as u32;
    println!("Summary across {n} flags");
    println!("------------------------");
    println!("  HONORED:             {honored}");
    println!("  HONORED-BY-DEFAULT:  {honored_by_default}  (sup-xml default already ≡ libxml2-on)");
    println!("  IGNORED:             {ignored}");
    println!("  DIVERGES:            {diverges}");
    println!("  INSENSITIVE:         {insensitive}  (probe needs refining; not a real gap)");
    println!("  defaults divergence: {defaults_div}  (overlaps with the rows above)");
    println!();
    if !translator_ignored.is_empty() {
        println!("Flags the compat translator currently drops:");
        for name in &translator_ignored {
            println!("  - {name}");
        }
        println!();
        println!("Each represents a C caller whose `xmlReadMemory(.., FLAG)` call gets");
        println!("sup-xml's default behaviour instead of what they asked for.  Wiring");
        println!("a flag is usually a one-line addition to `map_libxml2_options`,");
        println!("conditional on the underlying `ParseOptions` field existing.");
        println!();
    }
    if !defaults_divergence.is_empty() {
        println!("Flags where sup-xml and libxml2 *defaults* already disagree:");
        for name in &defaults_divergence {
            println!("  - {name}");
        }
        println!();
        println!("This is independent of the translator — it means even a C caller");
        println!("passing options=0 sees a different tree shape on sup-xml than on");
        println!("libxml2.  Decide per-flag whether to (a) match libxml2's default");
        println!("(safer for drop-in compat) or (b) keep sup-xml's choice (modern");
        println!("API design) and document the divergence.");
    }

    // ── Defaults divergence section ───────────────────────────────────────
    //
    // Independent of flags: run a curated set of probes through both
    // libraries with options=0 / ParseOptions::default() and just
    // observe whether the fingerprints agree.
    println!();
    println!();
    println!("Defaults-divergence sweep");
    println!("=========================");
    println!();
    println!("Same probe through `xmlReadMemory(.., 0)` and `parse_bytes(_, default())`.");
    println!("MATCH means both libraries produce equivalent fingerprints;");
    println!("DIVERGE means a C caller using defaults sees a different tree.");
    println!();

    let mut defaults_match    = 0u32;
    let mut defaults_diverge  = 0u32;
    let mut defaults_rejects  = 0u32;  // both reject (probably bench bug)
    let mut diverged: Vec<&str> = Vec::new();

    for probe in DEFAULTS {
        println!("── {}", probe.name);
        println!("   intent: {}", probe.intent);
        println!("   probe:  {}", show_bytes(probe.bytes));
        let lx = libxml_parse(probe.bytes, 0);
        let sx = supxml_parse(probe.bytes, 0);
        println!("   libxml2: {}", lx.brief());
        println!("   sup-xml: {}", sx.brief());
        let verdict = if !lx.accepted && !sx.accepted {
            defaults_rejects += 1;
            "BOTH REJECT — probe may be a bench bug"
        } else if lx == sx {
            defaults_match += 1;
            "MATCH"
        } else {
            defaults_diverge += 1;
            diverged.push(probe.name);
            "DIVERGE — defaults produce different trees"
        };
        println!("   verdict: {verdict}");
        println!();
    }

    let dn = DEFAULTS.len() as u32;
    println!("Summary across {dn} defaults probes");
    println!("-----------------------------------");
    println!("  MATCH:       {defaults_match}");
    println!("  DIVERGE:     {defaults_diverge}");
    println!("  BOTH REJECT: {defaults_rejects}");
    println!();
    if !diverged.is_empty() {
        println!("Probes where defaults diverge:");
        for n in &diverged { println!("  - {n}"); }
        println!();
        println!("For each: decide whether to match libxml2 (drop-in compat) or");
        println!("keep our choice and document the divergence in a public-facing");
        println!("compatibility table.");
    }
}
