//! `sup-xml` — command-line tool for the SupXML library.
//!
//! Subcommands: `lint`, `format`, `xpath`, `validate`, `repair`,
//! `stats`, `c14n`.  Stubs for `diff` are reserved for when the
//! underlying engine lands.

#![forbid(unsafe_code)]

use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Args, Parser, Subcommand};

// M2: the entire library — XML and HTML, parse and serialize — runs on the
// arena DOM.  CLI consumes the top-level `sup_xml::*` surface; no legacy
// fallbacks remain.
use std::sync::Arc;
use sup_xml::{
    canonicalize_to_bytes,
    parse_bytes, parse_bytes_with_recovered,
    parse_html_bytes_opts, parse_html_bytes_with_recovered,
    process_xincludes,
    serialize_to_string, serialize_with, serialize_html_to_string,
    C14nMode, CanonicalizeOptions, ChainedResolver, EntityResolver,
    FilesystemResolver, HtmlParseOptions, NetworkResolver, NodeKind,
    ParseOptions, SerializeOptions, XIncludeOptions, XPathContext, XPathValue,
    XmlByteStreamReader, XmlError, HUGE_BUFFER_SIZE,
};
use sup_xml::Document;
use sup_xml::xsd::{FsResolver, Schema, SchemaResolver};
use sup_xml::ResolveError;

// ── exit codes ────────────────────────────────────────────────────────────────
//
// Convention C (rustfmt / prettier / eslint / tsc):
//
// * `0` — clean / success
// * `1` — the thing tested returned a negative result.  The run itself
//         worked; what it produced is the bad news:
//           - `lint`:        well-formedness errors
//           - `validate`:    XSD violations
//           - `format --check`:   input not in canonical form
//           - `xpath --exists`:   nodeset empty / false
// * `2` — the run itself failed (couldn't get to the point of an
//         answer):
//           - bad CLI args / unknown flag
//           - file not found, permission denied, other I/O
//           - schema source unparseable

const EXIT_OK:       u8 = 0;
const EXIT_NEGATIVE: u8 = 1;
const EXIT_ERROR:    u8 = 2;

// ── top-level CLI ─────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "sup-xml",
    version,
    about = "Memory-safe, spec-compliant XML CLI",
    long_about = "sup-xml — lint, format, query, validate, and repair XML \
                  documents using the SupXML library."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    #[command(flatten)]
    global: GlobalOpts,
}

#[derive(Args, Clone)]
struct GlobalOpts {
    /// Turn on recovery mode (continue past non-fatal errors).
    #[arg(short = 'r', long, global = true)]
    recover: bool,

    /// Print only on failure.
    #[arg(short = 'q', long, global = true)]
    quiet: bool,

    /// Permit DTD / entity / schema fetches from HOST over HTTPS.
    /// Repeatable.  Without any `--allow-host` flags, network fetches
    /// are refused.  Hostnames are matched exactly (no wildcards).
    #[arg(long, global = true, value_name = "HOST")]
    allow_host: Vec<String>,

    /// Permit DTD / entity / schema fetches from DIR on the local
    /// filesystem.  Repeatable.  Without any `--allow-fs` flags,
    /// filesystem fetches are refused.  Each DIR is the security
    /// boundary; never pass `/`.
    #[arg(long, global = true, value_name = "DIR")]
    allow_fs: Vec<PathBuf>,

    /// With `--allow-host`: permit `http://` URLs (default: HTTPS only).
    /// Almost always wrong; use only for testing or air-gapped networks.
    #[arg(long, global = true, requires = "allow_host")]
    allow_http: bool,

    /// With `--allow-host`: permit URLs that resolve to RFC 1918 /
    /// loopback / link-local IPs.  Default: refused for SSRF defense.
    /// Use only when your `--allow-host` set already constrains to
    /// trusted internal hosts.
    #[arg(long, global = true, requires = "allow_host")]
    allow_private_ips: bool,

    /// Reject inputs larger than this many bytes.  Default 1 GiB
    /// (1_073_741_824).  Pass `0` to disable the cap entirely —
    /// useful when piping known-trusted multi-GB streams, but a
    /// foot-gun against untrusted input, so it's opt-in.
    #[arg(long, global = true, default_value_t = 1_073_741_824, value_name = "BYTES")]
    max_size: u64,

    /// Working buffer size for the streaming reader (used by
    /// `lint`).  Caps the largest single XML token (text node
    /// notwithstanding — text content is split across events,
    /// but element names, attribute values, comments, and PIs
    /// must fit within this size).  Default 10 MiB — matches
    /// libxml2's `XML_MAX_TEXT_LENGTH`.  Larger sizes cost more
    /// RAM but accept larger tokens.  Conflicts with `--huge`.
    #[arg(long, global = true, value_name = "BYTES",
          default_value_t = 10 * 1024 * 1024,
          conflicts_with = "huge")]
    buffer_size: u64,

    /// Shortcut for `--buffer-size 1073741824` (1 GiB) — matches
    /// libxml2's `XML_PARSE_HUGE` (lxml's `huge_tree=True`).  Use
    /// when parsing inputs with unusually large tokens (embedded
    /// base64 blobs in SVG, OOXML packages, etc.).  Conflicts
    /// with `--buffer-size`.
    #[arg(long, global = true, conflicts_with = "buffer_size")]
    huge: bool,

    /// Print elapsed wall time to stderr.
    #[arg(long, global = true)]
    timing: bool,

    /// Parse input as HTML5 (lenient, browser-equivalent recovery)
    /// rather than strict XML.  Affects `lint`, `format`, `xpath`,
    /// `repair`, and `stats`.  When set, `format` / `repair` also emit
    /// HTML-shape output (void elements as `<br>`, raw script/style,
    /// boolean attribute shorthand) instead of XML.
    ///
    /// Equivalent to libxml2's `xmllint --html`.
    #[arg(long, global = true)]
    html: bool,

    /// With `--html`: disable browser-style recovery, so a
    /// tokenisation error becomes a hard parse failure.  Useful with
    /// `lint --html --strict-html` to ask "is this actually valid
    /// HTML5?" instead of "could a browser render it?"  Without
    /// `--html` this flag has no effect (strict XML is already the
    /// default for XML inputs).
    #[arg(long, global = true, requires = "html")]
    strict_html: bool,

    /// Resolve `<xi:include>` elements after parsing, replacing each
    /// with the referenced resource's content (W3C XInclude 1.0).
    /// Fetches go through the same `--allow-fs` / `--allow-host`
    /// allowlists that govern DTD / entity resolution; without either,
    /// any `xi:include` carrying an `href` falls back or errors.
    ///
    /// Applies to subcommands that build a document tree (`format`,
    /// `xpath`, `xslt`, `validate`, `diff`, `c14n`, `stats`).  Has no
    /// effect under `--html` (XInclude is XML-only) and on streaming
    /// commands (`lint`, `repair`).
    #[arg(long, global = true, conflicts_with = "html")]
    xinclude: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Parse + re-emit.  Default compact (normalizes quoting,
    /// escaping, XML decl); `--pretty` for indented output.
    /// `--check` for a CI gate; `--in-place` to rewrite the source.
    ///
    /// Exit: 0 = success (or already-canonical with `--check`);
    /// 1 = `--check` mismatch or parse failure; 2 = run itself
    /// failed (bad args, I/O error).
    #[command(alias = "print")]
    Format(FormatArgs),

    /// Check well-formedness without producing output.  Silent on
    /// success (exit 0); writes diagnostics to stderr on failure.
    /// Use `--verbose` to print a per-file `OK` line.
    ///
    /// Exit: 0 = all inputs well-formed; 1 = at least one input
    /// failed well-formedness; 2 = run itself failed (bad args,
    /// file not found, I/O error).
    Lint(LintArgs),

    /// Evaluate an XPath 1.0 expression.
    ///
    /// Exit: 0 = success (or non-empty result with `--exists`);
    /// 1 = parse failure or `--exists` miss; 2 = run itself failed
    /// (bad args, I/O error).
    Xpath(XpathArgs),

    /// Validate against an XML Schema (XSD).  Silent on success
    /// (exit 0); writes per-issue diagnostics to stderr on failure.
    /// Use `--verbose` to print a per-file `FILE: valid` line.
    ///
    /// Exit: 0 = every instance valid; 1 = at least one instance
    /// failed validation; 2 = run itself failed (schema source
    /// unparseable, bad args, I/O error).
    Validate(ValidateArgs),

    /// Apply an XSLT 1.0 stylesheet to an XML input.
    ///
    /// Reads the input XML, compiles the stylesheet (resolving
    /// `xsl:include` / `xsl:import` / `document(...)` URIs relative to
    /// the stylesheet's directory), applies it, and writes the result
    /// to stdout or `-o FILE`.
    ///
    /// Exit: 0 = transform succeeded;
    /// 1 = stylesheet or input failed to parse / apply;
    /// 2 = run itself failed (bad args, I/O error).
    Xslt(XsltArgs),

    /// Print document statistics (sizes, depths, counts).
    Stats(StatsArgs),

    /// Parse in recovery mode and write the cleaned document.
    Repair(RepairArgs),

    /// Canonicalize (W3C Canonical XML 1.0 / Exclusive C14N 1.0).
    /// Used as the primitive under XML digital signatures (XML-DSig,
    /// SAML, WS-Security, EU eIDAS / XAdES).
    C14n(C14nArgs),

    /// Structural diff between two documents: walks both trees and
    /// reports added, removed, and changed elements, attributes, and
    /// text.  `--json` emits machine-readable diff records.
    Diff(DiffArgs),

    /// Show the active license: locate and verify a license
    /// certificate and print who it's issued to and when it expires.
    ///
    /// With no `--path`, searches the default
    /// `.supso/license_certificates/` locations (and the `SUPSO_LICENSE_PATH`
    /// environment variable).
    ///
    /// Exit: 0 = a valid license was found; 1 = none found or the
    /// certificate failed verification; 2 = run itself failed.
    License(LicenseArgs),
}

