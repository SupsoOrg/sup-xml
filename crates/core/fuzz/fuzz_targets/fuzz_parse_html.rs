#![no_main]

//! Fuzz target — feed arbitrary bytes to the HTML5 parser and assert it
//! never panics, hangs, or indexes out of bounds.
//!
//! The HTML path is a rich crash surface: WHATWG encoding sniffing over raw
//! bytes, html5ever's tokenizer + tree builder running in lenient recovery
//! mode, and our `TreeSink` grafting nodes into the arena DOM.  Malformed
//! markup is *expected* — in recovery mode HTML has no well-formedness errors
//! — so a parse that returns `Ok` with a recovered tree is a valid outcome;
//! only panics / infinite loops / OOB are bugs the fuzzer should surface.
//!
//! The default `HtmlParseOptions` depth and text-size limits bound tree growth
//! so pathological nesting can't become an OOM the fuzzer misreports as a
//! crash.  Bytes are fed directly (not gated on valid UTF-8) so the encoding
//! sniffer and its decoders are exercised too.

use libfuzzer_sys::fuzz_target;
use sup_xml_core::html::parse_html_bytes;

fuzz_target!(|data: &[u8]| {
    let _ = parse_html_bytes(data);
});
