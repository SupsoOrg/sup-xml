//! Integration tests for the `sup-xml` binary.
//!
//! Each test invokes the binary in a subprocess and asserts on
//! stdout, stderr, and the exit code.  No external crates; uses
//! `env!("CARGO_BIN_EXE_sup-xml")` to locate the just-built binary
//! and `env!("CARGO_TARGET_TMPDIR")` for per-test scratch files.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const SAMPLE_OK: &str = "\
<?xml version=\"1.0\"?>\n\
<catalog>\n\
  <book id=\"1\"><title>Dune</title><author>Herbert</author></book>\n\
  <book id=\"2\"><title>Foundation</title><author>Asimov</author></book>\n\
</catalog>\n";

const SAMPLE_MALFORMED: &str = "<r>tom & jerry<unclosed>";

// ── helpers ───────────────────────────────────────────────────────────────────

/// A committed, production-key-signed license certificate (org "Acme
/// Corp", expires 2030-01-01).  Every gated command needs a valid
/// license, so the default command builder points `SUPSO_LICENSE_PATH` at it.
///
/// NOTE: this certificate expires 2030-01-01; these tests will start
/// failing after that date until the fixture is replaced with a
/// longer-lived one (re-minted by the issuer).
fn license_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/license.cert")
}

fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_sup-xml"));
    c.env("SUPSO_LICENSE_PATH", license_fixture());
    c
}

/// A command builder with no license reachable: `SUPSO_LICENSE_PATH` unset,
/// and `HOME` plus the working directory pointed at an empty scratch dir
/// so the default `.supso/license_certificates/` search finds nothing.
fn bin_unlicensed() -> Command {
    let empty = tmp("no-license-home");
    fs::create_dir_all(&empty).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_sup-xml"));
    c.env_remove("SUPSO_LICENSE_PATH");
    c.env("HOME", &empty);
    c.env("USERPROFILE", &empty);
    c.current_dir(&empty);
    c
}