// ── arg groups ────────────────────────────────────────────────────────────────

#[derive(Args)]
struct LintArgs {
    /// One or more files; reads stdin when none given (or `-`).
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,

    /// Report every file (default: stop at the first failure).
    /// Implied by `--json` so the report covers every input.
    #[arg(long)]
    keep_going: bool,

    /// Print a `FILE: OK` line per successful file.  Default is
    /// silent-on-success — matching `make`, `ruff`, `rustfmt --check`,
    /// `tsc --noEmit`, where success is signaled by exit status alone.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Emit a JSON array of `{file, ok, error?}` records — one per
    /// input — instead of human-readable output.  Implies
    /// `--keep-going` so the report is complete.  Exit code is
    /// unchanged: 0 iff every file passed.
    #[arg(long, conflicts_with = "verbose")]
    json: bool,
}

#[derive(Args)]
struct FormatArgs {
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Pretty-print: insert newlines and indentation between
    /// elements.  Default is compact (no extra whitespace beyond
    /// what's in the document).
    #[arg(long)]
    pretty: bool,

    /// Spaces per indent level when `--pretty` is on.  Ignored
    /// otherwise.
    #[arg(long, default_value_t = 2, value_name = "N")]
    indent: usize,

    /// Use tabs instead of spaces for indentation when `--pretty`
    /// is on.  Ignored otherwise.
    #[arg(long, conflicts_with = "indent")]
    indent_tabs: bool,

    /// Omit the `<?xml ... ?>` declaration.  In `--html` mode the
    /// declaration is always omitted regardless of this flag.
    #[arg(long)]
    no_xml_decl: bool,

    /// Write to FILE instead of stdout.
    #[arg(short = 'o', long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Overwrite the input file in place.
    #[arg(short = 'i', long, conflicts_with = "output", requires = "file")]
    in_place: bool,

    /// Exit non-zero if the input doesn't already match what
    /// `print` would emit (CI gate — works in both compact and
    /// `--pretty` modes).
    #[arg(long, conflicts_with_all = ["output", "in_place"])]
    check: bool,
}

#[derive(Args)]
struct XpathArgs {
    /// The XPath 1.0 expression.
    expr: String,

    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Print only the count of matched nodes.
    #[arg(long, conflicts_with_all = ["exists", "nodes"])]
    count: bool,

    /// Exit 0 if any node matched, 1 otherwise; no output.
    #[arg(long, conflicts_with_all = ["count", "nodes"])]
    exists: bool,

    /// Print matched nodes serialized as XML (default: text content).
    #[arg(long)]
    nodes: bool,

    /// Separator between matches (default: newline).
    #[arg(long, default_value = "\n", value_name = "SEP")]
    separator: String,
}

#[derive(Args)]
struct XsltArgs {
    /// The XSLT 1.0 stylesheet file.
    #[arg(short = 's', long, value_name = "STYLESHEET")]
    stylesheet: PathBuf,

    /// The XML input.  Reads stdin when omitted (or `-`).
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Write the result to this file (default: stdout).
    #[arg(short = 'o', long, value_name = "OUT")]
    output: Option<PathBuf>,

    /// Set a top-level `xsl:param` to a string value.  Repeat the
    /// flag for each param (`--param a=1 --param b=2`).  Each
    /// invocation takes exactly one `NAME=VALUE` argument so a
    /// trailing positional input file isn't accidentally consumed.
    #[arg(long, value_name = "NAME=VALUE")]
    param: Vec<String>,
}

#[derive(Args)]
struct ValidateArgs {
    /// The XSD schema file.  Mutually exclusive with `--schematron`.
    #[arg(long, value_name = "FILE", conflicts_with = "schematron")]
    schema: Option<PathBuf>,

    /// A Schematron rules file (ISO/IEC 19757-3).  Reported findings
    /// include both failed assertions (validation failures) and
    /// successful reports (informational, do not affect exit code).
    /// Mutually exclusive with `--schema`.
    #[arg(long, value_name = "FILE", conflicts_with = "schema")]
    schematron: Option<PathBuf>,

    /// XML files to validate; reads stdin when none given.
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,

    /// Print a `FILE: valid` line per successful file.  Default is
    /// silent-on-success — matching `make`, `ruff`, `rustfmt --check`,
    /// `tsc --noEmit`, where success is signaled by exit status alone.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Emit a JSON array of `{file, ok, issues: [...]}` records —
    /// one per input — instead of human-readable diagnostics.
    /// Each issue is `{message, line, column, path, kind}` for XSD
    /// or `{kind, message, path}` for Schematron.  Exit code is
    /// unchanged: 0 iff every file passed.
    #[arg(long, conflicts_with = "verbose")]
    json: bool,
}

#[derive(Args)]
struct RepairArgs {
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Write to FILE instead of stdout.
    #[arg(short = 'o', long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Overwrite the input file in place.
    #[arg(short = 'i', long, conflicts_with = "output", requires = "file")]
    in_place: bool,
}

#[derive(Args)]
struct StatsArgs {
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Emit a single JSON object with snake_case field names instead
    /// of the human-readable key/value table.  Field set is stable:
    /// `bytes`, `xml_version`, `encoding`, `elements`, `attributes`,
    /// `text_nodes`, `cdata`, `comments`, `pis`, `entity_refs`,
    /// `max_depth`, `text_bytes`.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct C14nArgs {
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Use Exclusive Canonical XML 1.0 (`xml-exc-c14n`) instead of
    /// the default Canonical XML 1.0 (`xml-c14n`).  Required by
    /// SAML, WS-Security, XAdES — anywhere a signed subtree must
    /// remain valid when extracted from its surrounding document.
    #[arg(long)]
    exclusive: bool,

    /// (Exclusive C14N only) Space-separated list of namespace
    /// prefixes to render even when not visibly used (the spec's
    /// `InclusiveNamespaces PrefixList`).  Use the empty string to
    /// force the default namespace.
    #[arg(long, value_name = "PREFIX-LIST", requires = "exclusive")]
    inclusive_prefixes: Option<String>,

    /// Include comment nodes in the output — the `#WithComments`
    /// variant of each algorithm.  Default: comments are stripped
    /// (the canonical-XML spec's behaviour, and what XML-DSig
    /// signatures over c14n output assume).  Set this when the
    /// comments themselves are part of what gets signed or compared.
    #[arg(long)]
    comments: bool,

    /// Write to FILE instead of stdout.
    #[arg(short = 'o', long, value_name = "FILE")]
    output: Option<PathBuf>,
}

#[derive(Args)]
struct DiffArgs {
    /// Reference document.
    #[arg(value_name = "LEFT")]
    left:  PathBuf,

    /// Document to compare against `LEFT`.
    #[arg(value_name = "RIGHT")]
    right: PathBuf,

    /// Emit a JSON array of `{op, path, ...}` records (one per
    /// diff line) instead of human-readable output.  Exit code
    /// stays 0 = identical / 1 = differ / 2 = error.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct LicenseArgs {
    /// Verify a specific certificate file (or a directory of them)
    /// instead of searching the default locations.
    #[arg(long, value_name = "FILE")]
    path: Option<PathBuf>,
}

// ── entry point ───────────────────────────────────────────────────────────────

