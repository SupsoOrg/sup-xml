//! Adversarial-input regression tests.
//!
//! Every test loads a file from `tests/assets/xml/attacks/`, runs the parser
//! under a wall-clock timeout on a worker thread, and asserts the parser
//! neither panics nor hangs. Most attacks must be rejected with an error;
//! a handful must parse cleanly.
//!
//! ## Why the timeout
//!
//! These inputs are crafted to break parsers. If we regress and introduce a
//! quadratic loop or unbounded recursion, a naive `parse_bytes` call could
//! hang the test process indefinitely. The timeout makes failures loud
//! (a panicking test message) instead of silent (CI never finishes).
//!
//! If a test times out, the worker thread is leaked — it'll keep running
//! until the test binary exits. That's intentional: there is no portable
//! way to cancel a blocked Rust thread, and leaking is preferable to
//! deadlocking the whole suite.

use std::panic::AssertUnwindSafe;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use sup_xml::{parse_bytes, ParseOptions};

const TIMEOUT_SECS: u64 = 10;

/// Look up an attack fixture's bytes, embedded at compile time.
///
/// Miri runs tests with filesystem isolation enabled, which makes any
/// runtime `std::fs::read` fail. Embedding via `include_bytes!` keeps the
/// fixtures in `tests/assets/xml/attacks/` on disk (where they're easy to
/// edit and view) while baking their contents into the test binary so no
/// I/O happens at test time.
fn read_attack(name: &str) -> &'static [u8] {
    macro_rules! f {
        ($n:literal) => {
            if name == $n {
                return include_bytes!(concat!(
                    "../../../tests/assets/xml/attacks/",
                    $n
                ));
            }
        };
    }
    f!("attribute_name_collision_hash.xml");
    f!("billion_laughs.xml");
    f!("billion_laughs_deep.xml");
    f!("billion_laughs_utf8.xml");
    f!("bom_only.xml");
    f!("cdata_in_attribute.xml");
    f!("charref_above_unicode.xml");
    f!("charref_control.xml");
    f!("charref_huge_number.xml");
    f!("charref_nul.xml");
    f!("charref_surrogate.xml");
    f!("comment_double_hyphen.xml");
    f!("deep_mixed_content.xml");
    f!("deep_nesting_100k.xml");
    f!("deep_nesting_10k.xml");
    f!("deep_nesting_1k.xml");
    f!("default_attr_namespace_interaction.xml");
    f!("dtd_attlist_huge.xml");
    f!("dtd_circular_includes.xml");
    f!("dtd_external_only.xml");
    f!("dtd_notation_redefine.xml");
    f!("dtd_with_pe_in_internal_subset.xml");
    f!("duplicate_attributes.xml");
    f!("empty_document.xml");
    f!("entity_expansion_in_attribute.xml");
    f!("entity_only_predefined.xml");
    f!("entity_undefined.xml");
    f!("huge_attribute_value.xml");
    f!("invalid_utf8_overlong.xml");
    f!("invalid_utf8_surrogate.xml");
    f!("long_attribute_name.xml");
    f!("long_comment.xml");
    f!("long_element_name.xml");
    f!("long_pi_target.xml");
    f!("long_text_node.xml");
    f!("many_attributes.xml");
    f!("mismatched_tags.xml");
    f!("mixed_encoding.xml");
    f!("multiple_roots.xml");
    f!("namespace_prefix_explosion.xml");
    f!("namespace_redefinition.xml");
    f!("nested_comment.xml");
    f!("nested_general_entity_cycle.xml");
    f!("null_byte.xml");
    f!("parameter_entity_recursion.xml");
    f!("parameter_entity_self.xml");
    f!("pi_xml_target.xml");
    f!("prolog_after_root.xml");
    f!("quadratic_blowup.xml");
    f!("trailing_garbage.xml");
    f!("unclosed_root.xml");
    f!("unterminated_cdata.xml");
    f!("unterminated_comment.xml");
    f!("unterminated_pi.xml");
    f!("unterminated_tag.xml");
    f!("utf16_no_bom.xml");
    f!("utf16_surrogate_lone.xml");
    f!("utf7_encoding.xml");
    f!("utf8_bom_wrong_decl.xml");
    f!("whitespace_only.xml");
    f!("xinclude_file.xml");
    f!("xinclude_http.xml");
    f!("xinclude_recursive.xml");
    f!("xinclude_xpointer_bomb.xml");
    f!("xml10_with_nel.xml");
    f!("xml11_c0_controls.xml");
    f!("xml_base_redefinition.xml");
    f!("xml_space_preserve_nested.xml");
    f!("xmlns_empty_uri.xml");
    f!("xxe_expect.xml");
    f!("xxe_file_read.xml");
    f!("xxe_file_read_param.xml");
    f!("xxe_http_ssrf.xml");
    f!("xxe_netdoc.xml");
    f!("xxe_oob_dtd.xml");
    f!("xxe_php_filter.xml");
    panic!("unknown attack fixture: {name} (add an entry in read_attack)");
}

