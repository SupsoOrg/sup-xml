//! Diagnostic: does each XML parser actually enforce XML 1.0
//! well-formedness rules for text content?  Not a benchmark — a
//! correctness probe that feeds well-formed and *deliberately
//! ill-formed* XML to every parser we link against and reports who
//! accepts vs rejects.
//!
//! Originally written to test the hypothesis that quick-xml's default
//! `read_event` runs a tighter text-event inner loop because it
//! `memchr(b'<')` instead of our `memchr3(b'<', b'&', b']')` — and
//! therefore silently accepts XML that spec-strict parsers reject.
//! Extended to compare against libxml2 (the C reference impl),
//! roxmltree (popular Rust DOM parser), and xml-rs (popular Rust SAX
//! parser) so we can see which side of the strictness divide each
//! one lives on.
//!
//! Run with:
//!     cargo bench -p sup-xml-bench --bench text_validation_check

use std::os::raw::{c_char, c_int};

use quick_xml::Reader as QxReader;
use quick_xml::events::Event as QxEvent;
use sup_xml::{BytesEvent, ParseOptions, XmlBytesReader};

use anyxml::sax::{
    Attributes as AnyAttrs, EntityResolver as AnyEntityResolver,
    ErrorHandler as AnyErrorHandler, SAXHandler as AnySAXHandler,
    XMLReader as AnyXMLReader,
};
use anyxml::sax::error::SAXParseError as AnySAXParseError;

// ── libxml2 FFI shim (matches head_to_head.rs) ──────────────────────
#[allow(non_camel_case_types)]
enum XmlDoc {}
unsafe extern "C" {
    fn xmlParseMemory(buffer: *const c_char, size: c_int) -> *mut XmlDoc;
    fn xmlFreeDoc(doc: *mut XmlDoc);
}