fn main() -> ExitCode {
    let cli = Cli::parse();
    let start = Instant::now();

    let code = match dispatch(&cli) {
        Ok(()) => EXIT_OK,
        // ── negative-result outcomes (the run worked, the answer is "no") ──
        Err(CliError::Parse(e)) => {
            eprintln!("{e}");
            EXIT_NEGATIVE
        }
        Err(CliError::CheckFailed) => EXIT_NEGATIVE,
        Err(CliError::ExistsMiss)  => EXIT_NEGATIVE,
        // ── run-itself-failed outcomes (we couldn't get to an answer) ──
        Err(CliError::Validation(msg)) => {
            // Used only for "schema source itself unparseable" today;
            // distinct from "this instance failed validation" (that's
            // CheckFailed above).  An unparseable schema means the
            // invocation is broken, so it lands in the error bucket.
            eprintln!("{msg}");
            EXIT_ERROR
        }
        Err(CliError::Usage(msg)) => {
            eprintln!("error: {msg}");
            EXIT_ERROR
        }
        Err(CliError::Io(e)) => {
            eprintln!("io error: {e}");
            EXIT_ERROR
        }
        Err(CliError::Unlicensed(reason)) => {
            eprintln!("error: a valid license is required to run this command.");
            eprintln!("  {reason}");
            eprintln!(
                "  add a license certificate under .supso/license_certificates/ \
                 (or set SUPSO_LICENSE_PATH), then run `sup-xml license` to check it."
            );
            EXIT_ERROR
        }
    };

    if cli.global.timing {
        eprintln!("elapsed: {:?}", start.elapsed());
    }
    ExitCode::from(code)
}

fn dispatch(cli: &Cli) -> Result<(), CliError> {
    // Every command except `license` (and the clap-handled `help` /
    // `--version`, which never reach here) requires a valid,
    // non-expired license.
    if !matches!(cli.cmd, Cmd::License(_)) {
        require_license()?;
    }
    match &cli.cmd {
        Cmd::Lint(a) => run_lint(a, &cli.global),
        Cmd::Format(a) => run_format(a, &cli.global),
        Cmd::Xpath(a) => run_xpath(a, &cli.global),
        Cmd::Xslt(a) => run_xslt(a, &cli.global),
        Cmd::Validate(a) => run_validate(a, &cli.global),
        Cmd::Repair(a) => run_repair(a, &cli.global),
        Cmd::Stats(a) => run_stats(a, &cli.global),
        Cmd::C14n(a) => run_c14n(a, &cli.global),
        Cmd::Diff(a) => run_diff(a, &cli.global),
        Cmd::License(a) => run_license(a, &cli.global),
    }
}

/// Gate for all non-`license` commands: a valid (or grace-period)
/// license certificate must be present in one of the default locations
/// (or via `SUPSO_LICENSE_PATH`).  See [`sup_xml_license::License::validate_certificate`].
fn require_license() -> Result<(), CliError> {
    let cert = sup_xml_license::License::validate_certificate()
        .map_err(|e| CliError::Unlicensed(e.to_string()))?;
    // A grace-period certificate still licenses the command, but the
    // lapse is surfaced on stderr so it isn't missed (a CLI rarely has a
    // `log` subscriber installed to catch the gate's logged notice).
    if let Some(notice) = cert.grace_notice() {
        eprintln!("warning: {notice}");
    }
    Ok(())
}

// ── error type ────────────────────────────────────────────────────────────────

enum CliError {
    Parse(XmlError),
    Validation(String),
    Usage(String),
    Io(io::Error),
    CheckFailed,
    ExistsMiss,
    /// No valid license certificate was found for a command that
    /// requires one.  Carries the underlying reason.
    Unlicensed(String),
}

// ── JSON output (hand-rolled, no serde_json dep) ─────────────────────────────
//
// Both --json paths emit fixed, flat shapes (stats: one object; lint:
// array of `{file, ok, error?}`).  Hand-rolling keeps the CLI's
// runtime-dep list to just `clap` and saves ~50 transitive crates.

/// Escape `s` per RFC 8259 §7 so it can be embedded between JSON
/// `"..."` quotes.  Handles the four-byte string escapes and the
/// `\uXXXX` form for control characters.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '"'  => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `"escaped string"` for use as a JSON string literal.
fn json_str(s: &str) -> String {
    format!("\"{}\"", json_escape(s))
}

/// `1234` / `null` for JSON integer-or-null fields.
fn json_uint_opt(n: Option<u32>) -> String {
    n.map(|v| v.to_string()).unwrap_or_else(|| "null".into())
}

impl From<XmlError> for CliError {
    fn from(e: XmlError) -> Self { CliError::Parse(e) }
}
impl From<io::Error> for CliError {
    fn from(e: io::Error) -> Self { CliError::Io(e) }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Read input as raw bytes — the library's encoding detection (BOM
/// + XML declaration sniffing) runs downstream, so we don't force
/// UTF-8 here.  Lets UTF-16, ISO-8859-1, etc. pass through to the
/// parser, which can decode them properly when the document
/// declares its encoding.
fn read_input(file: Option<&Path>, max_size: u64) -> Result<Vec<u8>, CliError> {
    let bytes = match file {
        None => {
            let mut buf = Vec::new();
            io::stdin().read_to_end(&mut buf)?;
            buf
        }
        Some(p) if p.as_os_str() == "-" => {
            let mut buf = Vec::new();
            io::stdin().read_to_end(&mut buf)?;
            buf
        }
        Some(p) => fs::read(p)?,
    };
    if max_size > 0 && bytes.len() as u64 > max_size {
        return Err(CliError::Usage(format!(
            "input is {} bytes; --max-size is {}",
            bytes.len(), max_size,
        )));
    }
    Ok(bytes)
}

fn write_output(target: Option<&Path>, content: &str) -> Result<(), CliError> {
    match target {
        None => {
            let stdout = io::stdout();
            let mut h = stdout.lock();
            h.write_all(content.as_bytes())?;
        }
        Some(p) => fs::write(p, content)?,
    }
    Ok(())
}

/// Atomically replace `path` with `new_contents`, preserving the
/// original file's mode bits on Unix.  Writes to a sibling tempfile,
/// matches its permissions to the original, then renames over the
/// target — so a crash or failed write never leaves the user's file
/// truncated or with wrong perms.
///
/// On Windows, atomicity comes from `rename` alone (no POSIX-mode
/// concept to preserve).  On Unix, `rename(2)` is atomic for same-FS
/// targets, which is guaranteed here because the tempfile is created
/// in the same parent directory as the target.
fn write_in_place(path: &Path, new_contents: &str) -> Result<(), CliError> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("out");
    // Per-invocation tempfile name — pid + nanosecond timestamp keeps
    // parallel `sup-xml` calls on the same file from colliding.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{stem}.sup-xml.tmp.{pid}.{nonce}",
        pid = std::process::id(),
    ));

    // Body that does the write+chmod+rename.  Any error here triggers
    // the cleanup arm below; the tempfile is left only on a hard
    // failure (process killed between write and rename), where the
    // hidden-prefix + nonce makes it easy to spot and remove.
    let body = || -> Result<(), io::Error> {
        fs::write(&tmp, new_contents)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = fs::metadata(path) {
                let mode = meta.permissions().mode();
                fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
            }
        }
        fs::rename(&tmp, path)?;
        Ok(())
    };
    match body() {
        Ok(())  => Ok(()),
        Err(e)  => { let _ = fs::remove_file(&tmp); Err(CliError::Io(e)) }
    }
}

fn parse_opts(g: &GlobalOpts) -> ParseOptions {
    ParseOptions {
        recovery_mode: g.recover,
        external_resolver: build_resolver(g),
        ..ParseOptions::default()
    }
}

/// Parse `input` as XML, then optionally resolve `<xi:include>`
/// elements when `--xinclude` was passed.  Centralises the
/// parse + post-process step used by every doc-tree subcommand so
/// the XInclude pass is wired in exactly once.
///
/// `source_path` is the on-disk location of the input (used to
/// resolve relative `href` attributes); `None` for stdin.
fn parse_xml_at(
    input: &[u8],
    source_path: Option<&Path>,
    g: &GlobalOpts,
) -> Result<Document, CliError> {
    let doc = parse_bytes(input, &parse_opts(g))?;
    if !g.xinclude {
        return Ok(doc);
    }
    let xi_opts = XIncludeOptions {
        resolver: xinclude_resolver(g, source_path),
        ..XIncludeOptions::default()
    };
    Ok(process_xincludes(&doc, &xi_opts)?)
}

/// Build the resolver passed to [`process_xincludes`].  When the
/// input came from a file, wrap the user-configured resolver so
/// relative `href` attributes (e.g. `href="frag.xml"`) resolve
/// against the input's directory before the inner resolver's
/// allowlist check runs.  Without this, relative hrefs would be
/// joined against the process's cwd — which is rarely what the
/// user means and which then trips the `--allow-fs` allowlist.
fn xinclude_resolver(
    g: &GlobalOpts, source_path: Option<&Path>,
) -> Option<Arc<dyn EntityResolver>> {
    let inner = build_resolver(g)?;
    let base_dir = source_path
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    match base_dir {
        Some(dir) => Some(Arc::new(BaseDirResolver { inner, base_dir: dir })),
        None      => Some(inner),
    }
}