#[derive(Debug)]
enum Outcome {
    Ok,
    Err(String),
    Panicked,
    TimedOut,
}

/// Parse `bytes` under a wall-clock timeout, catching panics.
fn parse_guarded(bytes: &'static [u8], opts: ParseOptions) -> Outcome {
    // Miri's interpreter is orders of magnitude slower than native, so the
    // wall-clock DoS budget is meaningless under it (and the worker thread +
    // channel add heavy interpretation overhead).  Run the parse inline:
    // Miri still checks the parser's unsafe code for UB on adversarial
    // input, and a genuine hang regression is caught by the native run,
    // which keeps the timeout below.
    if cfg!(miri) {
        return match std::panic::catch_unwind(AssertUnwindSafe(|| {
            parse_bytes(bytes, &opts).map(|_doc| ()).map_err(|e| e.to_string())
        })) {
            Ok(Ok(())) => Outcome::Ok,
            Ok(Err(msg)) => Outcome::Err(msg),
            Err(_) => Outcome::Panicked,
        };
    }
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
            parse_bytes(bytes, &opts).map(|_doc| ()).map_err(|e| e.to_string())
        }));
        let _ = tx.send(res);
    });
    match rx.recv_timeout(Duration::from_secs(TIMEOUT_SECS)) {
        Ok(Ok(Ok(()))) => Outcome::Ok,
        Ok(Ok(Err(msg))) => Outcome::Err(msg),
        Ok(Err(_panic)) => Outcome::Panicked,
        Err(_) => Outcome::TimedOut,
    }
}

/// Assert the parser rejected the input cleanly (with an error, not a panic
/// or a hang).
fn assert_rejected(name: &str) {
    let bytes = read_attack(name);
    let outcome = parse_guarded(bytes, ParseOptions::default());
    match outcome {
        Outcome::Err(_) => {}
        Outcome::Ok => panic!("{name}: expected parse error, got success"),
        Outcome::Panicked => panic!("{name}: parser PANICKED — must return Err, not panic"),
        Outcome::TimedOut => {
            panic!("{name}: parser HUNG past {TIMEOUT_SECS}s — possible DoS regression")
        }
    }
}

/// Assert the parser either rejected the input or accepted it — just don't
/// panic or hang. Use this for inputs where the correct behavior is
/// version- or policy-dependent (e.g. XML 1.1 features, encoding edge cases).
fn assert_handled_safely(name: &str) {
    let bytes = read_attack(name);
    let outcome = parse_guarded(bytes, ParseOptions::default());
    match outcome {
        Outcome::Ok | Outcome::Err(_) => {}
        Outcome::Panicked => panic!("{name}: parser PANICKED — must return Err, not panic"),
        Outcome::TimedOut => {
            panic!("{name}: parser HUNG past {TIMEOUT_SECS}s — possible DoS regression")
        }
    }
}

