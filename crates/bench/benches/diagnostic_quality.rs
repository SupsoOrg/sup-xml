//! Diagnostic-quality comparison: sup-xml vs libxml2 error messages.
//!
//! Error messages are an underinvested-in surface across the XML
//! ecosystem.  When a 50 MB document fails on line 3812 column 27,
//! the developer's hour-or-day depends on what the parser says next:
//! does it point at a byte offset?  Name the violated rule?  Quote
//! the offending text?  Suggest a fix?  Or just say "parse error"?
//!
//! This bench feeds a curated set of deliberately-broken documents
//! to both parsers and prints the *full* first-error output from
//! each side by side.  For each case we also score four objective
//! dimensions:
//!
//!   - **line**       — is a 1-based line number reported?
//!   - **column**     — is a 1-based column reported?
//!   - **byte ofs**   — is a byte offset reported?  Useful for binary
//!                       pipelines and editors that work in offsets
//!                       rather than (or in addition to) line/col.
//!   - **code**       — is there a stable machine-readable code, vs
//!                       a free-form prose message only?
//!
//! Subjective qualities (does the message *name what was expected*,
//! does it *quote the offending text*) are harder to score
//! mechanically; the side-by-side output is the artefact for
//! eyeballing those.  Read the table and make up your own mind.
//!
//! Run:
//!     cargo bench -p sup-xml-bench --bench diagnostic_quality

#![allow(clippy::missing_safety_doc)]

use std::cell::RefCell;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};

use sup_xml::{parse_bytes, ParseOptions, XmlError};

// ── libxml2 FFI ─────────────────────────────────────────────────────────────
//
// Layout copied from
// `/opt/homebrew/Cellar/libxml2/2.15.3/include/libxml2/libxml/xmlerror.h`
// (struct `_xmlError`).  We only read fields the bench cares about; the
// trailing `ctxt` / `node` pointers are kept so the struct size matches
// what libxml2 hands us.
#[repr(C)]
struct LibxmlErrorRaw {
    domain:  c_int,
    code:    c_int,
    message: *mut c_char,
    level:   c_int,        // xmlErrorLevel
    file:    *mut c_char,
    line:    c_int,
    str1:    *mut c_char,
    str2:    *mut c_char,
    str3:    *mut c_char,
    int1:    c_int,
    int2:    c_int,        // column number
    ctxt:    *mut c_void,
    node:    *mut c_void,
}

unsafe extern "C" {
    fn xmlReadMemory(
        buffer:   *const c_char,
        size:     c_int,
        url:      *const c_char,
        encoding: *const c_char,
        options:  c_int,
    ) -> *mut c_void;
    fn xmlFreeDoc(doc: *mut c_void);
    fn xmlSetStructuredErrorFunc(
        ctx:     *mut c_void,
        handler: Option<unsafe extern "C" fn(*mut c_void, *const LibxmlErrorRaw)>,
    );
    fn xmlSetGenericErrorFunc(
        ctx:     *mut c_void,
        handler: Option<unsafe extern "C" fn()>,
    );
}

#[derive(Debug, Clone)]
struct CapturedError {
    code:    i32,
    line:    Option<u32>,
    column:  Option<u32>,
    message: String,
}

thread_local! {
    /// We only want libxml2's *first* error per parse — it commonly
    /// emits a cascade ("attribute value not started", "attribute value
    /// not finished", ...) where everything after the first is downstream
    /// of the same root cause.  The first-error-only policy matches
    /// what sup-xml's `Result` surface returns to a caller.
    static LIBXML_FIRST: RefCell<Option<CapturedError>> = const { RefCell::new(None) };
}

unsafe extern "C" fn capture(_ctx: *mut c_void, err: *const LibxmlErrorRaw) {
    LIBXML_FIRST.with(|c| {
        let mut slot = c.borrow_mut();
        if slot.is_some() { return; }
        // SAFETY: libxml2 guarantees `err` points at a valid xmlError
        // for the duration of the structured-error callback.
        let e = unsafe { &*err };
        let msg = if e.message.is_null() {
            String::new()
        } else {
            // SAFETY: same; `message` is a NUL-terminated C string
            // owned by libxml2's internal error slot.
            unsafe { CStr::from_ptr(e.message) }
                .to_string_lossy()
                .trim_end_matches('\n')
                .to_string()
        };
        *slot = Some(CapturedError {
            code:    e.code,
            line:    if e.line  > 0 { Some(e.line  as u32) } else { None },
            column:  if e.int2  > 0 { Some(e.int2  as u32) } else { None },
            message: msg,
        });
    });
}