fn main() {
    let cases: &[(&str, &str)] = &[
        // ── well-formed baseline ──────────────────────────────────────
        ("baseline-plain-text",
            "<r>just some text content</r>"),
        ("baseline-with-entity",
            "<r>foo &amp; bar</r>"),
        ("baseline-cdata-clean",
            // CDATA section followed by a clean closing tag — both
            // parsers should accept.  (The `]]>` here closes CDATA;
            // it is NOT in text content, so § 2.4 doesn't apply.)
            "<r><![CDATA[anything goes here]]></r>"),

        // ── XML 1.0 § 2.4 [CharData] violation:  literal `]]>` in
        //    text content is forbidden.  A strict parser MUST reject.
        ("ill-formed: ]]> in text",
            "<r>some text]]> more text</r>"),
        ("ill-formed: ]]> at start of text",
            "<r>]]> leading</r>"),
        ("ill-formed: ]]> embedded",
            "<r>aaa]]>bbb</r>"),

        // ── XML 1.0 § 4.1 [Reference]: bare `&` in text is also
        //    forbidden — must be `&amp;` or a real entity ref.
        ("ill-formed: bare ampersand in text",
            "<r>tom & jerry</r>"),

        // ── Sanity: < in text is also forbidden (must be &lt;) but
        //    every parser catches this because `<` is the dispatch
        //    byte.  Included as a control for parser-rejection.
        ("ill-formed: bare lt in text (control)",
            "<r>1 < 2</r>"),

        // ── Additional gaps independently noted in
        //    tafia/quick-xml#848 ("well-formedness", open since
        //    Feb 2025).  Every one of these is a well-formedness
        //    error per XML 1.0; conforming parsers must reject.

        // § 3.1 [STag]/[ETag]: every start tag MUST have a matching
        // end tag.  An unclosed element at EOF is a fatal error.
        ("ill-formed: missing end tag (unclosed root)",
            "<root><a><b></a>"),
        ("ill-formed: unclosed element at EOF",
            "<r><x>"),
        // Mismatched end tag — a start tag's name must equal the
        // matching end tag's.
        ("ill-formed: mismatched end tag",
            "<r><a></b></r>"),

        // § 2.1 [document] = prolog element Misc* — exactly ONE
        // root element is allowed at the document level.
        ("ill-formed: two root elements",
            "<a/><b/>"),
        // Text at document level (between/around root) is forbidden
        // by [document]; only Misc (comments / PIs / whitespace) is
        // legal there.
        ("ill-formed: text at document level",
            "hello<r/>"),
        ("ill-formed: text after root",
            "<r/>trailing text"),

        // § 2.8 [XMLDecl] / [VersionInfo]: the XML declaration must
        // include a version attribute.  An empty `<?xml?>` declaration
        // is malformed; so is one missing the required version.
        ("ill-formed: empty XML declaration",
            "<?xml?><r/>"),
        ("ill-formed: XML decl without version",
            "<?xml encoding='UTF-8'?><r/>"),
    ];

    let bar  = format!("├{:─^42}┼{:─^11}┼{:─^11}┼{:─^11}┼{:─^11}┼{:─^11}┼{:─^11}┤", "", "", "", "", "", "", "");
    let top  = format!("┌{:─^42}┬{:─^11}┬{:─^11}┬{:─^11}┬{:─^11}┬{:─^11}┬{:─^11}┐", "", "", "", "", "", "", "");
    let bot  = format!("└{:─^42}┴{:─^11}┴{:─^11}┴{:─^11}┴{:─^11}┴{:─^11}┴{:─^11}┘", "", "", "", "", "", "", "");
    println!("\n{top}");
    println!("│ {:40} │ {:>9} │ {:>9} │ {:>9} │ {:>9} │ {:>9} │ {:>9} │",
        "case", "sup-xml", "quick-xml", "libxml2", "roxmltree", "xml-rs", "anyxml");
    println!("{bar}");
    for (label, src) in cases {
        let sx = run_super(src.as_bytes());
        let qx = run_qxml(src.as_bytes());
        let lx = run_libxml2(src.as_bytes());
        let rx = run_roxmltree(src.as_bytes());
        let xr = run_xml_rs(src.as_bytes());
        let ax = run_anyxml(src.as_bytes());
        println!("│ {:40} │ {:>9} │ {:>9} │ {:>9} │ {:>9} │ {:>9} │ {:>9} │",
            label,
            verdict(&sx), verdict(&qx), verdict(&lx),
            verdict(&rx), verdict(&xr), verdict(&ax),
        );
    }
    println!("{bot}");
    println!("\nlegend:  OK     = parsed without error");
    println!("         REJECT = parser flagged the input as invalid");
    println!();
    println!("Any 'ill-formed' row showing OK is silently accepting a violation");
    println!("of XML 1.0 — the spec requires those inputs to be rejected.");
}

#[derive(Debug)]
#[allow(dead_code)]   // `msg` is captured for human inspection when debugging.
enum Outcome {
    Ok { events: u32 },
    Reject { event_at: u32, msg: String },
}

fn verdict(o: &Outcome) -> String {
    match o {
        Outcome::Ok { events } => format!("OK ({}ev)", events),
        Outcome::Reject { event_at, .. } => format!("REJECT@{}", event_at),
    }
}

fn run_super(src: &[u8]) -> Outcome {
    // Default options — full spec validation, no skip flags.
    let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(src) }
        .with_options(ParseOptions::default());
    let mut n = 0u32;
    loop {
        match r.next() {
            Ok(BytesEvent::Eof) => return Outcome::Ok { events: n },
            Ok(_) => n += 1,
            Err(e) => return Outcome::Reject { event_at: n, msg: e.to_string() },
        }
    }
}

fn run_qxml(src: &[u8]) -> Outcome {
    let mut r = QxReader::from_reader(src);
    let mut n = 0u32;
    loop {
        match r.read_event() {
            Ok(QxEvent::Eof) => return Outcome::Ok { events: n },
            Ok(_) => n += 1,
            Err(e) => return Outcome::Reject { event_at: n, msg: e.to_string() },
        }
    }
}

