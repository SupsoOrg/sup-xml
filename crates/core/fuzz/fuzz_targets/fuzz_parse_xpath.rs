#![no_main]

//! Fuzz target — feed arbitrary bytes (interpreted as UTF-8 strings) to
//! [`parse_xpath`] and assert the XPath lexer + parser never panic.
//!
//! XPath expressions are a classic crash surface: hand-written recursive
//! descent over a token stream with lookahead, numeric literal parsing,
//! QName lexing with `:` ambiguity vs. axis separators, and predicate
//! nesting all interact.  Errors (malformed expression, unexpected EOF,
//! lex failure) are fine; panics, infinite loops, and OOB indexing are
//! bugs the fuzzer should surface.

use libfuzzer_sys::fuzz_target;
use sup_xml_core::xpath::parse_xpath;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = parse_xpath(s);
    }
});