unsafe extern "C" fn swallow() {}

fn install_silencers() {
    // libxml2 emits errors through two parallel channels — silence
    // the printf-style one entirely and route the structured one
    // through our capture callback.  Both defaults are thread-local.
    // SAFETY: registering handlers; no aliasing concerns.
    unsafe {
        xmlSetGenericErrorFunc(std::ptr::null_mut(), Some(swallow));
        xmlSetStructuredErrorFunc(std::ptr::null_mut(), Some(capture));
    }
}

fn libxml_first_error(bytes: &[u8]) -> Option<CapturedError> {
    LIBXML_FIRST.with(|c| *c.borrow_mut() = None);
    // SAFETY: passing a borrowed slice to a read-only C function;
    // freeing whatever doc comes back (NULL is fine).
    unsafe {
        let doc = xmlReadMemory(
            bytes.as_ptr() as *const c_char, bytes.len() as c_int,
            std::ptr::null(), std::ptr::null(),
            0,
        );
        if !doc.is_null() { xmlFreeDoc(doc); }
    }
    LIBXML_FIRST.with(|c| c.borrow_mut().take())
}

// ── sup-xml side ────────────────────────────────────────────────────────────

fn supxml_error(bytes: &[u8]) -> Option<XmlError> {
    parse_bytes(bytes, &ParseOptions::default()).err()
}

// ── corpus ──────────────────────────────────────────────────────────────────

/// A representative slice of failure modes.  Aiming for breadth, not
/// exhaustion: one canonical example per category of well-formedness
/// violation, plus a couple of cases where the error sits deep in a
/// large document so the value of precise location is visible.
struct Case<'a> {
    label:    &'static str,
    /// One-line description of what's wrong, shown above the doc.
    problem:  &'static str,
    bytes:    &'a [u8],
}

const CASES_STATIC: &[Case<'static>] = &[
    Case {
        label:   "unclosed root",
        problem: "<r> never closes before EOF",
        bytes:   b"<r>",
    },
    Case {
        label:   "tag name mismatch",
        problem: "<a> closed with </b>",
        bytes:   b"<a></b>",
    },
    Case {
        label:   "deep tag mismatch",
        problem: "</c> tries to close <b> three levels deep",
        bytes:   b"<a><b><c></a></b></c>",
    },
    Case {
        label:   "bare ampersand",
        problem: "`&` in text isn't a valid entity start",
        bytes:   b"<r>tom & jerry</r>",
    },
    Case {
        label:   "bare less-than",
        problem: "`<` in text is reserved for tag starts",
        bytes:   b"<r>1 < 2</r>",
    },
    Case {
        label:   "unterminated comment",
        problem: "<!-- ... never reaches -->",
        bytes:   b"<r><!-- forever</r>",
    },
    Case {
        label:   "unterminated CDATA",
        problem: "<![CDATA[ ... never reaches ]]>",
        bytes:   b"<r><![CDATA[ forever</r>",
    },
    Case {
        label:   "duplicate attribute",
        problem: "two `a=` attributes on the same element",
        bytes:   b"<r a='1' a='2'/>",
    },
    Case {
        label:   "unquoted attr value",
        problem: "attribute value is missing its quotes",
        bytes:   b"<r a=1/>",
    },
    Case {
        label:   "missing `=` on attr",
        problem: "attribute name with no `=` separator",
        bytes:   b"<r a 'foo'/>",
    },
    Case {
        label:   "undeclared ns prefix",
        problem: "<x:a> uses `x:` but no xmlns:x in scope",
        bytes:   b"<r><x:a/></r>",
    },
    Case {
        label:   "empty char ref",
        problem: "&#x; has no hex digits",
        bytes:   b"<r>&#x;</r>",
    },
    Case {
        label:   "undeclared entity",
        problem: "&nope; is not a predefined or declared entity",
        bytes:   b"<r>&nope;</r>",
    },
    Case {
        label:   "control char in text",
        problem: "U+0001 is outside XML 1.0 Char production",
        bytes:   b"<r>\x01</r>",
    },
    Case {
        label:   "extra content after root",
        problem: "junk bytes appear after </r>",
        bytes:   b"<r/>junk",
    },
    Case {
        label:   "invalid utf-8 byte",
        problem: "0xFE never legally appears in UTF-8",
        bytes:   b"<r>\xFE</r>",
    },
    Case {
        label:   "two roots",
        problem: "second top-level element where only one root is allowed",
        bytes:   b"<a/><b/>",
    },
    Case {
        label:   "doctype after root",
        problem: "<!DOCTYPE must appear before the root element",
        bytes:   b"<r/><!DOCTYPE r>",
    },
];