fn run_libxml2(src: &[u8]) -> Outcome {
    // libxml2's xmlParseMemory: returns NULL on parse failure.  We
    // can't get a per-event count out of it (it builds a DOM in one
    // shot), so "events" here is 0 / 1 — present just to keep the
    // verdict format consistent.
    //
    // SAFETY: passing a borrowed pointer + length to the C entry
    // point; the slice outlives the call; we free the doc immediately
    // if non-null.  Documented C API.
    // Why unsafe: the only way to call into C.
    let doc = unsafe {
        xmlParseMemory(src.as_ptr() as *const c_char, src.len() as c_int)
    };
    if doc.is_null() {
        Outcome::Reject { event_at: 0, msg: "libxml2 returned NULL doc".into() }
    } else {
        unsafe { xmlFreeDoc(doc); }
        Outcome::Ok { events: 1 }
    }
}

fn run_roxmltree(src: &[u8]) -> Outcome {
    // roxmltree::Document::parse takes a &str — it errors if the
    // bytes aren't UTF-8 OR if the XML is ill-formed.
    let s = match std::str::from_utf8(src) {
        Ok(s) => s,
        Err(e) => return Outcome::Reject { event_at: 0, msg: format!("not UTF-8: {e}") },
    };
    match roxmltree::Document::parse(s) {
        Ok(doc) => Outcome::Ok { events: doc.descendants().count() as u32 },
        Err(e)  => Outcome::Reject { event_at: 0, msg: e.to_string() },
    }
}

fn run_xml_rs(src: &[u8]) -> Outcome {
    use xml::reader::{EventReader, XmlEvent};
    let mut n = 0u32;
    for ev in EventReader::new(src) {
        match ev {
            Ok(XmlEvent::EndDocument) => return Outcome::Ok { events: n },
            Ok(_) => n += 1,
            Err(e) => return Outcome::Reject { event_at: n, msg: e.to_string() },
        }
    }
    // Fell off the end without an explicit EndDocument — count it
    // as accepted.
    Outcome::Ok { events: n }
}

/// anyxml uses a Java-style SAX handler model: three traits
/// (`SAXHandler`, `EntityResolver`, `ErrorHandler`) implemented on
/// one struct.  The default `ErrorHandler` discards errors silently
/// — we override `error`/`fatal_error` to flag a bool instead, then
/// combine that with `parse_str`'s `Result` to decide accept/reject.
#[derive(Default)]
struct AnyXmlCounter { n: u32, errored: bool }

impl AnyEntityResolver for AnyXmlCounter {}
impl AnyErrorHandler for AnyXmlCounter {
    fn error(&mut self, _: AnySAXParseError)       { self.errored = true; }
    fn fatal_error(&mut self, _: AnySAXParseError) { self.errored = true; }
}
impl AnySAXHandler for AnyXmlCounter {
    fn start_element(&mut self, _: Option<&str>, _: Option<&str>, _: &str, _: &AnyAttrs) {
        self.n += 1;
    }
    fn end_element(&mut self, _: Option<&str>, _: Option<&str>, _: &str) {
        self.n += 1;
    }
    fn characters(&mut self, _: &str) { self.n += 1; }
}

fn run_anyxml(src: &[u8]) -> Outcome {
    let s = match std::str::from_utf8(src) {
        Ok(s) => s,
        Err(e) => return Outcome::Reject { event_at: 0, msg: format!("not UTF-8: {e}") },
    };
    let mut reader = AnyXMLReader::builder()
        .set_handler(AnyXmlCounter::default())
        .build();
    let parse_result = reader.parse_str(s, None);
    let h = &reader.handler;
    if h.errored || parse_result.is_err() {
        Outcome::Reject { event_at: h.n, msg: format!("err={:?} flag={}", parse_result.err(), h.errored) }
    } else {
        Outcome::Ok { events: h.n }
    }
}