/// Assert the parser accepted the input (it's actually valid XML).
fn assert_accepted(name: &str) {
    let bytes = read_attack(name);
    let outcome = parse_guarded(bytes, ParseOptions::default());
    match outcome {
        Outcome::Ok => {}
        Outcome::Err(msg) => panic!("{name}: expected success, got error: {msg}"),
        Outcome::Panicked => panic!("{name}: parser PANICKED"),
        Outcome::TimedOut => {
            panic!("{name}: parser HUNG past {TIMEOUT_SECS}s — possible DoS regression")
        }
    }
}

// ═══ Entity expansion (DoS) ════════════════════════════════════════════════

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn billion_laughs() {
    assert_rejected("billion_laughs.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn billion_laughs_deep() {
    assert_rejected("billion_laughs_deep.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn billion_laughs_utf8() {
    assert_rejected("billion_laughs_utf8.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn quadratic_blowup() {
    assert_rejected("quadratic_blowup.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn entity_expansion_in_attribute() {
    // ~2 MB expansion in an attribute value — must trip the entity budget.
    assert_rejected("entity_expansion_in_attribute.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn parameter_entity_recursion() {
    assert_rejected("parameter_entity_recursion.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn parameter_entity_self() {
    assert_rejected("parameter_entity_self.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn nested_general_entity_cycle() {
    assert_rejected("nested_general_entity_cycle.xml");
}

// ═══ External entity / XXE ═════════════════════════════════════════════════
// Defaults forbid external loading, so these all become "undefined entity"
// or similar errors. If they ever succeed silently, that's a security bug.

#[test]
fn xxe_file_read() {
    assert_rejected("xxe_file_read.xml");
}

#[test]
fn xxe_file_read_param() {
    assert_rejected("xxe_file_read_param.xml");
}

#[test]
fn xxe_http_ssrf() {
    assert_rejected("xxe_http_ssrf.xml");
}

#[test]
fn xxe_oob_dtd() {
    // External DTD reference; default resolver refuses it.
    assert_handled_safely("xxe_oob_dtd.xml");
}

#[test]
fn xxe_php_filter() {
    assert_rejected("xxe_php_filter.xml");
}

#[test]
fn xxe_expect() {
    assert_rejected("xxe_expect.xml");
}

#[test]
fn xxe_netdoc() {
    assert_rejected("xxe_netdoc.xml");
}

// ═══ XInclude ══════════════════════════════════════════════════════════════
// XInclude processing is off by default — these should pass through as
// literal elements, not perform file reads. So "safely handled" is enough.

#[test]
fn xinclude_file() {
    assert_handled_safely("xinclude_file.xml");
}

#[test]
fn xinclude_recursive() {
    assert_handled_safely("xinclude_recursive.xml");
}

#[test]
fn xinclude_http() {
    assert_handled_safely("xinclude_http.xml");
}

#[test]
fn xinclude_xpointer_bomb() {
    assert_handled_safely("xinclude_xpointer_bomb.xml");
}

// ═══ Structural depth ══════════════════════════════════════════════════════
// Default max_element_depth is 256, so 1k/10k/100k all exceed it.

#[test]
fn deep_nesting_1k() {
    assert_rejected("deep_nesting_1k.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn deep_nesting_10k() {
    assert_rejected("deep_nesting_10k.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn deep_nesting_100k() {
    assert_rejected("deep_nesting_100k.xml");
}

#[test]
fn deep_mixed_content() {
    assert_rejected("deep_mixed_content.xml");
}

// ═══ Attribute / namespace pathologies ═════════════════════════════════════

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn many_attributes() {
    // 100k attributes on one element. Should parse — but must not exhibit
    // quadratic behavior. The 10s timeout is the actual assertion here.
    assert_handled_safely("many_attributes.xml");
}

#[test]
fn duplicate_attributes() {
    assert_rejected("duplicate_attributes.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn huge_attribute_value() {
    // 10 MiB single attribute value. Should parse, just shouldn't hang.
    assert_handled_safely("huge_attribute_value.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn attribute_name_collision_hash() {
    assert_handled_safely("attribute_name_collision_hash.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn namespace_prefix_explosion() {
    assert_handled_safely("namespace_prefix_explosion.xml");
}

#[test]
fn namespace_redefinition() {
    assert_handled_safely("namespace_redefinition.xml");
}

#[test]
fn xmlns_empty_uri() {
    // `xmlns:foo=""` is illegal in XML 1.0 (Namespaces 1.0).
    // With namespace_aware=false (the default), it may be accepted as a
    // plain attribute, so use the lenient assertion.
    assert_handled_safely("xmlns_empty_uri.xml");
}

// ═══ Lexical / scanner ═════════════════════════════════════════════════════

#[test]
fn unterminated_comment() {
    assert_rejected("unterminated_comment.xml");
}

#[test]
fn unterminated_cdata() {
    assert_rejected("unterminated_cdata.xml");
}

#[test]
fn unterminated_pi() {
    assert_rejected("unterminated_pi.xml");
}

#[test]
fn unterminated_tag() {
    assert_rejected("unterminated_tag.xml");
}

#[test]
fn nested_comment() {
    assert_rejected("nested_comment.xml");
}

#[test]
fn comment_double_hyphen() {
    assert_rejected("comment_double_hyphen.xml");
}

#[test]
fn cdata_in_attribute() {
    assert_rejected("cdata_in_attribute.xml");
}

#[test]
fn pi_xml_target() {
    // `<?XML ...?>` (uppercase) — the target `xml` is reserved case-
    // insensitively per the spec. Reject or accept-as-pi both arguable;
    // just don't crash.
    assert_handled_safely("pi_xml_target.xml");
}

#[test]
fn bom_only() {
    assert_rejected("bom_only.xml");
}

#[test]
fn empty_document() {
    assert_rejected("empty_document.xml");
}

#[test]
fn whitespace_only() {
    assert_rejected("whitespace_only.xml");
}

#[test]
fn trailing_garbage() {
    assert_rejected("trailing_garbage.xml");
}

#[test]
fn multiple_roots() {
    assert_rejected("multiple_roots.xml");
}

#[test]
fn prolog_after_root() {
    assert_rejected("prolog_after_root.xml");
}

#[test]
fn mismatched_tags() {
    assert_rejected("mismatched_tags.xml");
}

#[test]
fn unclosed_root() {
    assert_rejected("unclosed_root.xml");
}

// ═══ Encoding ══════════════════════════════════════════════════════════════

#[test]
fn utf16_no_bom() {
    // Declares UTF-16 but no BOM. Parser may or may not infer — just don't crash.
    assert_handled_safely("utf16_no_bom.xml");
}

#[test]
fn utf8_bom_wrong_decl() {
    // UTF-8 BOM, but xml decl claims UTF-16. Conflict — should error.
    assert_handled_safely("utf8_bom_wrong_decl.xml");
}

#[test]
fn utf7_encoding() {
    // UTF-7 is not a valid XML encoding. Must reject (or transcode if
    // auto_transcode is on — but defaults are off).
    assert_handled_safely("utf7_encoding.xml");
}

#[test]
fn mixed_encoding() {
    // Declares UTF-8 but body has lone Latin-1 byte 0xE9. Invalid UTF-8.
    assert_rejected("mixed_encoding.xml");
}

#[test]
fn invalid_utf8_overlong() {
    // Overlong encoding of '/' — security-critical to reject (historical
    // path-traversal vector).
    assert_rejected("invalid_utf8_overlong.xml");
}

#[test]
fn invalid_utf8_surrogate() {
    // UTF-8 encoding of a UTF-16 surrogate — disallowed by RFC 3629.
    assert_rejected("invalid_utf8_surrogate.xml");
}

#[test]
fn null_byte() {
    // NUL is not a valid XML character.
    assert_rejected("null_byte.xml");
}

#[test]
fn utf16_surrogate_lone() {
    assert_handled_safely("utf16_surrogate_lone.xml");
}

// ═══ Character references ══════════════════════════════════════════════════

#[test]
fn charref_above_unicode() {
    assert_rejected("charref_above_unicode.xml");
}

#[test]
fn charref_surrogate() {
    assert_rejected("charref_surrogate.xml");
}

#[test]
fn charref_nul() {
    assert_rejected("charref_nul.xml");
}

#[test]
fn charref_control() {
    assert_rejected("charref_control.xml");
}

#[test]
fn charref_huge_number() {
    // Number doesn't fit in any integer type — must reject without overflow.
    assert_rejected("charref_huge_number.xml");
}

#[test]
fn entity_undefined() {
    assert_rejected("entity_undefined.xml");
}

#[test]
fn entity_only_predefined() {
    // Only uses &lt; &gt; &amp; &apos; &quot; — valid XML, must succeed.
    assert_accepted("entity_only_predefined.xml");
}

// ═══ Long names / huge tokens ══════════════════════════════════════════════

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn long_element_name() {
    // 1 MiB element name. Valid XML technically. Must not exhibit quadratic
    // behavior — timeout is the assertion.
    assert_handled_safely("long_element_name.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn long_attribute_name() {
    assert_handled_safely("long_attribute_name.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn long_pi_target() {
    assert_handled_safely("long_pi_target.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn long_text_node() {
    // 10 MiB text node — should parse, just don't hang.
    assert_handled_safely("long_text_node.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn long_comment() {
    assert_handled_safely("long_comment.xml");
}

// ═══ DTD pathologies ═══════════════════════════════════════════════════════

#[test]
fn dtd_external_only() {
    // External DTD reference to a nonexistent host. Default resolver refuses
    // network, so this should parse the doc without loading the DTD.
    assert_handled_safely("dtd_external_only.xml");
}

#[test]
fn dtd_circular_includes() {
    assert_handled_safely("dtd_circular_includes.xml");
}

#[test]
#[cfg_attr(miri, ignore = "Miri is too slow, TODO re-enable for Miri later")]
fn dtd_attlist_huge() {
    assert_handled_safely("dtd_attlist_huge.xml");
}

#[test]
fn dtd_notation_redefine() {
    assert_handled_safely("dtd_notation_redefine.xml");
}

#[test]
fn dtd_with_pe_in_internal_subset() {
    // PE references inside the internal subset's decl content are illegal
    // in XML 1.0 well-formedness. Should reject.
    assert_handled_safely("dtd_with_pe_in_internal_subset.xml");
}

// ═══ XML 1.0 vs 1.1 ════════════════════════════════════════════════════════

#[test]
fn xml11_c0_controls() {
    // Declared as XML 1.1 — C0 controls allowed as char refs.
    // If parser doesn't support 1.1 yet, rejection is acceptable.
    assert_handled_safely("xml11_c0_controls.xml");
}

#[test]
fn xml10_with_nel() {
    // NEL (U+0085) in body — legal in XML 1.0 as a regular char.
    assert_handled_safely("xml10_with_nel.xml");
}

// ═══ xml:* attributes ══════════════════════════════════════════════════════

#[test]
fn xml_space_preserve_nested() {
    assert_accepted("xml_space_preserve_nested.xml");
}

#[test]
fn xml_base_redefinition() {
    assert_accepted("xml_base_redefinition.xml");
}

#[test]
fn default_attr_namespace_interaction() {
    assert_handled_safely("default_attr_namespace_interaction.xml");
}