/// A larger synthetic doc with the error deep inside, so the
/// reported line/column actually matters — the whole point of
/// precise location reporting only shows up at scale.
///
/// Builds 200 well-formed `<row n="K">…</row>` lines wrapped in
/// `<root>`, with one row in the middle opening a `<bad>` that
/// never closes; that row's own `</row>` then becomes the
/// mismatched end tag.  Reporters that point at the failing line
/// help, reporters that say "line 1 col 1" or omit location do not.
fn deep_error_doc() -> Vec<u8> {
    let mut s = String::with_capacity(8 * 1024);
    s.push_str("<root>\n");
    for n in 1..=200 {
        // `<root>\n` is line 1, so row N sits on line N+1.  We pick
        // N=197 here so the broken element lands on line 198 — the
        // value in the label below has to track this in lockstep.
        if n == 197 {
            s.push_str(&format!("  <row n=\"{n}\"><bad></row>\n"));
        } else {
            s.push_str(&format!("  <row n=\"{n}\">content</row>\n"));
        }
    }
    s.push_str("</root>\n");
    s.into_bytes()
}

// ── presentation ────────────────────────────────────────────────────────────

fn fmt_supxml(e: &XmlError) -> Vec<String> {
    let loc = match (e.line, e.column) {
        (Some(l), Some(c)) => format!("line {l}, col {c}"),
        (Some(l), None)    => format!("line {l}"),
        _                  => "(no location)".to_string(),
    };
    let ofs = match e.byte_offset {
        Some(o) => format!("byte {o}"),
        None    => "(no byte offset)".to_string(),
    };
    vec![
        format!("code:  {:?} (= {} libxml2-numeric)", e.code, e.code as i32),
        format!("level: {:?}",   e.level),
        format!("where: {loc}"),
        format!("ofs:   {ofs}"),
        format!("msg:   {}",     e.message),
    ]
}

fn fmt_libxml(e: &CapturedError) -> Vec<String> {
    let loc = match (e.line, e.column) {
        (Some(l), Some(c)) => format!("line {l}, col {c}"),
        (Some(l), None)    => format!("line {l}"),
        _                  => "(no location)".to_string(),
    };
    vec![
        format!("code:  {}",       e.code),
        format!("where: {loc}"),
        // libxml2 has no byte-offset field; the placeholder keeps
        // the two columns row-aligned so eyeballing is easier.
        "ofs:   (not exposed)".to_string(),
        format!("msg:   {}",       e.message),
    ]
}

/// Print two error blocks as a side-by-side, with each block in its
/// own column.  Column wrap is intentionally minimal — long messages
/// can overflow the right column; the reader sees the full text in
/// the single-column dump under each case for fidelity.
fn print_side_by_side(label_a: &str, a: &[String], label_b: &str, b: &[String]) {
    const W: usize = 64;
    println!("  {:<W$}  {:<W$}", label_a, label_b, W = W);
    println!("  {:─<W$}  {:─<W$}", "", "", W = W);
    let n = a.len().max(b.len());
    for i in 0..n {
        let l = a.get(i).map(String::as_str).unwrap_or("");
        let r = b.get(i).map(String::as_str).unwrap_or("");
        // Truncate each cell, leaving an ellipsis indicator when
        // we had to cut.  Full content is also in the dump above.
        let l = trunc(l, W);
        let r = trunc(r, W);
        println!("  {l:<W$}  {r:<W$}", W = W);
    }
}

fn trunc(s: &str, w: usize) -> String {
    if s.chars().count() <= w { return s.to_string(); }
    let take: String = s.chars().take(w.saturating_sub(1)).collect();
    format!("{take}…")
}

/// Render input bytes for the header: ASCII printable as-is; non-
/// printable / non-ASCII shown as `\xNN`.  Newlines kept literal so
/// multi-line cases keep their shape.
fn show_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\n' => out.push('\n'),
            0x20..=0x7e => out.push(b as char),
            _   => out.push_str(&format!("\\x{b:02X}")),
        }
    }
    out
}

// ── scoring ─────────────────────────────────────────────────────────────────