fn tmp(name: &str) -> PathBuf {
    let dir: PathBuf = env!("CARGO_TARGET_TMPDIR").into();
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn write_tmp(name: &str, contents: &str) -> PathBuf {
    let p = tmp(name);
    fs::write(&p, contents).unwrap();
    p
}

fn run(args: &[&OsStr]) -> Output {
    bin().args(args).output().unwrap()
}

fn run_stdin(args: &[&OsStr], stdin: &str) -> Output {
    let mut child = bin()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn ok(o: &Output, msg: &str) {
    assert!(
        o.status.success(),
        "{msg}: rc={:?}\nstdout={}\nstderr={}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr),
    );
}

fn rc(o: &Output) -> i32 {
    o.status.code().expect("process killed by signal")
}

fn stdout(o: &Output) -> String { String::from_utf8(o.stdout.clone()).unwrap() }
fn stderr(o: &Output) -> String { String::from_utf8(o.stderr.clone()).unwrap() }

// ── lint ──────────────────────────────────────────────────────────────────────

#[test]
fn lint_well_formed_file_succeeds_silently() {
    // Default lint is silent-on-success: exit code carries the signal.
    let f = write_tmp("ok.xml", SAMPLE_OK);
    let o = run(&[OsStr::new("lint"), f.as_os_str()]);
    ok(&o, "lint ok.xml");
    assert!(stdout(&o).is_empty(), "expected silent default; got stdout: {:?}", stdout(&o));
    assert!(stderr(&o).is_empty(), "expected silent default; got stderr: {:?}", stderr(&o));
}

#[test]
fn lint_verbose_emits_ok_line_per_file() {
    let f = write_tmp("ok_v.xml", SAMPLE_OK);
    let o = run(&[OsStr::new("lint"), OsStr::new("--verbose"), f.as_os_str()]);
    ok(&o, "lint --verbose ok_v.xml");
    assert!(stdout(&o).contains("OK"),
        "expected --verbose to print an OK line; got: {:?}", stdout(&o));
}

#[test]
fn lint_malformed_file_fails_with_message() {
    let f = write_tmp("bad.xml", SAMPLE_MALFORMED);
    let o = run(&[OsStr::new("lint"), f.as_os_str()]);
    assert_eq!(rc(&o), 1);
    let err = stderr(&o);
    assert!(err.contains("bad.xml"), "expected filename in stderr; got: {err}");
    assert!(err.lines().count() == 1, "expected single error line; got:\n{err}");
}

#[test]
fn lint_quiet_well_formed_emits_no_stdout() {
    let f = write_tmp("ok2.xml", SAMPLE_OK);
    let o = run(&[OsStr::new("-q"), OsStr::new("lint"), f.as_os_str()]);
    ok(&o, "lint -q");
    assert!(stdout(&o).is_empty(), "expected empty stdout; got: {}", stdout(&o));
}

#[test]
fn lint_keep_going_reports_only_failures_by_default() {
    let good = write_tmp("good.xml", SAMPLE_OK);
    let bad = write_tmp("bad2.xml", SAMPLE_MALFORMED);
    let o = run(&[OsStr::new("lint"), OsStr::new("--keep-going"),
                  good.as_os_str(), bad.as_os_str()]);
    assert_eq!(rc(&o), 1);
    assert!(stdout(&o).is_empty(), "default lint should not print OK lines");
    assert!(stderr(&o).contains("bad2.xml"));
}

#[test]
fn lint_keep_going_verbose_reports_both_files() {
    let good = write_tmp("good_v.xml", SAMPLE_OK);
    let bad = write_tmp("bad2_v.xml", SAMPLE_MALFORMED);
    let o = run(&[OsStr::new("lint"), OsStr::new("--verbose"), OsStr::new("--keep-going"),
                  good.as_os_str(), bad.as_os_str()]);
    assert_eq!(rc(&o), 1);
    assert!(stdout(&o).contains("good_v.xml: OK"));
    assert!(stderr(&o).contains("bad2_v.xml"));
}

#[test]
fn lint_stdin_well_formed() {
    let o = run_stdin(&[OsStr::new("lint")], SAMPLE_OK);
    ok(&o, "lint stdin");
    assert!(stdout(&o).is_empty(), "default lint should be silent on success");
}

#[test]
fn lint_stdin_well_formed_verbose_labels_stdin() {
    let o = run_stdin(&[OsStr::new("lint"), OsStr::new("-v")], SAMPLE_OK);
    ok(&o, "lint -v stdin");
    assert!(stdout(&o).contains("<stdin>: OK"));
}

#[test]
fn lint_json_emits_array_with_per_file_records() {
    let good = write_tmp("lint_json_good.xml", SAMPLE_OK);
    let bad  = write_tmp("lint_json_bad.xml",  SAMPLE_MALFORMED);
    let o = run(&[
        OsStr::new("lint"), OsStr::new("--json"),
        good.as_os_str(), bad.as_os_str(),
    ]);
    // Mixed: one pass, one fail.  Exit is `1` (the negative-result
    // code) regardless of `--json`.
    assert_eq!(rc(&o), 1, "stderr={}", stderr(&o));
    let out = stdout(&o);
    assert!(out.starts_with('[') && out.trim_end().ends_with(']'),
        "expected JSON array envelope; got: {out}");
    assert!(out.contains(r#""ok":true"#),  "missing ok:true: {out}");
    assert!(out.contains(r#""ok":false"#), "missing ok:false: {out}");
    assert!(out.contains("lint_json_good.xml"), "missing good filename: {out}");
    assert!(out.contains("lint_json_bad.xml"),  "missing bad filename:  {out}");
    // The failure record should carry an error object with a message
    // and a non-null line.
    assert!(out.contains(r#""message":""#),
        "expected error.message field on failure: {out}");
}

#[test]
fn lint_json_implies_keep_going() {
    // Without --json, a single failure stops iteration.  With
    // --json, every file must appear in the report.  Build a mix
    // where the FIRST file fails and assert the second one still
    // shows up.
    let bad  = write_tmp("lint_json_kg_bad.xml",  SAMPLE_MALFORMED);
    let good = write_tmp("lint_json_kg_good.xml", SAMPLE_OK);
    let o = run(&[
        OsStr::new("lint"), OsStr::new("--json"),
        bad.as_os_str(), good.as_os_str(),
    ]);
    assert_eq!(rc(&o), 1, "stderr={}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("lint_json_kg_bad.xml"),  "bad file missing: {out}");
    assert!(out.contains("lint_json_kg_good.xml"), "second file should appear under --json: {out}");
}

#[test]
fn lint_json_all_ok_returns_zero_with_empty_stderr() {
    let p = write_tmp("lint_json_all_ok.xml", SAMPLE_OK);
    let o = run(&[OsStr::new("lint"), OsStr::new("--json"), p.as_os_str()]);
    ok(&o, "lint --json all-ok");
    assert!(stderr(&o).is_empty(), "expected silent stderr; got: {:?}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains(r#""ok":true"#) && out.contains(r#""error":null"#),
        "expected ok:true + error:null on success; got: {out}");
}

#[test]
fn lint_json_escapes_quotes_in_error_messages() {
    // Diagnostics often quote the offending byte sequence ("...").
    // Those embedded `"` must be JSON-escaped so the array still
    // parses.  We don't try to control the exact message — just
    // confirm we never emit a bare `"` inside the message field.
    let bad = write_tmp("lint_json_escape.xml", "<a><b></a>");
    let o = run(&[OsStr::new("lint"), OsStr::new("--json"), bad.as_os_str()]);
    assert_eq!(rc(&o), 1);
    let out = stdout(&o);
    // Find the `"message":"..."` span and confirm any internal
    // double-quote is preceded by a backslash.  Brittle parsing
    // but sufficient for a smoke test without serde_json.
    let key = r#""message":""#;
    let msg_start = out.find(key).expect("no message field") + key.len();
    let after = &out[msg_start..];
    // Scan until the closing unescaped `"`.
    let mut i = 0usize;
    let bytes = after.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'"' && (i == 0 || bytes[i - 1] != b'\\') { break; }
        i += 1;
    }
    let msg_body = &after[..i];
    // Every `"` inside must be `\"`.
    assert!(!msg_body.chars().enumerate().any(|(idx, c)|
        c == '"' && (idx == 0 || msg_body.as_bytes()[idx - 1] != b'\\')),
        "found unescaped `\"` inside message: {msg_body:?}");
}

// ── format ────────────────────────────────────────────────────────────────────

#[test]
fn format_default_is_compact_round_trip() {
    let o = run_stdin(&[OsStr::new("format")], "<r a=\"1\"><b/></r>");
    ok(&o, "print compact roundtrip");
    let out = stdout(&o);
    assert!(out.contains("<?xml"));
    assert!(out.contains("a=\"1\""));
    assert!(out.contains("<b/>"));
    // Compact: no inserted newline between <r> and <b/>.
    assert!(!out.contains("<r>\n"), "compact must not add newline: {out:?}");
}

#[test]
fn format_pretty_indents_two_spaces_by_default() {
    let o = run_stdin(&[OsStr::new("format"), OsStr::new("--pretty")], "<r><a/></r>");
    ok(&o, "print --pretty default indent");
    let out = stdout(&o);
    assert!(out.contains("<r>\n  <a/>"), "unexpected indent: {out:?}");
}

#[test]
fn format_pretty_indent_n_controls_width() {
    let o = run_stdin(
        &[
            OsStr::new("format"),
            OsStr::new("--pretty"),
            OsStr::new("--indent"),
            OsStr::new("4"),
        ],
        "<r><a/></r>",
    );
    ok(&o, "print --pretty --indent 4");
    assert!(stdout(&o).contains("<r>\n    <a/>"));
}

#[test]
fn format_pretty_indent_tabs() {
    let o = run_stdin(
        &[
            OsStr::new("format"),
            OsStr::new("--pretty"),
            OsStr::new("--indent-tabs"),
        ],
        "<r><a/></r>",
    );
    ok(&o, "print --pretty --indent-tabs");
    assert!(stdout(&o).contains("<r>\n\t<a/>"));
}

#[test]
fn format_no_xml_decl_omits_prolog() {
    let o = run_stdin(
        &[OsStr::new("format"), OsStr::new("--no-xml-decl")],
        "<r/>",
    );
    ok(&o, "print --no-xml-decl");
    let out = stdout(&o);
    assert!(!out.contains("<?xml"));
    assert!(out.trim_end() == "<r/>");
}

#[test]
fn format_check_passes_on_already_normalized() {
    // First produce a canonical compact form.
    let o1 = run_stdin(&[OsStr::new("format")], "<r><a/></r>");
    let normalized = stdout(&o1);
    let p = write_tmp("print_check_pass.xml", &normalized);
    let o = run(&[OsStr::new("format"), OsStr::new("--check"), p.as_os_str()]);
    ok(&o, "print --check on normalized input");
}

#[test]
fn format_check_fails_on_unnormalized() {
    // Single-quoted attrs aren't canonical (we always emit double).
    let p = write_tmp("print_check_fail.xml", "<r a='1'/>");
    let o = run(&[OsStr::new("format"), OsStr::new("--check"), p.as_os_str()]);
    assert_eq!(rc(&o), 1);
}

#[test]
fn format_pretty_check_fails_on_unformatted() {
    let p = write_tmp("print_pretty_check_fail.xml", "<r><a/></r>");
    let o = run(&[
        OsStr::new("format"),
        OsStr::new("--pretty"),
        OsStr::new("--check"),
        p.as_os_str(),
    ]);
    assert_eq!(rc(&o), 1);
}

#[test]
fn format_pretty_in_place_rewrites_file() {
    let p = write_tmp("print_inplace.xml", "<r><a/></r>");
    let o = run(&[
        OsStr::new("format"),
        OsStr::new("--pretty"),
        OsStr::new("--in-place"),
        p.as_os_str(),
    ]);
    ok(&o, "print --pretty --in-place");
    let written = fs::read_to_string(&p).unwrap();
    assert!(written.contains("\n  <a/>"), "file not rewritten: {written:?}");
}

#[cfg(unix)]
#[test]
fn format_in_place_preserves_executable_mode() {
    // Write a file with a non-default mode (0o755 — executable),
    // round-trip via `--in-place`, confirm the mode survived.  Used
    // to silently revert to default 0o644 because `fs::write`
    // recreates the file from scratch.
    use std::os::unix::fs::PermissionsExt;
    let p = write_tmp("format_inplace_mode.xml", "<r><a/></r>");
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    let before = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    assert_eq!(before, 0o755, "test setup: mode wasn't applied (umask?)");

    let o = run(&[OsStr::new("format"), OsStr::new("--in-place"), p.as_os_str()]);
    ok(&o, "format --in-place preserve mode");

    let after = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    assert_eq!(after, 0o755, "expected mode 0o755 preserved; got 0o{after:o}");
}

#[cfg(unix)]
#[test]
fn format_in_place_preserves_group_and_other_read_bits() {
    // Mode bits beyond the user-write default — confirms we're
    // capturing the full nine bits, not just executable.
    use std::os::unix::fs::PermissionsExt;
    let p = write_tmp("format_inplace_mode2.xml", "<r/>");
    fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
    let before = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    if before != 0o640 {
        // Some filesystems (notably tmpfs / sandboxed CI) won't honour
        // arbitrary modes; skip silently rather than false-positive.
        eprintln!("skip: filesystem won't honour mode 0o640 (got 0o{before:o})");
        return;
    }

    let o = run(&[OsStr::new("format"), OsStr::new("--in-place"), p.as_os_str()]);
    ok(&o, "format --in-place preserve 0o640");

    let after = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    assert_eq!(after, 0o640, "expected 0o640 preserved; got 0o{after:o}");
}

#[test]
fn format_in_place_leaves_no_tempfile_on_success() {
    // The atomic-write path uses a `.X.sup-xml.tmp.<pid>.<nonce>`
    // sibling.  After a successful rename the sibling shouldn't
    // exist anymore — confirm we're not leaking tempfiles.
    let p = write_tmp("format_inplace_clean.xml", "<r/>");
    let o = run(&[OsStr::new("format"), OsStr::new("--in-place"), p.as_os_str()]);
    ok(&o, "format --in-place no leftover tempfile");

    let parent = p.parent().unwrap();
    let stem = p.file_name().unwrap().to_string_lossy().to_string();
    let leftover_prefix = format!(".{stem}.sup-xml.tmp.");
    let leftovers: Vec<_> = fs::read_dir(parent).unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(&leftover_prefix))
        .collect();
    assert!(leftovers.is_empty(),
        "expected no leftover sup-xml tempfiles; found: {:?}",
        leftovers.iter().map(|e| e.file_name()).collect::<Vec<_>>());
}

#[test]
fn format_accepts_print_alias_for_back_compat() {
    // `print` is the historical name; kept as a hidden clap alias so
    // existing scripts don't break after the rename to `format`.
    let a = run_stdin(&[OsStr::new("print")],  "<r a='1'/>");
    let b = run_stdin(&[OsStr::new("format")], "<r a='1'/>");
    ok(&a, "print alias");
    ok(&b, "format command");
    assert_eq!(stdout(&a), stdout(&b),
        "print and format should produce identical output");
}

// ── xpath ─────────────────────────────────────────────────────────────────────

#[test]
fn xpath_default_prints_text_content() {
    let o = run_stdin(&[OsStr::new("xpath"), OsStr::new("//title")], SAMPLE_OK);
    ok(&o, "xpath text");
    let out = stdout(&o);
    assert!(out.contains("Dune"));
    assert!(out.contains("Foundation"));
}

#[test]
fn xpath_count_prints_number() {
    let o = run_stdin(&[OsStr::new("xpath"), OsStr::new("--count"), OsStr::new("//book")],
                     SAMPLE_OK);
    ok(&o, "xpath --count");
    assert_eq!(stdout(&o).trim(), "2");
}

#[test]
fn xpath_exists_hit_returns_zero() {
    let o = run_stdin(&[OsStr::new("xpath"), OsStr::new("--exists"),
                        OsStr::new("//book[@id='2']")], SAMPLE_OK);
    ok(&o, "xpath --exists hit");
    assert!(stdout(&o).is_empty());
}

#[test]
fn xpath_exists_miss_returns_one() {
    let o = run_stdin(&[OsStr::new("xpath"), OsStr::new("--exists"),
                        OsStr::new("//nope")], SAMPLE_OK);
    assert_eq!(rc(&o), 1);
}

#[test]
fn xpath_custom_separator() {
    let o = run_stdin(&[OsStr::new("xpath"), OsStr::new("--separator"),
                        OsStr::new(", "), OsStr::new("//title")], SAMPLE_OK);
    ok(&o, "xpath --separator");
    assert_eq!(stdout(&o).trim(), "Dune, Foundation");
}

#[test]
fn xpath_nodes_serializes_element_subtree() {
    // `--nodes` should emit each matched element as XML, including
    // its attributes and children — not just the string-value the
    // default mode returns.
    let o = run_stdin(&[OsStr::new("xpath"), OsStr::new("--nodes"),
                        OsStr::new("//book")], SAMPLE_OK);
    ok(&o, "xpath --nodes //book");
    let out = stdout(&o);
    assert!(out.contains(r#"<book id="1">"#),
        "expected element start tag with attr; got: {out:?}");
    assert!(out.contains("<title>Dune</title>"),
        "expected nested children serialized; got: {out:?}");
    assert!(out.contains(r#"<book id="2">"#),
        "expected second match; got: {out:?}");
}

#[test]
fn xpath_nodes_serializes_attribute_as_name_value() {
    let o = run_stdin(&[OsStr::new("xpath"), OsStr::new("--nodes"),
                        OsStr::new("//@id")], SAMPLE_OK);
    ok(&o, "xpath --nodes //@id");
    let out = stdout(&o);
    let lines: Vec<&str> = out.trim_end().split('\n').collect();
    assert_eq!(lines, vec![r#"id="1""#, r#"id="2""#],
        "expected attribute serializations; got: {out:?}");
}

#[test]
fn xpath_nodes_escapes_attribute_value() {
    // Attribute values with `&`, `<`, `"` must be escaped on output.
    let o = run_stdin(&[OsStr::new("xpath"), OsStr::new("--nodes"),
                        OsStr::new("//@q")],
                      r#"<r q='a &amp; b &lt; c &quot;d&quot;'/>"#);
    ok(&o, "xpath --nodes escapes");
    assert!(stdout(&o).contains(r#"q="a &amp; b &lt; c &quot;d&quot;""#),
        "expected escaped attr value; got: {}", stdout(&o));
}

#[test]
fn xpath_nodes_custom_separator_between_matches() {
    let o = run_stdin(&[OsStr::new("xpath"), OsStr::new("--nodes"),
                        OsStr::new("--separator"), OsStr::new(" | "),
                        OsStr::new("//author")], SAMPLE_OK);
    ok(&o, "xpath --nodes --separator");
    let out = stdout(&o);
    assert!(out.contains("<author>Herbert</author> | <author>Asimov</author>"),
        "expected custom separator between serialized nodes; got: {out:?}");
}

// ── validate ──────────────────────────────────────────────────────────────────

const SCHEMA_PORT: &str = "\
<?xml version=\"1.0\"?>\n\
<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">\n\
  <xs:element name=\"port\" type=\"xs:int\"/>\n\
</xs:schema>\n";

#[test]
fn validate_valid_doc_succeeds_silently() {
    // Default validate is silent-on-success: exit code carries the signal.
    let schema = write_tmp("port.xsd", SCHEMA_PORT);
    let o = run_stdin(&[OsStr::new("validate"), OsStr::new("--schema"),
                        schema.as_os_str()], "<port>42</port>");
    ok(&o, "validate ok");
    assert!(stdout(&o).is_empty(), "expected silent default; got stdout: {:?}", stdout(&o));
    assert!(stderr(&o).is_empty(), "expected silent default; got stderr: {:?}", stderr(&o));
}

#[test]
fn validate_verbose_emits_valid_line_per_file() {
    let schema = write_tmp("port_v.xsd", SCHEMA_PORT);
    let xml    = write_tmp("port_v.xml", "<port>42</port>");
    let o = run(&[OsStr::new("validate"), OsStr::new("--schema"),
                  schema.as_os_str(), OsStr::new("-v"), xml.as_os_str()]);
    ok(&o, "validate -v ok");
    assert!(stdout(&o).contains("port_v.xml: valid"),
        "expected per-file 'valid' line; got: {:?}", stdout(&o));
}

#[test]
fn validate_verbose_stdin_labels_stdin() {
    let schema = write_tmp("port_v2.xsd", SCHEMA_PORT);
    let o = run_stdin(&[OsStr::new("validate"), OsStr::new("--schema"),
                        schema.as_os_str(), OsStr::new("-v")],
                      "<port>42</port>");
    ok(&o, "validate -v stdin");
    assert!(stdout(&o).contains("<stdin>: valid"),
        "expected '<stdin>: valid' in stdout; got: {:?}", stdout(&o));
}

#[test]
fn validate_quiet_overrides_verbose() {
    // `--quiet` wins so callers that inherit both stay silent.
    let schema = write_tmp("port_q.xsd", SCHEMA_PORT);
    let o = run_stdin(&[OsStr::new("-q"), OsStr::new("validate"),
                        OsStr::new("--schema"), schema.as_os_str(),
                        OsStr::new("-v")],
                      "<port>42</port>");
    ok(&o, "validate -q -v");
    assert!(stdout(&o).is_empty(), "quiet must override verbose; got: {:?}", stdout(&o));
}

#[test]
fn validate_invalid_doc_fails_with_diagnostic() {
    let schema = write_tmp("port2.xsd", SCHEMA_PORT);
    let o = run_stdin(&[OsStr::new("validate"), OsStr::new("--schema"),
                        schema.as_os_str()], "<port>abc</port>");
    assert_eq!(rc(&o), 1);
    assert!(stderr(&o).contains("abc"), "expected diagnostic referencing 'abc': {}", stderr(&o));
}

// A two-file schema where the *main* schema uses `<xs:include>` to
// reference a sibling that defines the type `Port`.  Compiling either
// file alone — or with no schema resolver wired in — fails because
// the include can't be resolved.  Used by the `--allow-fs` tests
// below to prove the CLI now wires the allowlist into XSD
// compilation, not just into entity resolution.

const SCHEMA_INCLUDE_MAIN: &str = "\
<?xml version=\"1.0\"?>\n\
<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">\n\
  <xs:include schemaLocation=\"port_types.xsd\"/>\n\
  <xs:element name=\"port\" type=\"Port\"/>\n\
</xs:schema>\n";

const SCHEMA_INCLUDE_PART: &str = "\
<?xml version=\"1.0\"?>\n\
<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">\n\
  <xs:simpleType name=\"Port\">\n\
    <xs:restriction base=\"xs:int\">\n\
      <xs:minInclusive value=\"1\"/>\n\
      <xs:maxInclusive value=\"65535\"/>\n\
    </xs:restriction>\n\
  </xs:simpleType>\n\
</xs:schema>\n";

/// Place the two schema fragments in a fresh subdirectory of the
/// test tmpdir so the `--allow-fs <dir>` we hand the resolver only
/// covers them (no cross-test pollution from CARGO_TARGET_TMPDIR).
fn write_include_schema_pair(subdir: &str) -> (PathBuf, PathBuf) {
    let root: PathBuf = env!("CARGO_TARGET_TMPDIR").into();
    let dir = root.join(subdir);
    fs::create_dir_all(&dir).unwrap();
    let main = dir.join("port_main.xsd");
    let part = dir.join("port_types.xsd");
    fs::write(&main, SCHEMA_INCLUDE_MAIN).unwrap();
    fs::write(&part, SCHEMA_INCLUDE_PART).unwrap();
    (dir, main)
}

#[test]
fn validate_xs_include_fails_without_allow_fs() {
    // Without `--allow-fs`, the schema compiler has no resolver and
    // the `<xs:include>` becomes a compile error.  The current
    // behaviour (regression target — used to silently fail the same
    // way even when `--allow-fs` WAS given).
    let (_dir, main) = write_include_schema_pair("validate_no_resolver");
    let o = run_stdin(&[OsStr::new("validate"), OsStr::new("--schema"),
                        main.as_os_str()], "<port>80</port>");
    assert_ne!(rc(&o), 0, "expected non-zero exit; stdout={:?} stderr={:?}",
        stdout(&o), stderr(&o));
    let err = stderr(&o);
    // The diagnostic should make it clear the included file couldn't
    // be loaded — either as a resolver / include / Port-undefined
    // error.  Be lenient about the exact wording; what matters is
    // that compilation failed and the user gets a hint.
    assert!(
        err.contains("port_types.xsd")
            || err.contains("include")
            || err.contains("resolve")
            || err.contains("Port"),
        "expected diagnostic mentioning the missing include or undefined type; got: {err}"
    );
}

#[test]
fn validate_xs_include_succeeds_with_allow_fs() {
    let (dir, main) = write_include_schema_pair("validate_with_allow_fs");
    let o = run_stdin(&[
        OsStr::new("--allow-fs"), dir.as_os_str(),
        OsStr::new("validate"),
        OsStr::new("--schema"), main.as_os_str(),
    ], "<port>80</port>");
    ok(&o, "validate with --allow-fs should resolve sibling include");
    // Default validate is silent on success.
    assert!(stdout(&o).is_empty(), "expected silent default; got: {:?}", stdout(&o));
}

#[test]
fn validate_xs_include_with_allow_fs_catches_real_violation() {
    // Sanity: the included `Port` type really is in force — a port
    // out of range should be rejected, not silently accepted.
    let (dir, main) = write_include_schema_pair("validate_allow_fs_catches");
    let o = run_stdin(&[
        OsStr::new("--allow-fs"), dir.as_os_str(),
        OsStr::new("validate"),
        OsStr::new("--schema"), main.as_os_str(),
    ], "<port>99999</port>");
    assert_eq!(rc(&o), 1);
    assert!(stderr(&o).contains("99999") || stderr(&o).contains("maxInclusive"),
        "expected range violation diagnostic; got: {}", stderr(&o));
}

#[test]
fn validate_xs_include_via_http_rejected_without_allow_host() {
    // A schemaLocation that points at a `http://` URL: without
    // `--allow-host`, the CLI's resolver should reject it cleanly
    // (not panic, not silently fail-open).
    let dir = {
        let root: PathBuf = env!("CARGO_TARGET_TMPDIR").into();
        let d = root.join("validate_http_rejected");
        fs::create_dir_all(&d).unwrap();
        d
    };
    let main = dir.join("http_main.xsd");
    fs::write(&main, "\
<?xml version=\"1.0\"?>\n\
<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">\n\
  <xs:include schemaLocation=\"http://invalid.example/port_types.xsd\"/>\n\
  <xs:element name=\"port\" type=\"xs:int\"/>\n\
</xs:schema>\n").unwrap();

    // With `--allow-fs` only, no network resolver is wired up, so
    // the http:// schemaLocation must surface as a permission /
    // resolve error rather than a panic or silent fall-through.
    let o = run_stdin(&[
        OsStr::new("--allow-fs"), dir.as_os_str(),
        OsStr::new("validate"),
        OsStr::new("--schema"), main.as_os_str(),
    ], "<port>80</port>");
    assert_ne!(rc(&o), 0, "expected non-zero exit; stdout={:?} stderr={:?}",
        stdout(&o), stderr(&o));
    let err = stderr(&o);
    assert!(
        err.contains("network") || err.contains("--allow-host")
            || err.contains("http") || err.contains("Permission") || err.contains("resolve"),
        "expected a clear hint about the network include being rejected; got: {err}"
    );
}

// ── repair ────────────────────────────────────────────────────────────────────

#[test]
fn repair_handles_bare_ampersand() {
    let o = run_stdin(&[OsStr::new("repair")], "<r>tom & jerry</r>");
    ok(&o, "repair bare &");
    assert!(stderr(&o).contains("bare '&'"));
    // The literal & must survive in the cleaned output as &amp;
    assert!(stdout(&o).contains("tom &amp; jerry"));
}

#[test]
fn repair_handles_unclosed_at_eof() {
    let o = run_stdin(&[OsStr::new("repair")], "<r><a>hello");
    ok(&o, "repair unclosed");
    assert!(stderr(&o).contains("unclosed element '<a>'"));
    assert!(stderr(&o).contains("unclosed element '<r>'"));
    let out = stdout(&o);
    assert!(out.contains("<a>hello</a>"));
    assert!(out.contains("</r>"));
}

#[test]
fn repair_handles_combined_bare_amp_and_unclosed() {
    let o = run_stdin(&[OsStr::new("repair")], "<r>tom & jerry<unclosed>");
    ok(&o, "repair combined");
    let err = stderr(&o);
    assert!(err.contains("bare '&'"));
    assert!(err.contains("unclosed element '<unclosed>'"));
    let out = stdout(&o);
    assert!(out.contains("tom &amp; jerry"));
}

#[test]
fn repair_well_formed_doc_emits_no_recovered_lines() {
    let o = run_stdin(&[OsStr::new("repair")], "<r><a>x</a></r>");
    ok(&o, "repair clean");
    assert!(!stderr(&o).contains("recovered:"));
    assert!(stdout(&o).contains("<a>x</a>"));
}

// ── stats ─────────────────────────────────────────────────────────────────────

#[test]
fn stats_reports_element_and_attribute_counts() {
    let o = run_stdin(&[OsStr::new("stats")], SAMPLE_OK);
    ok(&o, "stats");
    let out = stdout(&o);
    assert!(out.contains("elements:"));
    assert!(out.contains("attributes:"));
    assert!(out.contains("max depth:"));
    // catalog + 2 books + 2 titles + 2 authors = 7 elements
    assert!(out.contains("elements:     7"), "stats output:\n{out}");
}

#[test]
fn stats_json_emits_single_object_with_expected_fields() {
    let o = run_stdin(&[OsStr::new("stats"), OsStr::new("--json")], SAMPLE_OK);
    ok(&o, "stats --json");
    let out = stdout(&o);
    assert!(out.starts_with('{') && out.trim_end().ends_with('}'),
        "expected JSON object envelope; got: {out}");
    // Stable field set documented in --help.
    for field in &[
        r#""bytes":"#, r#""xml_version":"#, r#""encoding":"#,
        r#""elements":"#, r#""attributes":"#, r#""text_nodes":"#,
        r#""cdata":"#, r#""comments":"#, r#""pis":"#,
        r#""entity_refs":"#, r#""max_depth":"#, r#""text_bytes":"#,
    ] {
        assert!(out.contains(field),
            "missing field {field:?} in stats --json output: {out}");
    }
    // 7 elements in SAMPLE_OK — matches the text-mode test above.
    assert!(out.contains(r#""elements":7"#),
        "expected elements:7 in JSON; got: {out}");
}

#[test]
fn stats_json_escapes_strings() {
    // Re-emitted strings (encoding, xml_version) flow through json_str
    // and must be escaped per RFC 8259.  Easiest path is to feed
    // input with a tricky declared encoding name — we make the
    // version a non-empty string by giving a valid XML 1.0 doc.
    let o = run_stdin(
        &[OsStr::new("stats"), OsStr::new("--json")],
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><r/>",
    );
    ok(&o, "stats --json with encoding decl");
    let out = stdout(&o);
    assert!(out.contains(r#""xml_version":"1.0""#), "got: {out}");
    assert!(out.contains(r#""encoding":"UTF-8""#), "got: {out}");
}

// ── stubs and usage errors ────────────────────────────────────────────────────

// (c14n is no longer a stub — see the c14n test block above.)

#[test]
fn diff_identical_documents_exits_zero() {
    let a = write_tmp("diff_a.xml", "<r/>");
    let b = write_tmp("diff_b.xml", "<r/>");
    let o = run(&[OsStr::new("diff"), a.as_os_str(), b.as_os_str()]);
    assert_eq!(rc(&o), 0, "identical → exit 0; stderr={}", stderr(&o));
}

#[test]
fn diff_without_args_is_a_clap_usage_error() {
    // Defines the contract: `diff` now takes two required positional
    // args, so calling it bare produces a clap usage error (exit 2)
    // — same exit bucket as `not yet implemented`, but the diagnostic
    // is clap's "required argument" message, not ours.
    let o = run(&[OsStr::new("diff")]);
    assert_eq!(rc(&o), 2);
    let err = stderr(&o);
    assert!(err.contains("required") || err.contains("usage")
            || err.contains("Usage") || err.contains("LEFT"),
        "expected clap usage diagnostic mentioning the missing args; got: {err}");
}

#[test]
fn unknown_subcommand_exits_with_usage_code() {
    let o = run(&[OsStr::new("nonsense")]);
    assert!(!o.status.success());
}

#[test]
fn max_size_rejects_oversize_input() {
    let big = "<root>".to_string() + &"x".repeat(50) + "</root>";
    let o = run_stdin(&[OsStr::new("--max-size"), OsStr::new("10"),
                        OsStr::new("lint")], &big);
    assert_eq!(rc(&o), 2);
    assert!(stderr(&o).contains("max-size"));
}

#[test]
fn max_size_default_is_one_gib() {
    // Documenting the chosen default via the user-visible help text.
    // If anyone changes the constant without thinking about the
    // exposed-default contract, this test surfaces it.
    let o = run(&[OsStr::new("--help")]);
    ok(&o, "help output");
    let help = stdout(&o);
    assert!(help.contains("1073741824") || help.contains("1 GiB"),
        "expected default cap (1 GiB / 1073741824) advertised in --help; got:\n{help}");
    assert!(help.contains("0"),
        "expected `0 = disable cap` documented in --help; got:\n{help}");
}

#[test]
fn max_size_default_lets_normal_inputs_through() {
    // Sanity that the new default doesn't accidentally reject ordinary
    // documents — anything well under 1 GiB should still lint cleanly.
    let o = run_stdin(&[OsStr::new("lint")], SAMPLE_OK);
    ok(&o, "lint with default --max-size");
}

#[test]
fn max_size_zero_disables_the_cap() {
    // Explicit opt-out: `--max-size 0` is documented to mean "no
    // limit", so an input that would be rejected at any positive
    // threshold must pass when 0 is given.
    let big = "<root>".to_string() + &"x".repeat(50) + "</root>";
    let o = run_stdin(&[OsStr::new("--max-size"), OsStr::new("0"),
                        OsStr::new("lint")], &big);
    ok(&o, "lint with --max-size 0");
}

// ── streaming reader: --buffer-size and --huge ───────────────────────────────

#[test]
fn buffer_size_and_huge_are_mutually_exclusive() {
    // clap `conflicts_with` should reject the combination at
    // arg-parse time with a usage error (exit 2).
    let o = run_stdin(&[OsStr::new("--buffer-size"), OsStr::new("16777216"),
                        OsStr::new("--huge"),
                        OsStr::new("lint")], "<r/>");
    assert_eq!(rc(&o), 2);
    let err = stderr(&o);
    assert!(err.contains("cannot be used with") || err.contains("conflicts"),
        "expected clap conflict message; got: {err}");
}

#[test]
fn lint_streams_large_input_with_small_buffer() {
    // ~10 KiB input through a 1 KiB streaming buffer.  The wrapper
    // must refill / compact / rebind many times without losing
    // state.  All elements are small, so the per-token buffer cap
    // is never hit.
    let mut doc = String::from("<root>");
    for i in 0..500 {
        doc.push_str(&format!("<item id=\"{i}\">value</item>"));
    }
    doc.push_str("</root>");
    let o = run_stdin(&[OsStr::new("--buffer-size"), OsStr::new("1024"),
                        OsStr::new("lint")], &doc);
    ok(&o, "lint with --buffer-size 1024 on ~14 KiB doc");
}

#[test]
fn lint_errors_on_token_exceeding_buffer() {
    // A single element name larger than the internal buffer
    // (buffer_size × 2 — see BUF_CAPACITY_MULTIPLE) is the
    // documented streaming limitation.  Should fail with a parse
    // error (exit 1), not crash.  Buffer 1024 → internal cap 2048
    // → 4000-byte name exceeds it.
    let big_name = "a".repeat(4000);
    let doc = format!("<{big_name}/>");
    let o = run_stdin(&[OsStr::new("--buffer-size"), OsStr::new("1024"),
                        OsStr::new("lint")], &doc);
    assert_eq!(rc(&o), 1, "expected parse error; stderr={}", stderr(&o));
}

#[test]
fn huge_flag_accepts_large_token() {
    // The same input that fails with a 1 KiB buffer succeeds with
    // --huge (1 GiB buffer).  Sanity that --huge actually wires
    // through to a much larger buffer.
    let big_name = "a".repeat(4000);
    let doc = format!("<{big_name}/>");
    // First confirm baseline: --buffer-size 1024 fails.
    let bad = run_stdin(&[OsStr::new("--buffer-size"), OsStr::new("1024"),
                          OsStr::new("lint")], &doc);
    assert_eq!(rc(&bad), 1);
    // Now with --huge it should succeed.
    let good = run_stdin(&[OsStr::new("--huge"), OsStr::new("lint")], &doc);
    ok(&good, "lint --huge with large element name");
}

#[test]
fn streaming_lint_truncated_xml_errors_cleanly() {
    // Truncated input — no closing `</root>`.  Should produce a
    // parse error (exit 1), not panic or hang.
    let o = run_stdin(&[OsStr::new("lint")], "<root><a/>");
    assert_eq!(rc(&o), 1);
    assert!(!stderr(&o).is_empty(), "expected a diagnostic on stderr");
}

#[test]
fn nonexistent_file_returns_run_error_code() {
    // Under Convention C the I/O-failure code collapses into the
    // generic "run itself failed" bucket (2), distinct from the
    // negative-result bucket (1) that lint uses for malformed
    // input.  See `lint_vs_io_error_use_different_exit_codes` for
    // the discrimination test.
    let missing = Path::new("/this/path/should/not/exist.xml");
    let o = run(&[OsStr::new("lint"), missing.as_os_str()]);
    assert_eq!(rc(&o), 2);
}

#[test]
fn lint_vs_io_error_use_different_exit_codes() {
    // The point of Convention C: a script can distinguish "the
    // thing we asked about returned a negative result" from "the
    // run itself fell over" without parsing stderr.
    let bad = write_tmp("conv_c_bad.xml", SAMPLE_MALFORMED);
    let lint_fail = run(&[OsStr::new("lint"), bad.as_os_str()]);
    assert_eq!(rc(&lint_fail), 1, "malformed input → negative-result code");

    let missing = Path::new("/this/path/should/not/exist.xml");
    let io_fail = run(&[OsStr::new("lint"), missing.as_os_str()]);
    assert_eq!(rc(&io_fail), 2, "missing file → run-error code");
}

#[test]
fn validate_distinguishes_xsd_violation_from_unparseable_schema() {
    // XSD violation in the instance → 1 (the data is bad, but the
    // run worked).  Unparseable schema → 2 (we couldn't even get
    // to the point of validating).  Used to both collapse to 1.
    let good_schema = write_tmp("conv_c_port_good.xsd", SCHEMA_PORT);
    let bad_schema  = write_tmp("conv_c_port_bad.xsd",  "<not even xml");

    let violation = run_stdin(
        &[OsStr::new("validate"), OsStr::new("--schema"), good_schema.as_os_str()],
        "<port>abc</port>",
    );
    assert_eq!(rc(&violation), 1,
        "XSD violation should be negative-result (1); stderr={}", stderr(&violation));

    let bad_schema_run = run_stdin(
        &[OsStr::new("validate"), OsStr::new("--schema"), bad_schema.as_os_str()],
        "<port>80</port>",
    );
    assert_eq!(rc(&bad_schema_run), 2,
        "unparseable schema should be run-error (2); stderr={}", stderr(&bad_schema_run));
}

#[test]
fn format_check_vs_parse_error_both_use_negative_code() {
    // Both are negative-result outcomes — clean 1 either way.
    let dirty = write_tmp("conv_c_dirty.xml", "<r a='1'/>");
    let check = run(&[OsStr::new("format"), OsStr::new("--check"), dirty.as_os_str()]);
    assert_eq!(rc(&check), 1, "--check mismatch is negative-result");

    let parse_fail = run_stdin(&[OsStr::new("format")], "<r><b></r>");
    assert_eq!(rc(&parse_fail), 1, "parse failure is negative-result");
}

#[test]
fn diff_differing_documents_exits_one() {
    let a = write_tmp("dx_a.xml", "<r><x/></r>");
    let b = write_tmp("dx_b.xml", "<r><y/></r>");
    let o = run(&[OsStr::new("diff"), a.as_os_str(), b.as_os_str()]);
    assert_eq!(rc(&o), 1, "differing → exit 1; stderr={}", stderr(&o));
}

// ── --xinclude flag ──────────────────────────────────────────────────────────

#[test]
fn xinclude_resolves_relative_href_against_input_dir() {
    let frag = write_tmp("xi_frag.xml", "<piece><inner>hi</inner></piece>");
    let main_xml = format!(
        r#"<?xml version="1.0"?>
<doc xmlns:xi="http://www.w3.org/2001/XInclude">
  <xi:include href="{}"/>
</doc>"#,
        frag.file_name().unwrap().to_str().unwrap(),
    );
    let main = write_tmp("xi_main.xml", &main_xml);
    let allow = frag.parent().unwrap();
    let o = run(&[
        OsStr::new("format"),
        OsStr::new("--xinclude"),
        OsStr::new("--allow-fs"), allow.as_os_str(),
        main.as_os_str(),
    ]);
    ok(&o, "format --xinclude");
    let out = stdout(&o);
    assert!(out.contains("<inner>hi</inner>"), "expected included content; got: {out}");
    assert!(!out.contains("xi:include"), "xi:include element should be gone; got: {out}");
}

#[test]
fn xinclude_off_by_default_leaves_xi_include_intact() {
    let main_xml = r#"<?xml version="1.0"?>
<doc xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="anywhere.xml"/></doc>"#;
    let main = write_tmp("xi_passthrough.xml", main_xml);
    let o = run(&[OsStr::new("format"), main.as_os_str()]);
    ok(&o, "format without --xinclude");
    assert!(stdout(&o).contains("xi:include"), "xi:include should pass through");
}

#[test]
fn xinclude_conflicts_with_html() {
    let main = write_tmp("xi_html.xml", "<r/>");
    let o = run(&[
        OsStr::new("--xinclude"), OsStr::new("--html"),
        OsStr::new("format"), main.as_os_str(),
    ]);
    assert_ne!(rc(&o), 0, "should reject --xinclude + --html");
    assert!(stderr(&o).contains("cannot be used with"),
        "clap should refuse; got: {}", stderr(&o));
}

// ── --html flag ──────────────────────────────────────────────────────────────

const SAMPLE_HTML: &str = "<!DOCTYPE html><html><body><br><p>hi <b>x</b></p><img src=/x.png></body></html>";

#[test]
fn html_lint_accepts_html_with_void_elements_and_unquoted_attrs() {
    // This input is malformed XML (unquoted attr, unclosed <br>) but
    // valid HTML.  --html flag should accept it; without the flag,
    // strict XML lint should reject.
    let f = write_tmp("page.html", SAMPLE_HTML);
    let o = run(&[OsStr::new("--html"), OsStr::new("lint"), OsStr::new("-v"), f.as_os_str()]);
    ok(&o, "html lint should accept page.html");
    assert!(stdout(&o).contains("OK"));
}

#[test]
fn html_format_round_trips_to_html_shape() {
    let o = run_stdin(&[OsStr::new("--html"), OsStr::new("format")], SAMPLE_HTML);
    ok(&o, "html print");
    let out = stdout(&o);
    // DOCTYPE preserved.
    assert!(out.contains("<!DOCTYPE html>"), "missing DOCTYPE: {out}");
    // Void <br> emitted without self-close.
    assert!(out.contains("<br>"), "missing <br>: {out}");
    assert!(!out.contains("<br/>") && !out.contains("<br />"));
    // No XML declaration.
    assert!(!out.contains("<?xml"), "must not emit XML decl: {out}");
}

#[test]
fn html_xpath_counts_links() {
    let html = "<html><body><a href=/x>1</a><a href=/y>2</a><a>n</a></body></html>";
    let o = run_stdin(
        &[OsStr::new("--html"), OsStr::new("xpath"), OsStr::new("--count"), OsStr::new("//a[@href]")],
        html,
    );
    ok(&o, "html xpath --count");
    assert_eq!(stdout(&o).trim(), "2");
}

#[test]
fn html_stats_walks_tree() {
    let o = run_stdin(&[OsStr::new("--html"), OsStr::new("stats")], SAMPLE_HTML);
    ok(&o, "html stats");
    let out = stdout(&o);
    // Implicit html/head/body insertion + <br>, <p>, <b>, <img> = 7
    // elements.  Asserting on the line shape rather than the count
    // so html5ever changes don't break us.
    assert!(out.contains("elements:"), "stats output: {out}");
    assert!(out.contains("max depth:"), "stats output: {out}");
}

#[test]
fn html_repair_works_on_tag_soup() {
    // Mismatched closes — html5ever recovers; repair should produce
    // a serialised tree.
    let html = "<p><b>oops</p></b>";
    let o = run_stdin(&[OsStr::new("--html"), OsStr::new("repair")], html);
    ok(&o, "html repair");
    // We don't pin the exact output (depends on html5ever's
    // adoption-agency choices), just that something was emitted.
    assert!(!stdout(&o).is_empty());
}

// ── c14n ─────────────────────────────────────────────────────────────────────

#[test]
fn c14n_canonicalizes_simple_document() {
    let o = run_stdin(&[OsStr::new("c14n")], "<r a='1' xmlns:b='urn:b'><b:x>hi</b:x></r>");
    ok(&o, "c14n");
    let out = stdout(&o);
    // Canonical form: explicit close tag, double-quoted attrs,
    // namespace decl moved to come before regular attrs.
    assert_eq!(out, r#"<r xmlns:b="urn:b" a="1"><b:x>hi</b:x></r>"#);
}

#[test]
fn c14n_drops_xml_declaration() {
    let o = run_stdin(&[OsStr::new("c14n")], r#"<?xml version="1.0"?><r/>"#);
    ok(&o, "c14n drops xml decl");
    assert_eq!(stdout(&o), "<r></r>");
}

#[test]
fn c14n_with_comments() {
    let o = run_stdin(
        &[OsStr::new("c14n"), OsStr::new("--comments")],
        "<r><!-- hi --></r>",
    );
    ok(&o, "c14n --comments");
    assert_eq!(stdout(&o), "<r><!-- hi --></r>");
}

#[test]
fn c14n_omits_comments_by_default() {
    let o = run_stdin(&[OsStr::new("c14n")], "<r><!-- hi --></r>");
    ok(&o, "c14n no comments");
    assert_eq!(stdout(&o), "<r></r>");
}

#[test]
fn c14n_exclusive_mode_omits_unused_namespace() {
    // outer declares xmlns:a; inner doesn't use it.  C14N 1.0 would
    // render xmlns:a on outer; exc-c14n drops it because outer
    // doesn't visibly use `a` either.
    let o = run_stdin(
        &[OsStr::new("c14n"), OsStr::new("--exclusive")],
        r#"<a:outer xmlns:a="urn:a"><inner/></a:outer>"#,
    );
    ok(&o, "c14n --exclusive");
    let out = stdout(&o);
    // xmlns:a should appear because outer's element name uses it.
    assert!(out.contains(r#"xmlns:a="urn:a""#), "got: {out}");
    // inner should not have xmlns:a.
    assert!(!out.contains(r#"<inner xmlns:a"#), "got: {out}");
}

#[test]
fn c14n_inclusive_prefixes_requires_exclusive() {
    // --inclusive-prefixes only makes sense with --exclusive; clap
    // should reject the combination.
    let o = run_stdin(
        &[
            OsStr::new("c14n"),
            OsStr::new("--inclusive-prefixes"),
            OsStr::new("a"),
        ],
        "<r/>",
    );
    assert!(!o.status.success(), "should fail without --exclusive");
}

#[test]
fn c14n_writes_bytes_not_string() {
    // Output is bytes, not a String.  Specifically, the canonical
    // form does NOT add a trailing newline (unlike e.g. `print`).
    let o = run_stdin(&[OsStr::new("c14n")], "<r/>");
    ok(&o, "c14n bytes");
    assert_eq!(stdout(&o), "<r></r>");
    // No trailing newline.
    assert!(!stdout(&o).ends_with('\n'));
}

#[test]
fn html_lint_without_flag_rejects_html() {
    // Without --html, the malformed-as-XML input should be rejected.
    let o = run_stdin(&[OsStr::new("lint")], SAMPLE_HTML);
    assert_ne!(rc(&o), 0, "strict XML lint should reject HTML-shape input");
}

// `<!DOCTYPE html5>` is a tokeniser error in html5ever's strict mode
// (the spec only knows `html`).  Browsers — and our default lenient
// HTML parse — recover silently; `--strict-html` makes it fatal.
const HTML_BAD_DOCTYPE: &str = "<!DOCTYPE html5><html><body>hi</body></html>";

#[test]
fn html_lint_default_recovers_from_bad_doctype() {
    let o = run_stdin(&[OsStr::new("--html"), OsStr::new("lint")], HTML_BAD_DOCTYPE);
    ok(&o, "default --html should recover from bad DOCTYPE");
}

#[test]
fn html_lint_strict_rejects_bad_doctype() {
    let o = run_stdin(
        &[OsStr::new("--html"), OsStr::new("--strict-html"), OsStr::new("lint")],
        HTML_BAD_DOCTYPE,
    );
    assert_eq!(rc(&o), 1, "strict mode should reject; stderr={}", stderr(&o));
    assert!(stderr(&o).to_lowercase().contains("doctype"),
        "expected DOCTYPE-related diagnostic; got: {}", stderr(&o));
}

#[test]
fn strict_html_without_html_flag_is_usage_error() {
    // `--strict-html` has `requires = "html"` on the clap def.  Using
    // it without `--html` should be a usage error before any parsing
    // happens.
    let o = run_stdin(&[OsStr::new("--strict-html"), OsStr::new("lint")], SAMPLE_OK);
    assert_eq!(rc(&o), 2, "expected usage error; stderr={}", stderr(&o));
}

// ── resolver allowlist (--allow-fs / --allow-host) ───────────────────────────

#[test]
fn allow_http_without_allow_host_is_rejected() {
    // clap `requires` on --allow-http should fail at arg-parse time
    // when no --allow-host was given.
    let o = run(&[OsStr::new("--allow-http"), OsStr::new("lint"), OsStr::new("/dev/null")]);
    assert_eq!(rc(&o), 2, "--allow-http without --allow-host must be a usage error");
    assert!(stderr(&o).contains("--allow-host"));
}

#[test]
fn allow_fs_resolves_external_entity_inside_allowlisted_dir() {
    let dir = tmp("ent_ok").join("d");
    fs::create_dir_all(&dir).unwrap();
    let ent_path = dir.join("hello.txt");
    fs::write(&ent_path, "hello world").unwrap();
    let doc = format!(
        "<?xml version=\"1.0\"?>\n\
         <!DOCTYPE r [<!ENTITY ext SYSTEM \"file://{}\">]>\n\
         <r>&ext;</r>\n",
        ent_path.display()
    );
    let o = run_stdin(
        &[OsStr::new("--allow-fs"), dir.as_os_str(), OsStr::new("format")],
        &doc,
    );
    ok(&o, "external entity should resolve with --allow-fs");
    assert!(stdout(&o).contains("hello world"), "stdout={}", stdout(&o));
}

#[test]
fn no_resolver_refuses_external_entity() {
    let dir = tmp("ent_blocked").join("d");
    fs::create_dir_all(&dir).unwrap();
    let ent_path = dir.join("hello.txt");
    fs::write(&ent_path, "hello world").unwrap();
    let doc = format!(
        "<?xml version=\"1.0\"?>\n\
         <!DOCTYPE r [<!ENTITY ext SYSTEM \"file://{}\">]>\n\
         <r>&ext;</r>\n",
        ent_path.display()
    );
    // No --allow-fs and no --allow-host => external_resolver is None.
    // The reference to &ext; must not yield the file's contents.
    let o = run_stdin(&[OsStr::new("format")], &doc);
    assert!(
        !stdout(&o).contains("hello world"),
        "external content must not leak without an allowlist: stdout={}",
        stdout(&o),
    );
}

// ── non-UTF-8 input encodings ────────────────────────────────────────────────
//
// XML declares its own encoding; the library's parsers detect it from
// the BOM or the `<?xml encoding="..."?>` declaration.  The CLI now
// passes raw bytes through, so a correctly-encoded UTF-16LE document
// (BOM + matching decl) parses just as well as one in UTF-8.  The
// previous behaviour rejected it at `read_input` with "input is not
// UTF-8" before the parser saw anything.

/// Encode `s` as UTF-16LE with a leading byte-order mark.  Used to
/// build fixture bytes for the encoding-aware subcommands.
fn utf16le_bom(s: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for u in s.encode_utf16() {
        out.push((u & 0xFF) as u8);
        out.push((u >> 8) as u8);
    }
    out
}

fn write_tmp_bytes(name: &str, bytes: &[u8]) -> PathBuf {
    let p = tmp(name);
    fs::write(&p, bytes).unwrap();
    p
}

const UTF16_DOC: &str = "\
<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
<root><greeting>hello</greeting><count>42</count></root>";

#[test]
fn lint_accepts_utf16le_with_bom() {
    let p = write_tmp_bytes("utf16_ok.xml", &utf16le_bom(UTF16_DOC));
    let o = run(&[OsStr::new("lint"), p.as_os_str()]);
    ok(&o, "lint UTF-16LE");
    assert!(stdout(&o).is_empty(), "expected silent default; got: {:?}", stdout(&o));
}

#[test]
fn stats_handles_utf16le_input() {
    let p = write_tmp_bytes("utf16_stats.xml", &utf16le_bom(UTF16_DOC));
    let o = run(&[OsStr::new("stats"), p.as_os_str()]);
    ok(&o, "stats UTF-16LE");
    let out = stdout(&o);
    // root + greeting + count
    assert!(out.contains("elements:     3"), "elements count: {out:?}");
}

#[test]
fn xpath_handles_utf16le_input() {
    let p = write_tmp_bytes("utf16_xpath.xml", &utf16le_bom(UTF16_DOC));
    let o = run(&[OsStr::new("xpath"), OsStr::new("//greeting"), p.as_os_str()]);
    ok(&o, "xpath UTF-16LE");
    assert!(stdout(&o).contains("hello"), "expected text content; got: {:?}", stdout(&o));
}

#[test]
fn validate_handles_utf16le_input() {
    let schema = write_tmp("port_utf16.xsd", SCHEMA_PORT);
    let p = write_tmp_bytes(
        "utf16_validate.xml",
        &utf16le_bom("<?xml version=\"1.0\" encoding=\"UTF-16\"?><port>80</port>"),
    );
    let o = run(&[OsStr::new("validate"), OsStr::new("--schema"),
                  schema.as_os_str(), p.as_os_str()]);
    ok(&o, "validate UTF-16LE");
}

#[test]
fn format_reencodes_utf16le_to_utf8() {
    // `format` always emits UTF-8 (the canonical form), so a UTF-16LE
    // input round-trips through the parser and comes back out as
    // UTF-8 bytes.  Confirms the bytes flow end-to-end and the
    // re-serializer doesn't lose the content.
    let p = write_tmp_bytes("utf16_format.xml", &utf16le_bom(UTF16_DOC));
    let o = run(&[OsStr::new("format"), p.as_os_str()]);
    ok(&o, "format UTF-16LE");
    let out = stdout(&o);
    assert!(out.contains("<greeting>hello</greeting>"),
        "expected re-emitted UTF-8 output; got: {out:?}");
    assert!(out.contains("<count>42</count>"),
        "expected second element preserved; got: {out:?}");
}

#[test]
fn lint_rejects_garbage_bytes_clearly() {
    // A buffer that isn't any text encoding at all should fail with
    // a parse-flavoured diagnostic, not a panic.  Confirms the
    // bytes-path doesn't swallow garbage silently.
    let p = write_tmp_bytes("garbage.xml", &[0xFE, 0xED, 0xFA, 0xCE, 0xDE, 0xAD, 0xBE, 0xEF]);
    let o = run(&[OsStr::new("lint"), p.as_os_str()]);
    assert_ne!(rc(&o), 0, "expected non-zero exit; stdout={:?} stderr={:?}",
        stdout(&o), stderr(&o));
    assert!(!stderr(&o).is_empty(), "expected a diagnostic, got empty stderr");
}

// ── license gate ────────────────────────────────────────────────────────────

#[test]
fn gated_command_is_blocked_without_a_license() {
    let xml = write_tmp("gate_no_license.xml", SAMPLE_OK);
    let o = bin_unlicensed().arg("lint").arg(&xml).output().unwrap();
    assert_eq!(o.status.code(), Some(2), "stderr={}", stderr(&o));
    assert!(
        stderr(&o).contains("valid license is required"),
        "expected the gate message; stderr={}",
        stderr(&o)
    );
}

#[test]
fn gated_command_runs_with_a_valid_license() {
    // `run` goes through `bin()`, which supplies the license fixture.
    let xml = write_tmp("gate_with_license.xml", SAMPLE_OK);
    let o = run(&[OsStr::new("lint"), xml.as_os_str()]);
    ok(&o, "lint should succeed with a valid license");
}

#[test]
fn help_is_not_gated() {
    let o = bin_unlicensed().arg("--help").output().unwrap();
    assert_eq!(o.status.code(), Some(0), "stderr={}", stderr(&o));
}

#[test]
fn license_command_is_not_gated() {
    // With no license the `license` command still runs (it isn't gated);
    // it reports "not valid" and exits 1 — not the gate's exit 2.
    let o = bin_unlicensed().arg("license").output().unwrap();
    assert_eq!(o.status.code(), Some(1), "stderr={}", stderr(&o));
    assert!(stderr(&o).contains("not valid"), "stderr={}", stderr(&o));
    assert!(
        !stderr(&o).contains("valid license is required"),
        "the license command must not be gated; stderr={}",
        stderr(&o)
    );
}

#[test]
fn license_command_shows_info_with_a_valid_license() {
    let o = bin().arg("license").output().unwrap();
    assert_eq!(o.status.code(), Some(0), "stderr={}", stderr(&o));
    // The certificate binds to the `sup-xml` project slug, which the command
    // prints alongside the organization it was issued to.
    assert!(stdout(&o).contains("sup-xml"), "stdout={}", stdout(&o));
    assert!(stdout(&o).contains("Test Co"), "stdout={}", stdout(&o));
}