/// Resolver wrapper that joins relative `system_id` paths against a
/// fixed base directory before delegating.  Used by the CLI's
/// XInclude flow so a `<xi:include href="frag.xml"/>` next to the
/// input file just works.  Absolute paths and `file://` /
/// `http(s)://` URIs pass through unchanged.
#[derive(Debug)]
struct BaseDirResolver {
    inner: Arc<dyn EntityResolver>,
    base_dir: PathBuf,
}

impl EntityResolver for BaseDirResolver {
    fn resolve(
        &self, public_id: Option<&str>, system_id: &str,
        base_uri: Option<&str>,
    ) -> Result<Vec<u8>, ResolveError> {
        let needs_base = !system_id.contains("://")
            && !Path::new(system_id).is_absolute();
        let joined: String;
        let effective = if needs_base {
            joined = self.base_dir.join(system_id).to_string_lossy().into_owned();
            joined.as_str()
        } else {
            system_id
        };
        self.inner.resolve(public_id, effective, base_uri)
    }
}

/// Construct an [`EntityResolver`] from `--allow-host` / `--allow-fs`
/// (and friends).  Returns `None` when neither allowlist was given —
/// the XXE-safe default.  Single-resolver cases skip the
/// `ChainedResolver` wrapper.
fn build_resolver(g: &GlobalOpts) -> Option<Arc<dyn EntityResolver>> {
    let want_net = !g.allow_host.is_empty();
    let want_fs  = !g.allow_fs.is_empty();
    let net: Option<Arc<dyn EntityResolver>> = want_net.then(|| {
        let mut r = NetworkResolver::new(g.allow_host.iter().cloned());
        if g.allow_http        { r = r.with_plaintext_http(); }
        if g.allow_private_ips { r = r.with_private_ips_allowed(); }
        Arc::new(r) as Arc<dyn EntityResolver>
    });
    let fs: Option<Arc<dyn EntityResolver>> = want_fs.then(|| {
        Arc::new(FilesystemResolver::new(g.allow_fs.clone())) as Arc<dyn EntityResolver>
    });
    match (net, fs) {
        (None,    None)    => None,
        (Some(n), None)    => Some(n),
        (None,    Some(f)) => Some(f),
        (Some(n), Some(f)) => Some(Arc::new(ChainedResolver::new(vec![n, f]))),
    }
}

fn html_opts(g: &GlobalOpts) -> HtmlParseOptions {
    // HTML defaults to browser-style recovery: a tokeniser error
    // doesn't abort the parse.  `--strict-html` flips that off so
    // `lint` can answer "is this actually valid HTML5?" instead of
    // "could a browser make sense of it?"
    HtmlParseOptions {
        recovery_mode: !g.strict_html,
        ..HtmlParseOptions::default()
    }
}

fn indent_string(spaces: usize, tabs: bool) -> String {
    if tabs { "\t".to_string() } else { " ".repeat(spaces) }
}

// ── subcommands ───────────────────────────────────────────────────────────────

fn run_lint(args: &LintArgs, g: &GlobalOpts) -> Result<(), CliError> {
    let html_opts = if g.html { Some(html_opts(g)) } else { None };
    let opts = parse_opts(g);
    let inputs: Vec<Option<&Path>> =
        if args.files.is_empty() { vec![None] } else { args.files.iter().map(|p| Some(p.as_path())).collect() };

    // `--json` collects per-file results into a structured array, so
    // the report has to cover every input — implies `--keep-going`.
    let keep_going = args.keep_going || args.json;
    let mut had_failure = false;
    let mut json_records: Vec<String> = if args.json { Vec::with_capacity(inputs.len()) } else { Vec::new() };
    for input in inputs {
        let label = input.map(|p| p.display().to_string()).unwrap_or_else(|| "<stdin>".into());
        let result = match &html_opts {
            Some(h) => lint_one_html(input, g.max_size, h),
            None    => lint_one(input, g, &opts),
        };
        match result {
            Ok(()) => {
                if args.json {
                    json_records.push(format!(
                        r#"{{"file":{},"ok":true,"error":null}}"#,
                        json_str(&label),
                    ));
                } else if args.verbose && !g.quiet {
                    // Silent by default — exit code carries the signal.
                    // `-v` / `--verbose` restores the per-file OK line.
                    // `--quiet` is independent and keeps suppressing output
                    // regardless of `--verbose`, so scripts that pass both
                    // (e.g. inherited shell defaults) stay quiet.
                    println!("{label}: OK");
                }
            }
            Err(CliError::Parse(e)) => {
                had_failure = true;
                if args.json {
                    json_records.push(format!(
                        r#"{{"file":{},"ok":false,"error":{{"message":{},"line":{},"column":{}}}}}"#,
                        json_str(&label),
                        json_str(&e.message),
                        json_uint_opt(e.line),
                        json_uint_opt(e.column),
                    ));
                } else {
                    eprintln!("{label}: {}", display_xml_err(&e));
                }
                if !keep_going { return Err(CliError::CheckFailed); }
            }
            // I/O, usage, and other CLI-level errors propagate with
            // their original exit code so the caller can distinguish
            // "well-formedness violation" (1) from "file missing" /
            // "input rejected by --max-size" (2).
            Err(other) => return Err(other),
        }
    }
    if args.json {
        // One big array on stdout.  `--quiet` is honored: an empty
        // result is more useful than a partial one in that case.
        if !g.quiet {
            println!("[{}]", json_records.join(","));
        }
    }
    if had_failure { Err(CliError::CheckFailed) } else { Ok(()) }
}

fn lint_one(file: Option<&Path>, g: &GlobalOpts, opts: &ParseOptions) -> Result<(), CliError> {
    // File path: stat first so we can reject oversize files with a
    // clear --max-size diagnostic instead of letting the streaming
    // reader silently truncate.  Stdin: no metadata, the Take
    // wrapper enforces the cap but oversized stdin produces a
    // parse error instead of a usage error (best we can do without
    // buffering it all first).
    let (reader, size_hint): (Box<dyn io::Read>, Option<usize>) = match file {
        Some(p) if p.as_os_str() != "-" => {
            let f = fs::File::open(p)?;
            let size = f.metadata().ok().map(|m| m.len() as usize);
            if let (Some(s), cap) = (size, g.max_size) {
                if cap > 0 && s as u64 > cap {
                    return Err(CliError::Usage(format!(
                        "input is {s} bytes; --max-size is {cap}",
                    )));
                }
            }
            (Box::new(io::BufReader::new(f)), size)
        }
        _ => (Box::new(io::stdin().lock()), None),
    };

    // For non-UTF-8 inputs the streaming reader bails out
    // immediately (v1 limitation).  Detect that case via a BOM
    // peek and fall back to the slurped path, which goes through
    // `transcode_to_utf8` and supports the full encoding set.
    // We need a BufRead view so we can peek without consuming.
    let mut buffered = io::BufReader::new(reader);
    let peek = buffered.fill_buf().map_err(CliError::Io)?;
    let needs_transcode = peek.starts_with(&[0xFF, 0xFE])
        || peek.starts_with(&[0xFE, 0xFF])
        || peek.starts_with(&[0x00, 0x00, 0xFE, 0xFF])
        || peek.starts_with(&[0xFF, 0xFE, 0x00, 0x00]);

    if needs_transcode {
        // Non-UTF-8 input: buffer it all, transcode, validate via
        // the slurped reader (which has its own UTF-16 / ISO-8859-1
        // pipeline).  The --max-size cap was already enforced
        // above for files; for stdin we honour it here too.
        return lint_one_slurped(buffered, g, opts);
    }

    // UTF-8 path: stream.  Wrap with `CapReader` so a cap hit on
    // stdin produces a clear --max-size diagnostic.  `0` disables
    // the cap (use u64::MAX as the underlying limit).
    let buffer_size = effective_buffer_size(g);
    let capped_limit = if g.max_size == 0 { u64::MAX } else { g.max_size };
    let capped: Box<dyn io::Read> = Box::new(CapReader {
        inner: buffered, remaining: capped_limit, hit_cap: false,
    });
    // The streaming reader may surface a CapReader io::Error as
    // either a construction error (initial fill exceeded the cap)
    // or a validate-time parse error (cap reached mid-parse).
    // Both cases translate to CliError::Usage so the user sees
    // the --max-size diagnostic rather than a confusing parse
    // error on truncated XML.
    let stream = match XmlByteStreamReader::with_size_hint(capped, size_hint, buffer_size) {
        Ok(s)  => s.with_options(opts.clone()),
        Err(e) if e.message.contains("--max-size cap reached") =>
            return Err(CliError::Usage(format!(
                "input exceeded --max-size {}", g.max_size,
            ))),
        Err(e) => return Err(CliError::Parse(e)),
    };
    match stream.validate() {
        Ok(())  => Ok(()),
        Err(e) if e.message.contains("--max-size cap reached") =>
            Err(CliError::Usage(format!(
                "input exceeded --max-size {}", g.max_size,
            ))),
        Err(e)  => Err(CliError::Parse(e)),
    }
}