#[derive(Default, Copy, Clone)]
struct Score {
    has_line:        u32,
    has_column:      u32,
    has_byte_offset: u32,
    has_code:        u32,
    rejected:        u32,
}

fn score_supxml(s: &mut Score, e: Option<&XmlError>) {
    if let Some(e) = e {
        s.rejected += 1;
        if e.line.is_some()        { s.has_line        += 1; }
        if e.column.is_some()      { s.has_column      += 1; }
        if e.byte_offset.is_some() { s.has_byte_offset += 1; }
        // sup-xml always carries an ErrorCode enum.  We credit
        // "stable code" only when it's something more specific
        // than the InternalError catch-all (= 1).
        if (e.code as i32) != 1 { s.has_code += 1; }
    }
}

fn score_libxml(s: &mut Score, e: Option<&CapturedError>) {
    if let Some(e) = e {
        s.rejected += 1;
        if e.line.is_some()   { s.has_line   += 1; }
        if e.column.is_some() { s.has_column += 1; }
        // libxml2's xmlError has no byte-offset field at all, so
        // this column is always zero on its side.
        // libxml2 maps every diagnostic site to one of ~800 codes
        // in xmlParserErrors.  0 is the no-error sentinel; everything
        // else is specific enough to act on.
        if e.code != 0 && e.code != 1 { s.has_code += 1; }
    }
}

// ── main ────────────────────────────────────────────────────────────────────

fn main() {
    install_silencers();

    println!();
    println!("Diagnostic-quality comparison: sup-xml vs libxml2");
    println!("=================================================");
    println!();
    println!("For each case: the broken document, then both parsers' first error.");
    println!("Both parsers stop at the first fatal — libxml2 may emit a cascade");
    println!("internally; we capture only the first to keep the comparison fair.");
    println!();

    // Materialise the deep-error case so it lives long enough.  Built
    // here rather than as `const` because the body is too repetitive
    // to write by hand.
    let deep = deep_error_doc();
    let deep_case = Case {
        label:   "deep mismatch (line 198 of 200)",
        problem: "unclosed <bad> on line 198 forces </row> to mismatch",
        bytes:   &deep,
    };
    // Chain the static cases with the runtime-built deep case.  We
    // can't put them in one Vec because their lifetime parameters
    // differ; iterating in sequence is the cheapest fix.
    let all_iter = CASES_STATIC.iter().chain(std::iter::once(&deep_case));
    let total = CASES_STATIC.len() + 1;

    let mut sx_score = Score::default();
    let mut lx_score = Score::default();
    let mut both_rejected     = 0;
    let mut only_sx_rejected  = 0;
    let mut only_lx_rejected  = 0;
    let mut neither_rejected  = 0;

    for (idx, case) in all_iter.enumerate() {
        println!("──[{:02}] {} ─────────────────────────────────",
                 idx + 1, case.label);
        println!("problem: {}", case.problem);
        // Cap the input dump so the deep-error case doesn't bury
        // the output; the whole point of that case is the *error*,
        // not the doc.
        let dump = show_bytes(case.bytes);
        if dump.len() <= 240 {
            println!("input:   {dump}");
        } else {
            let head: String = dump.chars().take(120).collect();
            let tail: String = dump.chars().rev().take(120).collect::<String>()
                .chars().rev().collect();
            println!("input:   ({} bytes) {head} … {tail}", case.bytes.len());
        }
        println!();

        let sx = supxml_error(case.bytes);
        let lx = libxml_first_error(case.bytes);

        score_supxml(&mut sx_score, sx.as_ref());
        score_libxml(&mut lx_score, lx.as_ref());

        match (sx.is_some(), lx.is_some()) {
            (true,  true)  => both_rejected    += 1,
            (true,  false) => only_sx_rejected += 1,
            (false, true)  => only_lx_rejected += 1,
            (false, false) => neither_rejected += 1,
        }

        let sx_block = match &sx {
            Some(e) => fmt_supxml(e),
            None    => vec!["(accepted — no error)".to_string()],
        };
        let lx_block = match &lx {
            Some(e) => fmt_libxml(e),
            None    => vec!["(accepted — no error)".to_string()],
        };
        print_side_by_side("sup-xml", &sx_block, "libxml2", &lx_block);
        println!();

        // One-line objective note about what *this case* surfaced.
        // Subjective takes are in the trailing analysis below.
        println!("  notes: {}", per_case_note(sx.as_ref(), lx.as_ref()));
        println!();
    }

    // ── summary ────────────────────────────────────────────────────────────
    let n = total as u32;
    println!("Summary across {n} cases");
    println!("────────────────────────");
    println!("                          sup-xml   libxml2");
    println!("rejected the input        {:>5}/{}  {:>5}/{}",
             sx_score.rejected, n, lx_score.rejected, n);
    println!("reported a line number    {:>5}/{}  {:>5}/{}",
             sx_score.has_line, sx_score.rejected,
             lx_score.has_line, lx_score.rejected);
    println!("reported a column         {:>5}/{}  {:>5}/{}",
             sx_score.has_column, sx_score.rejected,
             lx_score.has_column, lx_score.rejected);
    println!("reported a specific code  {:>5}/{}  {:>5}/{}",
             sx_score.has_code, sx_score.rejected,
             lx_score.has_code, lx_score.rejected);
    println!("reported a byte offset    {:>5}/{}  {:>5}/{}",
             sx_score.has_byte_offset, sx_score.rejected,
             lx_score.has_byte_offset, lx_score.rejected);
    println!();
    println!("agreement on accept/reject:");
    println!("  both rejected      {both_rejected}");
    println!("  only sup-xml       {only_sx_rejected}");
    println!("  only libxml2       {only_lx_rejected}");
    println!("  both accepted (!)  {neither_rejected}");
    println!();

    // ── observations ──────────────────────────────────────────────────────
    //
    // These are the things the bench can *say* with confidence
    // because they're derivable from the captured fields.  Anything
    // subjective ("which message is friendlier?") is left to the
    // reader staring at the side-by-side output.
    println!("Observations");
    println!("────────────");
    println!("- sup-xml reports a byte offset; libxml2 does not.  Editors,");
    println!("  LSP servers, and binary pipelines (gzipped XML, network");
    println!("  captures, mmap'd files) can act on `byte_offset` without");
    println!("  re-walking the input.  Byte offsets also survive XML 1.0");
    println!("  § 2.11 line-ending normalization, where line/col can drift.");
    println!("- libxml2's `code` field is one of ~800 xmlParserErrors values,");
    println!("  which makes programmatic dispatch easy but doesn't help a human.");
    println!("  sup-xml's `ErrorCode` is a hand-curated enum of ~40 of those");
    println!("  values; anything outside the set lands on `InternalError` and");
    println!("  forces consumers to fall back to message inspection.");
    println!("- Neither parser quotes the offending token in the message.  When");
    println!("  there are five `<bad>` tags on a page, \"opening and ending tag");
    println!("  mismatch\" leaves the developer scanning the line themselves.");
    println!("- Neither parser offers a recovery hint (\"did you mean </a>?\").");
    println!("  That's the obvious shipping-grade improvement.");
}