/// `io::Read` wrapper that enforces a byte cap.  Distinguishes
/// "the underlying source produced fewer bytes than the cap" (true
/// EOF — return 0 normally) from "the cap was hit but the source
/// had more bytes" (returns a typed `io::Error` so the CLI can
/// surface a clear `--max-size` diagnostic instead of a confusing
/// parse error on truncated XML).
///
/// For files we already enforce the cap up front via
/// `fs::metadata`; this wrapper is what makes stdin / pipe input
/// honour the cap with the same diagnostic.
struct CapReader<R: io::Read> {
    inner:     R,
    remaining: u64,
    hit_cap:   bool,
}

impl<R: io::Read> io::Read for CapReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            // Cap consumed.  Probe one byte to distinguish "source
            // ended" from "source still has data".  If probe finds a
            // byte, the cap was the limiting factor — surface that.
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => {
                    self.hit_cap = true;
                    Err(io::Error::new(
                        io::ErrorKind::Other,
                        "sup-xml-cli: --max-size cap reached",
                    ))
                }
            };
        }
        let take_n = (buf.len() as u64).min(self.remaining) as usize;
        let n = self.inner.read(&mut buf[..take_n])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Fallback to the slurped reader for non-UTF-8 inputs.  Buffers
/// the full input into memory, transcodes to UTF-8, then uses the
/// existing XmlBytesReader-based path.  Used by `lint_one` when a
/// non-UTF-8 BOM is detected on the input.
fn lint_one_slurped(
    mut reader: impl io::Read,
    g: &GlobalOpts,
    opts: &ParseOptions,
) -> Result<(), CliError> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    if g.max_size > 0 && bytes.len() as u64 > g.max_size {
        return Err(CliError::Usage(format!(
            "input is {} bytes; --max-size is {}",
            bytes.len(), g.max_size,
        )));
    }
    let utf8 = sup_xml::encoding::transcode_to_utf8(&bytes).map_err(CliError::Parse)?;
    let mut r = sup_xml::XmlBytesReader::from_bytes(&utf8)
        .map_err(CliError::Parse)?
        .with_options(opts.clone());
    loop {
        match r.next().map_err(CliError::Parse)? {
            sup_xml::BytesEvent::Eof => return Ok(()),
            _ => {}
        }
    }
}

/// Resolve the effective streaming-reader buffer size from the
/// global flags.  `--huge` wins if set (1 GiB); otherwise the
/// explicit `--buffer-size` value (or its default).  clap's
/// `conflicts_with` already rejects passing both at the same time.
fn effective_buffer_size(g: &GlobalOpts) -> usize {
    if g.huge { HUGE_BUFFER_SIZE } else { g.buffer_size as usize }
}

/// HTML variant of `lint_one`.  Parses with html5ever; in non-recovery
/// mode (the default), any parse error reported during tokenisation
/// is treated as a lint failure.  In `--recover` mode, errors are
/// quietly recovered and the lint passes — matching libxml2 `xmllint
/// --html --noerror` semantics.
fn lint_one_html(file: Option<&Path>, max_size: u64, opts: &HtmlParseOptions) -> Result<(), CliError> {
    let bytes = read_input(file, max_size)?;
    parse_html_bytes_opts(&bytes, opts).map_err(CliError::Parse)?;
    Ok(())
}

fn run_format(args: &FormatArgs, g: &GlobalOpts) -> Result<(), CliError> {
    let input = read_input(args.file.as_deref(), g.max_size)?;
    let serialized = if g.html {
        // HTML mode: html5ever parser + html-shape serializer.  Note
        // that --pretty for HTML is NOT supported in v1 — pretty
        // formatting for HTML needs block-vs-inline awareness which
        // isn't implemented yet, so we always emit compact HTML.
        if args.pretty && !g.quiet {
            eprintln!("warning: --pretty is not yet supported with --html; emitting compact output");
        }
        let doc = parse_html_bytes_opts(&input, &html_opts(g))?;
        serialize_html_to_string(&doc)
    } else {
        let doc = parse_xml_at(&input, args.file.as_deref(), g)?;
        let opts = SerializeOptions {
            write_xml_decl: !args.no_xml_decl,
            format: args.pretty,
            indent: indent_string(args.indent, args.indent_tabs),
            html_mode: false,
            xhtml: false,
            out_charset: sup_xml::OutputCharset::Utf8,
        };
        serialize_with(&doc, &opts)
    };

    if args.check {
        // Compare bytes-for-bytes: serialized output is always
        // canonical UTF-8, so a non-UTF-8 input naturally fails
        // --check (which is correct: re-emitting would re-encode).
        if serialized.as_bytes() != input.as_slice() {
            if !g.quiet {
                eprintln!(
                    "{} not in canonical form",
                    if args.pretty { "not pretty-printed" } else { "not normalized" }
                );
            }
            return Err(CliError::CheckFailed);
        }
        return Ok(());
    }
    if args.in_place {
        // `--in-place` is `requires = "file"` in clap, so the unwrap
        // is type-system-checked above the call site.
        write_in_place(args.file.as_deref().expect("--in-place requires a file arg"), &serialized)
    } else {
        write_output(args.output.as_deref(), &serialized)
    }
}

fn run_xpath(args: &XpathArgs, g: &GlobalOpts) -> Result<(), CliError> {
    let input = read_input(args.file.as_deref(), g.max_size)?;
    let doc = if g.html {
        // HTML names are lower-case post-tokenisation, so XPath
        // queries should also use lower-case (`//div`, not `//DIV`).
        // The XPath engine itself doesn't care — same tree types
        // either way.
        parse_html_bytes_opts(&input, &html_opts(g))?
    } else {
        parse_xml_at(&input, args.file.as_deref(), g)?
    };
    let ctx = XPathContext::new(&doc);

    if args.exists {
        let v = ctx.eval(&args.expr)?;
        return match v {
            XPathValue::NodeSet(ref ns) if !ns.is_empty() => Ok(()),
            XPathValue::Boolean(true) => Ok(()),
            _ => Err(CliError::ExistsMiss),
        };
    }
    if args.count {
        let n = ctx.eval_count(&args.expr)?;
        println!("{n}");
        return Ok(());
    }
    let pieces: Vec<String> = if args.nodes {
        // --nodes: each matched node serialized as XML (element subtree,
        // attr as name="value", namespace as xmlns:p="uri", document as
        // the whole serialized doc).  Non-NodeSet results collapse to
        // their XPath 1.0 string form.  See [`XPathContext::eval_node_xml`].
        ctx.eval_node_xml(&args.expr)?
    } else {
        let value = ctx.eval(&args.expr)?;
        match value {
            // `ForeignNodeSet` results from `document(URI)` calls.  The
            // CLI uses the default `XPathBindings` impl, which doesn't
            // implement `load_document`, so `document()` errors out
            // earlier as "unknown function" and this arm is unreachable
            // in practice — handled here only to keep the match
            // exhaustive.  `eval_strings` falls through to the generic
            // value-to-string conversion for this variant.
            XPathValue::NodeSet(_) | XPathValue::ForeignNodeSet(_) => ctx.eval_strings(&args.expr)?,
            XPathValue::Boolean(b) => vec![b.to_string()],
            XPathValue::Number(n)  => vec![format_xpath_number(n.as_f64())],
            XPathValue::String(s)  => vec![s],
            XPathValue::Typed(t)   => vec![t.lexical],
            // Atomic / mixed sequence: fall through to the generic
            // string projection used for node-sets so each item
            // surfaces on its own line.
            XPathValue::Sequence(_) | XPathValue::IntRange { .. } =>
                ctx.eval_strings(&args.expr)?,
            XPathValue::Map(_) | XPathValue::Array(_) | XPathValue::Function(_) => Vec::new(),
        }
    };
    let out = pieces.join(&args.separator);
    println!("{out}");
    Ok(())
}

fn format_xpath_number(n: f64) -> String {
    if n.is_nan() { return "NaN".into(); }
    if n.is_infinite() { return if n > 0.0 { "Infinity".into() } else { "-Infinity".into() }; }
    if n == n.trunc() && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn run_xslt(args: &XsltArgs, g: &GlobalOpts) -> Result<(), CliError> {
    let xslt_err = |stage: &str, e: String| -> CliError {
        CliError::Parse(XmlError::new(
            sup_xml::ErrorDomain::Xslt,
            sup_xml::ErrorLevel::Error,
            format!("xslt {stage}: {e}"),
        ))
    };

    // Stylesheet source + base URI (drives xsl:include / document() URI
    // resolution).
    let xsl_text = fs::read_to_string(&args.stylesheet).map_err(|e| {
        CliError::Usage(format!(
            "can't read stylesheet {}: {e}",
            args.stylesheet.display(),
        ))
    })?;
    let base = args.stylesheet.to_string_lossy().into_owned();
    let dir = args
        .stylesheet
        .parent()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let loader = sup_xml::xslt::FilesystemLoader::new(vec![dir]);

    let style = sup_xml::xslt::Stylesheet::compile_str_with_loader(
        &xsl_text, &loader, Some(&base),
    ).map_err(|e| xslt_err("compile", format!("{e:?}")))?;

    // Input XML.
    let src_bytes = read_input(args.file.as_deref(), g.max_size)?;
    let mut opts = parse_opts(g);
    opts.namespace_aware = true;
    let doc = parse_bytes(&src_bytes, &opts)?;
    let doc = if g.xinclude {
        let xi_opts = XIncludeOptions {
            resolver: xinclude_resolver(g, args.file.as_deref()),
            ..XIncludeOptions::default()
        };
        process_xincludes(&doc, &xi_opts)?
    } else {
        doc
    };

    // Parse `--param name=value` entries; reject malformed ones
    // up front so the engine never sees them.  Param values are
    // taken as strings (XSLT 1.0's natural type for top-level
    // params); the stylesheet can coerce inside the template.
    let mut params: Vec<(String, String)> = Vec::with_capacity(args.param.len());
    for entry in &args.param {
        match entry.split_once('=') {
            Some((k, v)) if !k.is_empty() =>
                params.push((k.to_string(), v.to_string())),
            _ => return Err(CliError::Usage(format!(
                "--param expects NAME=VALUE (got {entry:?})"
            ))),
        }
    }
    // Apply with the same loader so document(...) calls reach
    // adjacent files.
    let result = if params.is_empty() {
        style.apply_with_loader(&doc, &loader, Some(&base))
    } else {
        style.apply_with_params(&doc, &loader, Some(&base), &params)
    }.map_err(|e| xslt_err("apply", format!("{e:?}")))?;

    let out = result
        .to_string()
        .map_err(|e| xslt_err("serialize", format!("{e:?}")))?;
    write_output(args.output.as_deref(), &out)?;
    Ok(())
}

fn run_validate(args: &ValidateArgs, g: &GlobalOpts) -> Result<(), CliError> {
    // Branch on which rule language the caller asked for.  Clap's
    // `conflicts_with` already rejects supplying both; one MUST be
    // supplied — the second arm catches the all-defaults case.
    if let Some(path) = args.schematron.as_ref() {
        return run_validate_schematron(args, g, path);
    }
    let schema_path = args.schema.as_ref().ok_or_else(|| CliError::Usage(
        "either --schema (XSD) or --schematron is required".into(),
    ))?;
    let schema_src = fs::read_to_string(schema_path)
        .map_err(|e| CliError::Usage(format!("can't read schema {}: {e}", schema_path.display())))?;
    let compile_result = match build_schema_resolver(g) {
        Some(r) => Schema::compile_with(&schema_src, r),
        None    => Schema::compile_str(&schema_src),
    };
    let schema = compile_result.map_err(|e| {
        let loc = match (e.line, e.column) {
            (Some(l), Some(c)) => format!(":{l}:{c}"),
            (Some(l), None) => format!(":{l}"),
            _ => String::new(),
        };
        CliError::Validation(format!("schema {}{}: {}", schema_path.display(), loc, e.message))
    })?;

    let inputs: Vec<Option<&Path>> = if args.files.is_empty() {
        vec![None]
    } else {
        args.files.iter().map(|p| Some(p.as_path())).collect()
    };

    let mut had_failure = false;
    let mut json_records: Vec<String> = Vec::new();
    for input in inputs {
        let label = input.map(|p| p.display().to_string()).unwrap_or_else(|| "<stdin>".into());
        let xml = read_input(input, g.max_size)?;
        // `validate_bytes` only checks the bytes are already UTF-8;
        // pre-decode so UTF-16 etc. (detected from BOM + XML decl)
        // reach the validator as UTF-8.  Borrowed Cow for UTF-8
        // input — no allocation.
        let utf8 = sup_xml::encoding::transcode_to_utf8(&xml).map_err(CliError::Parse)?;
        // With `--xinclude`, expand `<xi:include>` elements before
        // running the validator.  The schema sees the post-XInclude
        // shape, which is the W3C-defined order (XInclude precedes
        // schema validation).  Validating the already-parsed
        // Document directly via `validate_doc` skips a parse-then-
        // serialise-then-parse round-trip.
        let xi_result = if g.xinclude {
            Some(parse_xml_at(&utf8, input, g)?)
        } else {
            None
        };
        let result = match xi_result.as_ref() {
            Some(doc) => schema.validate_doc(doc),
            None      => schema.validate_bytes(&utf8),
        };
        match result {
            Ok(()) => {
                if args.json {
                    json_records.push(format!(
                        r#"{{"file":{},"ok":true,"issues":[]}}"#, json_str(&label),
                    ));
                } else if args.verbose && !g.quiet {
                    println!("{label}: valid");
                }
            }
            Err(err) => {
                had_failure = true;
                if args.json {
                    let issues_json: Vec<String> = err.issues.iter().map(|issue| {
                        format!(
                            r#"{{"message":{},"line":{},"column":{},"path":{},"kind":{:?}}}"#,
                            json_str(&issue.message),
                            json_uint_opt(issue.line),
                            json_uint_opt(issue.column),
                            json_str(&issue.path),
                            format!("{:?}", issue.kind),
                        )
                    }).collect();
                    json_records.push(format!(
                        r#"{{"file":{},"ok":false,"issues":[{}]}}"#,
                        json_str(&label),
                        issues_json.join(","),
                    ));
                } else {
                    for issue in &err.issues {
                        let loc = match (issue.line, issue.column) {
                            (Some(l), Some(c)) => format!(":{l}:{c}"),
                            (Some(l), None) => format!(":{l}"),
                            _ => String::new(),
                        };
                        let path = if issue.path.is_empty() { String::new() } else { format!(" at {}", issue.path) };
                        eprintln!("{label}{loc}: {}{path}", issue.message);
                    }
                }
            }
        }
    }
    if args.json {
        println!("[{}]", json_records.join(","));
    }
    if had_failure { Err(CliError::CheckFailed) } else { Ok(()) }
}

/// Schematron path of `validate`.  Compiles the rules file via the
/// XSLT-backed engine, validates each input, and prints findings.
/// Failed assertions are validation failures (exit 1); successful
/// reports are informational and don't affect exit code.
fn run_validate_schematron(
    args: &ValidateArgs, g: &GlobalOpts, sch_path: &Path,
) -> Result<(), CliError> {
    use sup_xml::xslt::schematron::{Schematron, FindingKind};
    use sup_xml::xslt::FilesystemLoader;

    let sch_src = fs::read_to_string(sch_path).map_err(|e| {
        CliError::Usage(format!("can't read schematron {}: {e}", sch_path.display()))
    })?;
    let dir = sch_path.parent().map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let loader = FilesystemLoader::new(vec![dir]);
    let sch = Schematron::compile_str_with_loader(
        &sch_src, &loader, Some(&sch_path.to_string_lossy()),
    ).map_err(|e| {
        CliError::Validation(format!("schematron {}: {e:?}", sch_path.display()))
    })?;

    let inputs: Vec<Option<&Path>> = if args.files.is_empty() {
        vec![None]
    } else {
        args.files.iter().map(|p| Some(p.as_path())).collect()
    };

    let mut had_failure = false;
    for input in inputs {
        let label = input.map(|p| p.display().to_string())
            .unwrap_or_else(|| "<stdin>".into());
        let xml = read_input(input, g.max_size)?;
        let utf8 = sup_xml::encoding::transcode_to_utf8(&xml).map_err(CliError::Parse)?;
        let xml_str = std::str::from_utf8(&utf8).map_err(|e| {
            CliError::Validation(format!("{label}: input is not valid UTF-8: {e}"))
        })?;
        match sch.validate_str(xml_str) {
            Ok(report) => {
                for f in &report.findings {
                    let tag = match f.kind {
                        FindingKind::FailedAssert => "FAIL",
                        FindingKind::SuccessfulReport => "INFO",
                    };
                    let path = if f.location_id.is_empty() {
                        if f.context_name.is_empty() { String::new() }
                        else { format!(" at <{}>", f.context_name) }
                    } else {
                        format!(" at #{}", f.location_id)
                    };
                    eprintln!("{label}: {tag}{path}: {}", f.message);
                    if matches!(f.kind, FindingKind::FailedAssert) {
                        had_failure = true;
                    }
                }
                if report.findings.is_empty() && args.verbose && !g.quiet {
                    println!("{label}: valid");
                }
            }
            Err(e) => {
                had_failure = true;
                eprintln!("{label}: schematron validation failed: {e:?}");
            }
        }
    }
    if had_failure { Err(CliError::CheckFailed) } else { Ok(()) }
}

fn run_repair(args: &RepairArgs, g: &GlobalOpts) -> Result<(), CliError> {
    let input = read_input(args.file.as_deref(), g.max_size)?;

    let out = if g.html {
        // HTML is already lenient by default; --recover doesn't add
        // anything.  Surface recovered errors on stderr the same
        // way as the XML path.
        let h_opts = html_opts(g);
        let (parse_result, recovered) = parse_html_bytes_with_recovered(&input, &h_opts);
        for err in &recovered {
            eprintln!("recovered: {}", display_xml_err(err));
        }
        let doc = parse_result?;
        serialize_html_to_string(&doc)
    } else {
        let recover_opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
        let (parse_result, recovered) = parse_bytes_with_recovered(&input, &recover_opts);
        for err in &recovered {
            eprintln!("recovered: {}", display_xml_err(err));
        }
        let doc = parse_result?;
        serialize_to_string(&doc)
    };
    if args.in_place {
        write_in_place(args.file.as_deref().expect("--in-place requires a file arg"), &out)
    } else {
        write_output(args.output.as_deref(), &out)
    }
}

fn run_license(args: &LicenseArgs, _g: &GlobalOpts) -> Result<(), CliError> {
    use sup_xml_license::License;

    let result = match &args.path {
        Some(p) => sup_xml_license::validate_certificate_path(p),
        None => License::validate_certificate(),
    };

    match result {
        Ok(cert) => {
            let lic = &cert.license;
            match cert.grace_notice() {
                Some(notice) => {
                    println!("License: grace period");
                    println!("  {notice}");
                }
                None => println!("License: valid"),
            }
            println!("  Organization: {} ({})", lic.organization.name, lic.organization.id);
            println!("  Project:      {}", lic.project.name);
            println!("  Order:        {}", lic.order.id);
            println!("  Expires:      {}", lic.order.expires_at);
            println!("  Source:       {}", cert.path.display());
            // Surface any extra metadata (everything except the project
            // binding, which is shown above), sorted by key.
            for (k, v) in &lic.metadata {
                if k == "project" {
                    continue;
                }
                println!("  {k}: {v}");
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("License: not valid");
            eprintln!("  {e}");
            Err(CliError::CheckFailed)
        }
    }
}

fn run_c14n(args: &C14nArgs, g: &GlobalOpts) -> Result<(), CliError> {
    let input = read_input(args.file.as_deref(), g.max_size)?;
    let doc = parse_xml_at(&input, args.file.as_deref(), g)?;

    let mode = if args.exclusive {
        let inclusive_prefixes = args
            .inclusive_prefixes
            .as_deref()
            .map(|s| s.split_whitespace().map(|p| p.to_string()).collect())
            .unwrap_or_default();
        C14nMode::ExcC14n10 { inclusive_prefixes }
    } else {
        C14nMode::C14n10
    };
    let opts = CanonicalizeOptions { mode, with_comments: args.comments };

    let bytes = canonicalize_to_bytes(&doc, &opts);

    // Canonical XML is bytes (UTF-8 by definition).  Write raw to
    // stdout (or --output) without going through write_output, which
    // is String-typed.
    match args.output.as_deref() {
        Some(path) => fs::write(path, &bytes).map_err(CliError::Io)?,
        None => {
            use std::io::Write;
            std::io::stdout()
                .write_all(&bytes)
                .map_err(CliError::Io)?;
        }
    }
    Ok(())
}

/// Structural diff between two parsed XML documents.  Walks both
/// trees in parallel reporting the first mismatch at each level
/// (so a tree with N differences emits N lines, but doesn't pile
/// secondary-order errors on top of a structural one).  Exit code:
/// 0 = identical, 1 = differ, 2 = couldn't read or parse.
fn run_diff(args: &DiffArgs, g: &GlobalOpts) -> Result<(), CliError> {
    let left_bytes  = fs::read(&args.left).map_err(|e| {
        CliError::Usage(format!("can't read {}: {e}", args.left.display()))
    })?;
    let right_bytes = fs::read(&args.right).map_err(|e| {
        CliError::Usage(format!("can't read {}: {e}", args.right.display()))
    })?;
    let left_doc  = parse_xml_at(&left_bytes,  Some(&args.left),  g)?;
    let right_doc = parse_xml_at(&right_bytes, Some(&args.right), g)?;

    let mut diffs: Vec<String> = Vec::new();
    diff_node(left_doc.root(), right_doc.root(), &mut String::new(), &mut diffs);

    let stdout = io::stdout();
    let mut h = stdout.lock();
    if args.json {
        // Each diff line is one of:
        //   "- {path}: removed ..."        op="del"
        //   "+ {path}: added ..."          op="add"
        //   "- {path}: text/attr changed"  op="change"
        // The first character disambiguates; we keep the human
        // message intact in `text` so callers don't need to parse
        // the variant detail.
        h.write_all(b"[").map_err(CliError::Io)?;
        for (i, line) in diffs.iter().enumerate() {
            if i > 0 { h.write_all(b",").map_err(CliError::Io)?; }
            let op = match line.chars().next() {
                Some('+') => "add",
                Some('-') if line.contains(": added")    => "add",
                Some('-') if line.contains(": removed")  => "del",
                Some('-') => "change",
                _         => "change",
            };
            write!(h, r#"{{"op":"{op}","text":{}}}"#, json_str(line.as_str()))
                .map_err(CliError::Io)?;
        }
        h.write_all(b"]\n").map_err(CliError::Io)?;
        if diffs.is_empty() { return Ok(()); }
        return Err(CliError::CheckFailed);
    }
    if diffs.is_empty() { return Ok(()); }
    for line in &diffs {
        writeln!(h, "{line}").map_err(CliError::Io)?;
    }
    Err(CliError::CheckFailed)
}

/// Compare two nodes and any subtree below them, appending one
/// human-readable line per difference to `out`.  `path` is built
/// up incrementally — `/root/section[2]/p[1]/...`.  Recurses
/// element-by-element; mismatched node kinds report once and stop.
fn diff_node(
    l: &sup_xml::Node,
    r: &sup_xml::Node,
    path: &mut String,
    out: &mut Vec<String>,
) {
    use sup_xml::NodeKind;
    let lk = l.kind;
    let rk = r.kind;
    if lk != rk {
        out.push(format!("- {path}: type {lk:?} → {rk:?}"));
        return;
    }
    match lk {
        NodeKind::Element => {
            if l.name() != r.name() {
                out.push(format!(
                    "- {path}: element <{}> → <{}>",
                    l.name(), r.name(),
                ));
                return;
            }
            // Attributes: report missing / extra / value-changed.
            let mut l_attrs: Vec<(&str, &str)> = l.attributes().map(|a| (a.name(), a.value())).collect();
            let mut r_attrs: Vec<(&str, &str)> = r.attributes().map(|a| (a.name(), a.value())).collect();
            l_attrs.sort_by_key(|(n, _)| *n);
            r_attrs.sort_by_key(|(n, _)| *n);
            for (name, lv) in &l_attrs {
                match r_attrs.iter().find(|(rn, _)| rn == name) {
                    Some((_, rv)) if rv != lv =>
                        out.push(format!("- {path}/@{name}: {lv:?} → {rv:?}")),
                    Some(_) => {}
                    None    =>
                        out.push(format!("- {path}/@{name}: removed (was {lv:?})")),
                }
            }
            for (name, rv) in &r_attrs {
                if !l_attrs.iter().any(|(ln, _)| ln == name) {
                    out.push(format!("- {path}/@{name}: added ({rv:?})"));
                }
            }
            // Recurse content children in document order.  Track
            // per-name sibling indices for the path so consumers
            // can see *which* of N elements diverged.
            let l_children: Vec<&sup_xml::Node> = l.children().collect();
            let r_children: Vec<&sup_xml::Node> = r.children().collect();
            let max = l_children.len().max(r_children.len());
            let mut counters: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
            for i in 0..max {
                let lc = l_children.get(i).copied();
                let rc = r_children.get(i).copied();
                match (lc, rc) {
                    (Some(ln), Some(rn)) => {
                        let seg = step_label(ln, &mut counters);
                        let base = path.len();
                        path.push_str(&seg);
                        diff_node(ln, rn, path, out);
                        path.truncate(base);
                    }
                    (Some(ln), None) => {
                        let seg = step_label(ln, &mut counters);
                        out.push(format!("- {path}{seg}: removed"));
                    }
                    (None, Some(rn)) => {
                        let seg = step_label(rn, &mut counters);
                        out.push(format!("+ {path}{seg}: added"));
                    }
                    (None, None) => unreachable!(),
                }
            }
        }
        NodeKind::Text | NodeKind::CData => {
            let lc = l.content();
            let rc = r.content();
            if lc != rc {
                out.push(format!("- {path}: text {lc:?} → {rc:?}"));
            }
        }
        NodeKind::Comment => {
            let lc = l.content();
            let rc = r.content();
            if lc != rc {
                out.push(format!("- {path}: comment <!--{lc}--> → <!--{rc}-->"));
            }
        }
        NodeKind::Pi => {
            if l.name() != r.name() || l.content() != r.content() {
                out.push(format!(
                    "- {path}: PI <?{} {}?> → <?{} {}?>",
                    l.name(), l.content(), r.name(), r.content(),
                ));
            }
        }
        _ => {}
    }
}

fn step_label<'a>(
    node: &'a sup_xml::Node,
    counters: &mut std::collections::HashMap<&'a str, u32>,
) -> String {
    use sup_xml::NodeKind;
    match node.kind {
        NodeKind::Element => {
            let name = node.name();
            let c = counters.entry(name).or_insert(0);
            *c += 1;
            if *c == 1 {
                format!("/{name}")
            } else {
                format!("/{name}[{c}]")
            }
        }
        NodeKind::Text | NodeKind::CData => "/text()".into(),
        NodeKind::Comment => "/comment()".into(),
        NodeKind::Pi      => format!("/processing-instruction('{}')", node.name()),
        _ => "/?".into(),
    }
}

fn run_stats(args: &StatsArgs, g: &GlobalOpts) -> Result<(), CliError> {
    let input = read_input(args.file.as_deref(), g.max_size)?;
    let doc = if g.html {
        parse_html_bytes_opts(&input, &html_opts(g))?
    } else {
        parse_xml_at(&input, args.file.as_deref(), g)?
    };

    let mut s = Counts::default();
    s.bytes = input.len();
    s.version = doc.version.clone();
    s.encoding = doc.encoding.clone();
    walk(doc.root(), 1, &mut s);

    if args.json {
        // Order matches the human-readable table for diff-ability.
        println!(
            r#"{{"bytes":{},"xml_version":{},"encoding":{},"elements":{},"attributes":{},"text_nodes":{},"cdata":{},"comments":{},"pis":{},"entity_refs":{},"max_depth":{},"text_bytes":{}}}"#,
            s.bytes,
            json_str(&s.version),
            json_str(&s.encoding),
            s.elements,
            s.attributes,
            s.texts,
            s.cdata,
            s.comments,
            s.pis,
            s.entity_refs,
            s.max_depth,
            s.text_bytes,
        );
        return Ok(());
    }

    println!("bytes:        {}", s.bytes);
    println!("xml-version:  {}", s.version);
    println!("encoding:     {}", s.encoding);
    println!("elements:     {}", s.elements);
    println!("attributes:   {}", s.attributes);
    println!("text nodes:   {}", s.texts);
    println!("cdata:        {}", s.cdata);
    println!("comments:     {}", s.comments);
    println!("PIs:          {}", s.pis);
    println!("entity refs:  {}", s.entity_refs);
    println!("max depth:    {}", s.max_depth);
    println!("text bytes:   {}", s.text_bytes);
    Ok(())
}

#[derive(Default)]
struct Counts {
    bytes: usize,
    version: String,
    encoding: String,
    elements: usize,
    attributes: usize,
    texts: usize,
    cdata: usize,
    comments: usize,
    pis: usize,
    entity_refs: usize,
    max_depth: usize,
    text_bytes: usize,
}

fn walk(node: &sup_xml::Node<'_>, depth: usize, s: &mut Counts) {
    s.max_depth = s.max_depth.max(depth);
    match node.kind {
        NodeKind::Element => {
            s.elements   += 1;
            s.attributes += node.attributes().count();
            for child in node.children() {
                walk(child, depth + 1, s);
            }
        }
        NodeKind::Text => {
            s.texts      += 1;
            s.text_bytes += node.content().len();
        }
        NodeKind::CData => {
            s.cdata      += 1;
            s.text_bytes += node.content().len();
        }
        NodeKind::Comment   => { s.comments    += 1; }
        NodeKind::Pi        => { s.pis         += 1; }
        NodeKind::EntityRef => { s.entity_refs += 1; }
        // DTD internal subset — the node itself and its declaration
        // block are held under the DTD, not part of the element tree
        // these stats walk.
        NodeKind::DtdDecl => {}
        NodeKind::Dtd => {}
        // c-abi-only discriminants; never appear on a real Node.
        NodeKind::Attribute => unreachable!("Attribute kind never appears on a Node"),
        NodeKind::Document  => unreachable!("Document kind never appears on a Node"),
        NodeKind::DocumentFragment => unreachable!("DocumentFragment kind never appears on a Node"),
    }
}

// ── schema-resolution wiring ──────────────────────────────────────────────────
//
// XSD `<xs:import>` / `<xs:include>` / `<xs:redefine>` resolution goes
// through a separate trait ([`SchemaResolver`]) than the entity
// resolver used for DTD / general-entity loading.  The CLI wires the
// same `--allow-fs` / `--allow-host` allowlists into both — building
// one [`FsResolver`] per allowed directory for relative
// `schemaLocation` values, and adapting the existing
// [`NetworkResolver`] for absolute `http(s)://` ones.

/// Adapt an [`EntityResolver`] (which expects an already-absolute
/// SYSTEM URL) into a [`SchemaResolver`].  Only fires for
/// `http://` / `https://` locations; everything else returns
/// `Ok(None)` so the chain falls through to filesystem resolvers.
struct NetworkSchemaAdapter {
    inner: NetworkResolver,
}

impl SchemaResolver for NetworkSchemaAdapter {
    fn resolve(&self, location: &str, _target_ns: Option<&str>)
        -> Result<Option<Vec<u8>>, io::Error>
    {
        if !(location.starts_with("http://") || location.starts_with("https://")) {
            return Ok(None);
        }
        match self.inner.resolve(None, location, None) {
            Ok(bytes) => Ok(Some(bytes)),
            // "Refused" is the resolver's security policy denying the
            // request.  Surface it as a hard PermissionDenied so the
            // user sees "your --allow-host list rejected this" rather
            // than a silent "schema not found".
            Err(ResolveError::Refused(msg)) =>
                Err(io::Error::new(io::ErrorKind::PermissionDenied, msg)),
            Err(ResolveError::Io(msg)) | Err(ResolveError::Other(msg)) =>
                Err(io::Error::other(msg)),
        }
    }
}

/// Routes XSD schema-location requests across the configured
/// allowlists: URL-shaped locations go to the network adapter,
/// relative paths walk the [`FsResolver`] chain in order.
struct CliSchemaResolver {
    fs:  Vec<FsResolver>,
    net: Option<NetworkSchemaAdapter>,
}

impl SchemaResolver for CliSchemaResolver {
    fn resolve(&self, location: &str, target_ns: Option<&str>)
        -> Result<Option<Vec<u8>>, io::Error>
    {
        let is_url = location.starts_with("http://") || location.starts_with("https://");
        if is_url {
            return match &self.net {
                Some(n) => n.resolve(location, target_ns),
                // No `--allow-host` given: surface a clear permission
                // denial so the user knows which flag they're missing.
                None => Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("network schema fetch not enabled: {location:?} \
                             (pass --allow-host)"))),
            };
        }
        for r in &self.fs {
            match r.resolve(location, target_ns)? {
                Some(bytes) => return Ok(Some(bytes)),
                None        => continue,
            }
        }
        Ok(None)
    }
}