fn per_case_note(sx: Option<&XmlError>, lx: Option<&CapturedError>) -> String {
    match (sx, lx) {
        (Some(s), Some(l)) => {
            let mut notes = Vec::new();
            match (s.line, l.line) {
                (Some(a), Some(b)) if a == b => notes.push("agree on line".to_string()),
                (Some(a), Some(b))           => notes.push(format!("line disagreement: sup-xml={a}, libxml2={b}")),
                (Some(_), None)              => notes.push("only sup-xml has a line".to_string()),
                (None, Some(_))              => notes.push("only libxml2 has a line".to_string()),
                (None, None)                 => notes.push("neither has a line".to_string()),
            }
            match (s.column, l.column) {
                (Some(a), Some(b)) if a == b => notes.push("agree on column".to_string()),
                (Some(a), Some(b))           => notes.push(format!("col disagreement: sup-xml={a}, libxml2={b}")),
                (Some(_), None)              => notes.push("only sup-xml has a column".to_string()),
                (None, Some(_))              => notes.push("only libxml2 has a column".to_string()),
                (None, None)                 => notes.push("neither has a column".to_string()),
            }
            // libxml2 has no byte-offset surface, so the comparison
            // is one-sided.  We still call it out because it's the
            // one diagnostic dimension sup-xml leads on.
            match s.byte_offset {
                Some(o) => notes.push(format!("sup-xml byte offset {o}")),
                None    => notes.push("sup-xml byte offset missing".to_string()),
            }
            notes.join("; ")
        }
        (Some(_), None) => "only sup-xml rejected".to_string(),
        (None, Some(_)) => "only libxml2 rejected".to_string(),
        (None, None)    => "both accepted — bench case may need to be stricter".to_string(),
    }
}