/// Build a schema resolver from the global allowlists, or return
/// `None` if neither was given (the XXE-safe default — every
/// `<xs:import>` / `<xs:include>` becomes a compile error).
fn build_schema_resolver(g: &GlobalOpts) -> Option<CliSchemaResolver> {
    if g.allow_fs.is_empty() && g.allow_host.is_empty() { return None; }
    let fs: Vec<FsResolver> = g.allow_fs.iter()
        .map(|p| FsResolver::new(p.clone()))
        .collect();
    let net = (!g.allow_host.is_empty()).then(|| {
        let mut nr = NetworkResolver::new(g.allow_host.iter().cloned());
        if g.allow_http        { nr = nr.with_plaintext_http(); }
        if g.allow_private_ips { nr = nr.with_private_ips_allowed(); }
        NetworkSchemaAdapter { inner: nr }
    });
    Some(CliSchemaResolver { fs, net })
}

// ── error rendering ───────────────────────────────────────────────────────────

fn display_xml_err(e: &XmlError) -> String {
    let loc = match (e.line, e.column) {
        (Some(l), Some(c)) => format!("{l}:{c}: "),
        (Some(l), None) => format!("{l}: "),
        _ => String::new(),
    };
    let file = e.file.as_deref().map(|f| format!("{f}:")).unwrap_or_default();
    format!("{file}{loc}{}", e.message)
}

