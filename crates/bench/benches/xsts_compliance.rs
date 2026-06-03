//! W3C XML Schema Test Suite (XSTS 2006-11-06) cross-implementation runner.
//!
//! Walks every `.testSet` manifest under
//! `tests/assets/xsts/xmlschema2006-11-06/{sun,boeing,nist,ms}Meta/`,
//! dispatches each `<schemaTest>` to every linked-in XSD implementation,
//! and dispatches each `<instanceTest>` to every implementation using
//! the most recent same-group schema.  Reports per-contributor agreement
//! tables: how often each implementation matches the suite's expected
//! validity, where they disagree with each other, and which tests
//! exceeded the per-test wall-clock budget for each backend.
//!
//! Today's backends:
//!   - **sup-xml** — `sup_xml::xsd::Schema::compile_with` / `validate_str`
//!   - **libxml2** — `xmlSchemaParse` / `xmlSchemaValidateDoc` via FFI
//!
//! Xerces-J and Saxon-EE are obvious future additions (both consume the
//! same `.testSet` format).  Saxon needs an EE evaluation license and
//! Xerces needs a JVM subprocess daemon to amortise startup across 25k
//! tests, so neither is wired in here.
//!
//! Run:
//!     cargo bench -p sup-xml-bench --bench xsts_compliance
//!
//! Env vars:
//!     XSTS_VERBOSE=1     print every per-test disagreement
//!     XSTS_FILTER=<sub>  only run manifests whose path contains <sub>
//!     XSTS_QUIET=1       suppress the default per-manifest progress line
//!     XSTS_TIMEOUT=<N>   per-test wall-clock budget in seconds (default: 30)
//!
//! Per-test timeouts are enforced by running each backend on its own
//! dedicated worker thread.  When a test exceeds the budget the
//! worker is abandoned (it keeps running until process exit; the OS
//! reclaims its memory) and a fresh worker is spawned for the next
//! test.  Subsequent instance tests against a schema that timed out
//! are counted as "no schema" for that backend, so the surrounding
//! schema's instance row is not double-penalised.
//!
//! Fetch the suite first via `tests/assets/xsts/fetch.sh`.  If the
//! suite isn't present, the bench prints a hint and exits.

#![allow(clippy::missing_safety_doc)]

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use sup_xml::xsd::{FsResolver, Schema};
use sup_xml::{Event, XmlReader};

const XSTS_ROOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/assets/xsts/xmlschema2006-11-06"
);

const DEFAULT_TIMEOUT_SECS: u64 = 30;

// ── libxml2 schema-validation FFI ────────────────────────────────────────────
//
// Path-based parsing (rather than the memory variant used by the perf bench in
// `xsd.rs`) so that XSTS schemas' `xs:import` / `xs:include` directives resolve
// against the schema's own directory automatically.

type XmlSchemaParserCtxtPtr = *mut c_void;
type XmlSchemaPtr           = *mut c_void;
type XmlSchemaValidCtxtPtr  = *mut c_void;
type XmlDocPtr              = *mut c_void;

unsafe extern "C" {
    fn xmlReadFile(url: *const c_char, encoding: *const c_char, options: c_int) -> XmlDocPtr;
    fn xmlFreeDoc(doc: XmlDocPtr);

    fn xmlSchemaNewParserCtxt(url: *const c_char) -> XmlSchemaParserCtxtPtr;
    fn xmlSchemaParse(ctxt: XmlSchemaParserCtxtPtr) -> XmlSchemaPtr;
    fn xmlSchemaFreeParserCtxt(ctxt: XmlSchemaParserCtxtPtr);
    fn xmlSchemaFree(schema: XmlSchemaPtr);
    fn xmlSchemaSetParserErrors(
        ctxt: XmlSchemaParserCtxtPtr,
        err: Option<unsafe extern "C" fn()>,
        warn: Option<unsafe extern "C" fn()>,
        ctx: *mut c_void,
    );

    fn xmlSchemaNewValidCtxt(schema: XmlSchemaPtr) -> XmlSchemaValidCtxtPtr;
    fn xmlSchemaFreeValidCtxt(ctxt: XmlSchemaValidCtxtPtr);
    fn xmlSchemaValidateDoc(ctxt: XmlSchemaValidCtxtPtr, doc: XmlDocPtr) -> c_int;
    fn xmlSchemaSetValidErrors(
        ctxt: XmlSchemaValidCtxtPtr,
        err: Option<unsafe extern "C" fn()>,
        warn: Option<unsafe extern "C" fn()>,
        ctx: *mut c_void,
    );

    fn xmlSetGenericErrorFunc(ctx: *mut c_void, handler: Option<unsafe extern "C" fn()>);
    fn xmlSetStructuredErrorFunc(ctx: *mut c_void, handler: Option<unsafe extern "C" fn()>);
}

unsafe extern "C" fn libxml2_swallow() {}

/// libxml2 has two parallel error-dispatch paths (a generic printf-style
/// handler and a richer "structured" one); we have to silence both, and
/// because both are stored in *thread-local* defaults the call has to
/// happen on every thread that drives libxml2 — including each worker.
fn install_libxml2_silencer() {
    unsafe {
        xmlSetGenericErrorFunc(std::ptr::null_mut(), Some(libxml2_swallow));
        xmlSetStructuredErrorFunc(std::ptr::null_mut(), Some(libxml2_swallow));
    }
}

fn path_to_cstring(p: &Path) -> Option<CString> {
    CString::new(p.to_str()?).ok()
}

// ── outcomes & backend trait ─────────────────────────────────────────────────

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Validity { Valid, Invalid }

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Outcome {
    Valid,
    Invalid,
    /// Instance test where the prior schemaTest didn't yield a usable
    /// schema (failed compile or timed out).  Not counted as pass or
    /// fail for the backend — kept as its own column.
    NoSchema,
    /// Hit the per-test timeout.  Counted as its own column.
    Timeout,
}

trait Backend {
    fn name(&self) -> &'static str;
    fn compile_schema(&mut self, schema_path: &Path, timeout: Duration) -> Outcome;
    fn validate_instance(&mut self, instance_path: &Path, timeout: Duration) -> Outcome;
}

// ── worker thread plumbing ───────────────────────────────────────────────────
//
// One dedicated worker per backend.  All compile/validate work happens on
// the worker thread, communicating via channels.  If a request exceeds
// the per-test timeout, the front-end drops the channel and respawns the
// worker.  The orphaned worker continues running until either it returns
// (and its result is silently discarded — the receiver is gone) or the
// process exits.  This deliberately *leaks* memory inside the orphaned
// worker (its cached schema, the in-flight FFI allocations), which is
// fine for a bench: at most one orphan per timeout, and we exit shortly
// after the run completes.

enum Req {
    Compile(PathBuf),
    Validate(PathBuf),
}

/// Worker → front-end reply.  `NoSchema` is a runner-side concept
/// derived from "the last compile failed," not something workers signal.
enum Reply {
    Valid,
    Invalid,
}

struct Worker {
    tx: mpsc::Sender<Req>,
    rx: mpsc::Receiver<Reply>,
}

// ── sup-xml backend ──────────────────────────────────────────────────────────

struct SupXmlBackend {
    worker:             Option<Worker>,
    /// Whether the most recent `compile_schema` produced a usable
    /// schema on the *worker* side.  Tracked here so the front-end
    /// can short-circuit `validate_instance` calls when the prior
    /// compile failed (which would otherwise force the worker to
    /// reply Invalid for every instance, wasting time).
    schema_ready:       bool,
}

impl SupXmlBackend {
    fn new() -> Self { Self { worker: None, schema_ready: false } }

    fn worker_mut(&mut self) -> &mut Worker {
        if self.worker.is_none() {
            self.worker = Some(spawn_supxml_worker());
        }
        self.worker.as_mut().unwrap()
    }

    fn drop_worker(&mut self) {
        self.worker      = None;
        self.schema_ready = false;
    }
}

fn spawn_supxml_worker() -> Worker {
    let (req_tx, req_rx) = mpsc::channel::<Req>();
    let (rep_tx, rep_rx) = mpsc::channel::<Reply>();
    thread::spawn(move || {
        let mut current: Option<Schema> = None;
        while let Ok(req) = req_rx.recv() {
            let reply = match req {
                Req::Compile(p) => {
                    current = None;
                    match std::fs::read(&p) {
                        Err(_) => Reply::Invalid,
                        Ok(b) => {
                            let src = String::from_utf8_lossy(&b).into_owned();
                            let dir = p.parent().unwrap_or(Path::new(".")).to_path_buf();
                            match Schema::compile_with(&src, FsResolver::new(dir)) {
                                Ok(sch) => { current = Some(sch); Reply::Valid }
                                Err(_)  => Reply::Invalid,
                            }
                        }
                    }
                }
                Req::Validate(p) => match current.as_ref() {
                    None => Reply::Invalid,   // defensive; front-end filters
                    Some(schema) => match std::fs::read(&p) {
                        Err(_) => Reply::Invalid,
                        Ok(b)  => {
                            let src = String::from_utf8_lossy(&b).into_owned();
                            match schema.validate_str(&src) {
                                Ok(()) => Reply::Valid,
                                Err(_) => Reply::Invalid,
                            }
                        }
                    }
                }
            };
            if rep_tx.send(reply).is_err() { break; }
        }
    });
    Worker { tx: req_tx, rx: rep_rx }
}

impl Backend for SupXmlBackend {
    fn name(&self) -> &'static str { "sup-xml" }

    fn compile_schema(&mut self, path: &Path, timeout: Duration) -> Outcome {
        let worker = self.worker_mut();
        if worker.tx.send(Req::Compile(path.to_path_buf())).is_err() {
            self.drop_worker();
            return Outcome::Timeout;
        }
        match worker.rx.recv_timeout(timeout) {
            Ok(Reply::Valid)   => { self.schema_ready = true;  Outcome::Valid   }
            Ok(Reply::Invalid) => { self.schema_ready = false; Outcome::Invalid }
            Err(_) => {
                self.drop_worker();
                Outcome::Timeout
            }
        }
    }

    fn validate_instance(&mut self, path: &Path, timeout: Duration) -> Outcome {
        if !self.schema_ready { return Outcome::NoSchema; }
        let worker = self.worker_mut();
        if worker.tx.send(Req::Validate(path.to_path_buf())).is_err() {
            self.drop_worker();
            return Outcome::Timeout;
        }
        match worker.rx.recv_timeout(timeout) {
            Ok(Reply::Valid)   => Outcome::Valid,
            Ok(Reply::Invalid) => Outcome::Invalid,
            Err(_) => {
                self.drop_worker();
                Outcome::Timeout
            }
        }
    }
}

// ── libxml2 backend ──────────────────────────────────────────────────────────

struct Libxml2Backend {
    worker:       Option<Worker>,
    schema_ready: bool,
}

impl Libxml2Backend {
    fn new() -> Self { Self { worker: None, schema_ready: false } }

    fn worker_mut(&mut self) -> &mut Worker {
        if self.worker.is_none() {
            self.worker = Some(spawn_libxml2_worker());
        }
        self.worker.as_mut().unwrap()
    }

    fn drop_worker(&mut self) {
        self.worker       = None;
        self.schema_ready = false;
    }
}

fn spawn_libxml2_worker() -> Worker {
    let (req_tx, req_rx) = mpsc::channel::<Req>();
    let (rep_tx, rep_rx) = mpsc::channel::<Reply>();
    thread::spawn(move || {
        // libxml2's error-handler defaults are thread-local — silence
        // again here so the worker doesn't print to stderr.
        install_libxml2_silencer();
        // current is only ever touched on this thread — *mut c_void is
        // !Send but never crosses thread boundaries here.
        let mut current: Option<XmlSchemaPtr> = None;
        while let Ok(req) = req_rx.recv() {
            let reply = match req {
                Req::Compile(p) => {
                    if let Some(s) = current.take() {
                        unsafe { xmlSchemaFree(s); }
                    }
                    match path_to_cstring(&p) {
                        None => Reply::Invalid,
                        Some(c_path) => unsafe {
                            let ctx = xmlSchemaNewParserCtxt(c_path.as_ptr());
                            if ctx.is_null() {
                                Reply::Invalid
                            } else {
                                xmlSchemaSetParserErrors(ctx, None, None, std::ptr::null_mut());
                                let s = xmlSchemaParse(ctx);
                                xmlSchemaFreeParserCtxt(ctx);
                                if s.is_null() {
                                    Reply::Invalid
                                } else {
                                    current = Some(s);
                                    Reply::Valid
                                }
                            }
                        }
                    }
                }
                Req::Validate(p) => match current {
                    None => Reply::Invalid,   // defensive; front-end filters
                    Some(schema) => match path_to_cstring(&p) {
                        None => Reply::Invalid,
                        Some(c_path) => unsafe {
                            let doc = xmlReadFile(c_path.as_ptr(), std::ptr::null(), 0);
                            if doc.is_null() {
                                Reply::Invalid
                            } else {
                                let v = xmlSchemaNewValidCtxt(schema);
                                xmlSchemaSetValidErrors(v, None, None, std::ptr::null_mut());
                                let ok = xmlSchemaValidateDoc(v, doc) == 0;
                                xmlSchemaFreeValidCtxt(v);
                                xmlFreeDoc(doc);
                                if ok { Reply::Valid } else { Reply::Invalid }
                            }
                        }
                    }
                }
            };
            if rep_tx.send(reply).is_err() { break; }
        }
        // On normal exit, free the cached schema.  (An abandoned worker
        // never reaches here — its req_rx never closes from the
        // worker's POV until the front-end drops the sender, which
        // only happens on timeout-abandon.)
        if let Some(s) = current {
            unsafe { xmlSchemaFree(s); }
        }
    });
    Worker { tx: req_tx, rx: rep_rx }
}

impl Backend for Libxml2Backend {
    fn name(&self) -> &'static str { "libxml2" }

    fn compile_schema(&mut self, path: &Path, timeout: Duration) -> Outcome {
        let worker = self.worker_mut();
        if worker.tx.send(Req::Compile(path.to_path_buf())).is_err() {
            self.drop_worker();
            return Outcome::Timeout;
        }
        match worker.rx.recv_timeout(timeout) {
            Ok(Reply::Valid)   => { self.schema_ready = true;  Outcome::Valid   }
            Ok(Reply::Invalid) => { self.schema_ready = false; Outcome::Invalid }
            Err(_) => {
                self.drop_worker();
                Outcome::Timeout
            }
        }
    }

    fn validate_instance(&mut self, path: &Path, timeout: Duration) -> Outcome {
        if !self.schema_ready { return Outcome::NoSchema; }
        let worker = self.worker_mut();
        if worker.tx.send(Req::Validate(path.to_path_buf())).is_err() {
            self.drop_worker();
            return Outcome::Timeout;
        }
        match worker.rx.recv_timeout(timeout) {
            Ok(Reply::Valid)   => Outcome::Valid,
            Ok(Reply::Invalid) => Outcome::Invalid,
            Err(_) => {
                self.drop_worker();
                Outcome::Timeout
            }
        }
    }
}

// ── manifest parsing ─────────────────────────────────────────────────────────

#[derive(Debug)]
struct TestCase {
    kind:     TestKind,
    /// Primary document is `hrefs[0]`; additional entries (sun/boeing
    /// style) list companion schemas reachable via the same directory.
    hrefs:    Vec<String>,
    expected: Validity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestKind { Schema, Instance }

fn parse_test_set(xml: &str) -> Result<Vec<TestCase>, String> {
    let mut reader = XmlReader::from_str(xml);
    let mut cases: Vec<TestCase> = Vec::new();

    let mut in_schema_test = false;
    let mut in_instance_test = false;
    let mut pending_hrefs: Vec<String> = Vec::new();

    loop {
        let ev = reader.next().map_err(|e| e.to_string())?;
        match ev {
            Event::StartElement(tag) => {
                let name = tag.name().to_string();
                let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(&name).to_string();
                match local.as_str() {
                    "schemaTest"   => { in_schema_test = true;   pending_hrefs.clear(); }
                    "instanceTest" => { in_instance_test = true; pending_hrefs.clear(); }
                    "schemaDocument" | "instanceDocument" => {
                        for a in tag.attrs() {
                            let a = a.map_err(|e| e.to_string())?;
                            if a.name() == "xlink:href" || a.name() == "href" {
                                pending_hrefs.push(a.value().to_string());
                            }
                        }
                    }
                    "expected" => {
                        let mut validity: Option<Validity> = None;
                        for a in tag.attrs() {
                            let a = a.map_err(|e| e.to_string())?;
                            if a.name() == "validity" {
                                validity = match a.value() {
                                    "valid"   => Some(Validity::Valid),
                                    "invalid" => Some(Validity::Invalid),
                                    _ => None,
                                };
                            }
                        }
                        if let Some(v) = validity {
                            if !pending_hrefs.is_empty() {
                                let kind = if in_schema_test { TestKind::Schema }
                                           else if in_instance_test { TestKind::Instance }
                                           else { continue };
                                cases.push(TestCase {
                                    kind,
                                    hrefs: std::mem::take(&mut pending_hrefs),
                                    expected: v,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::EndElement(tag) => {
                let name = tag.name().to_string();
                let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(&name).to_string();
                match local.as_str() {
                    "schemaTest"   => in_schema_test = false,
                    "instanceTest" => in_instance_test = false,
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(cases)
}

// ── per-backend tally ────────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct BackendStats {
    schema_pass:        usize,
    schema_fail:        usize,
    schema_timeout:     usize,
    instance_pass:      usize,
    instance_fail:      usize,
    /// Instance tests that this backend couldn't attempt because its
    /// preceding schemaTest didn't yield a usable schema.  Counted
    /// separately so we don't pretend they were passes or fails.
    instance_no_schema: usize,
    instance_timeout:   usize,
    /// Wall-clock spent inside `compile_schema` / `validate_instance`
    /// for this backend, summed across every test (including timeouts,
    /// which contribute the full timeout budget).
    schema_time:        Duration,
    instance_time:      Duration,
}

impl BackendStats {
    fn add(&mut self, other: &BackendStats) {
        self.schema_pass       += other.schema_pass;
        self.schema_fail       += other.schema_fail;
        self.schema_timeout    += other.schema_timeout;
        self.instance_pass     += other.instance_pass;
        self.instance_fail     += other.instance_fail;
        self.instance_no_schema += other.instance_no_schema;
        self.instance_timeout  += other.instance_timeout;
        self.schema_time       += other.schema_time;
        self.instance_time     += other.instance_time;
    }

    // schema_total / instance_total used to report per-backend
    // denominator; they're now computed from the shared
    // ContribStats total so backends with more no-schema outcomes
    // can't shrink their own denominator.  Kept as helpers in case
    // future per-backend views want them again.
    #[allow(dead_code)]
    fn schema_total(&self)   -> usize { self.schema_pass + self.schema_fail }
    #[allow(dead_code)]
    fn instance_total(&self) -> usize { self.instance_pass + self.instance_fail }
}

/// Per-contributor accumulator: one column of stats per backend, plus
/// per-test agreement counts (how often every backend that produced
/// a verdict produced the same one — regardless of suite-expected).
#[derive(Clone)]
struct ContribStats {
    per_backend:    Vec<BackendStats>,
    schema_agree:   usize,
    schema_total:   usize,
    instance_agree: usize,
    instance_total: usize,
}

impl ContribStats {
    fn new(n_backends: usize) -> Self {
        Self {
            per_backend:    vec![BackendStats::default(); n_backends],
            schema_agree:   0,
            schema_total:   0,
            instance_agree: 0,
            instance_total: 0,
        }
    }

    fn add(&mut self, other: &ContribStats) {
        for (a, b) in self.per_backend.iter_mut().zip(other.per_backend.iter()) {
            a.add(b);
        }
        self.schema_agree   += other.schema_agree;
        self.schema_total   += other.schema_total;
        self.instance_agree += other.instance_agree;
        self.instance_total += other.instance_total;
    }
}

/// One record per (backend, test) timeout.  Printed at the end so you
/// can tell which schemas/instances caused which backend to give up.
struct TimeoutRecord {
    backend: usize,
    kind:    TestKind,
    path:    PathBuf,
}

// ── runner ───────────────────────────────────────────────────────────────────

fn run_test_set(
    manifest:  &Path,
    backends:  &mut [Box<dyn Backend>],
    timeout:   Duration,
    timeouts:  &mut Vec<TimeoutRecord>,
    verbose:   bool,
) -> ContribStats {
    let mut stats = ContribStats::new(backends.len());

    let manifest_dir = manifest.parent().unwrap_or(Path::new("."));
    let Ok(manifest_src) = std::fs::read_to_string(manifest) else { return stats; };
    let Ok(cases) = parse_test_set(&manifest_src) else { return stats; };

    for case in cases {
        let Some(primary_href) = case.hrefs.first() else { continue };
        let primary_path = manifest_dir.join(primary_href);

        match case.kind {
            TestKind::Schema => {
                stats.schema_total += 1;
                let mut outcomes: Vec<Outcome> = Vec::with_capacity(backends.len());
                for (i, b) in backends.iter_mut().enumerate() {
                    let t0 = Instant::now();
                    let o  = b.compile_schema(&primary_path, timeout);
                    let dt = t0.elapsed();
                    // Only accumulate wall-clock for cases that
                    // completed (Valid/Invalid).  Timed-out runs
                    // would contribute ~= the timeout budget each
                    // and dominate the per-backend totals — they're
                    // tracked via `schema_timeout` and surfaced
                    // with an asterisk in `print_timing` instead, so
                    // the "completed-cases wall-clock" comparison
                    // stays apples-to-apples.
                    match o {
                        Outcome::Valid | Outcome::Invalid => {
                            stats.per_backend[i].schema_time += dt;
                            let got = if matches!(o, Outcome::Valid)
                                { Validity::Valid } else { Validity::Invalid };
                            if got == case.expected {
                                stats.per_backend[i].schema_pass += 1;
                            } else {
                                stats.per_backend[i].schema_fail += 1;
                            }
                        }
                        Outcome::Timeout => {
                            stats.per_backend[i].schema_timeout += 1;
                            timeouts.push(TimeoutRecord {
                                backend: i, kind: TestKind::Schema,
                                path: primary_path.clone(),
                            });
                            if verbose {
                                eprintln!("  TIMEOUT schema [{}]  {}",
                                    b.name(), primary_path.display());
                            }
                        }
                        Outcome::NoSchema => unreachable!("compile never returns NoSchema"),
                    }
                    outcomes.push(o);
                }
                if all_agree(&outcomes) {
                    stats.schema_agree += 1;
                } else if verbose {
                    eprintln!("  schema disagree: {} expected={:?}  {}",
                        primary_path.display(), case.expected,
                        format_outcomes(backends, &outcomes));
                }
            }
            TestKind::Instance => {
                stats.instance_total += 1;
                let mut outcomes: Vec<Outcome> = Vec::with_capacity(backends.len());
                for (i, b) in backends.iter_mut().enumerate() {
                    let t0 = Instant::now();
                    let o  = b.validate_instance(&primary_path, timeout);
                    let dt = t0.elapsed();
                    // Same exclusion rule as the schema arm — only
                    // completed runs (Valid/Invalid) contribute to
                    // the wall-clock; no-schema is "couldn't even
                    // try" (zero work) and timeout is reported as
                    // an asterisk rather than as a 30-second
                    // contribution to the total.
                    match o {
                        Outcome::Valid | Outcome::Invalid => {
                            stats.per_backend[i].instance_time += dt;
                            let got = if matches!(o, Outcome::Valid)
                                { Validity::Valid } else { Validity::Invalid };
                            if got == case.expected {
                                stats.per_backend[i].instance_pass += 1;
                            } else {
                                stats.per_backend[i].instance_fail += 1;
                            }
                        }
                        Outcome::NoSchema => {
                            stats.per_backend[i].instance_no_schema += 1;
                        }
                        Outcome::Timeout => {
                            stats.per_backend[i].instance_timeout += 1;
                            timeouts.push(TimeoutRecord {
                                backend: i, kind: TestKind::Instance,
                                path: primary_path.clone(),
                            });
                            if verbose {
                                eprintln!("  TIMEOUT instance [{}]  {}",
                                    b.name(), primary_path.display());
                            }
                        }
                    }
                    outcomes.push(o);
                }
                // Agreement requires every backend to have produced a
                // concrete verdict (Valid or Invalid) and to match.
                // NoSchema / Timeout don't contribute either way.
                if outcomes.iter().all(|o| matches!(o, Outcome::Valid | Outcome::Invalid))
                    && all_agree(&outcomes)
                {
                    stats.instance_agree += 1;
                } else if verbose {
                    eprintln!("  instance disagree: {} expected={:?}  {}",
                        primary_path.display(), case.expected,
                        format_outcomes(backends, &outcomes));
                }
            }
        }
    }
    stats
}

fn all_agree(o: &[Outcome]) -> bool {
    o.windows(2).all(|w| w[0] == w[1])
}

fn format_outcomes(backends: &[Box<dyn Backend>], outcomes: &[Outcome]) -> String {
    let mut s = String::new();
    for (b, o) in backends.iter().zip(outcomes.iter()) {
        if !s.is_empty() { s.push_str("  "); }
        let cell = match o {
            Outcome::Valid     => "valid",
            Outcome::Invalid   => "invalid",
            Outcome::NoSchema  => "no-schema",
            Outcome::Timeout   => "TIMEOUT",
        };
        s.push_str(&format!("{}={}", b.name(), cell));
    }
    s
}

// ── entry point ──────────────────────────────────────────────────────────────

fn main() {
    install_libxml2_silencer();

    let verbose  = std::env::var("XSTS_VERBOSE").is_ok();
    let filter   = std::env::var("XSTS_FILTER").ok();
    let progress = std::env::var("XSTS_QUIET").is_err();
    let timeout  = Duration::from_secs(
        std::env::var("XSTS_TIMEOUT").ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
    );

    let root = PathBuf::from(XSTS_ROOT);
    if !root.exists() {
        eprintln!(
            "XSTS not present at {}\n\
             Run `tests/assets/xsts/fetch.sh` to download the W3C suite (~4MB).",
            root.display()
        );
        return;
    }

    let mut backends: Vec<Box<dyn Backend>> = vec![
        Box::new(SupXmlBackend::new()),
        Box::new(Libxml2Backend::new()),
    ];
    let backend_names: Vec<&'static str> = backends.iter().map(|b| b.name()).collect();

    println!("\nW3C XML Schema Test Suite — cross-implementation runner");
    println!("Backends: {}", backend_names.join(", "));
    println!("Per-test timeout: {:?}\n", timeout);

    // First pass: collect the manifests we'll actually run so the
    // progress line can say "[N/M]" instead of just N.  Directory
    // walk is trivially cheap compared with the run itself.
    let mut planned: Vec<(&str, PathBuf)> = Vec::new();
    for contributor in &["sunMeta", "boeingMeta", "nistMeta", "msMeta"] {
        let dir = root.join(contributor);
        if !dir.exists() { continue; }
        let Ok(read_dir) = std::fs::read_dir(&dir) else { continue };
        for entry in read_dir.flatten() {
            let path = entry.path();
            // sun/boeing/nist use `.testSet`; ms uses `.xml`.
            let ext = path.extension().and_then(|s| s.to_str());
            if ext != Some("testSet") && ext != Some("xml") { continue; }
            if let Some(f) = &filter {
                if !path.to_string_lossy().contains(f) { continue; }
            }
            planned.push((contributor, path));
        }
    }
    let total_manifests = planned.len();
    if progress {
        eprintln!("Running {} manifests...", total_manifests);
    }

    let mut totals = ContribStats::new(backends.len());
    let mut by_contributor_map: std::collections::BTreeMap<String, ContribStats> =
        std::collections::BTreeMap::new();
    let mut all_timeouts: Vec<TimeoutRecord> = Vec::new();
    let suite_t0 = Instant::now();

    for (i, (contributor, path)) in planned.iter().enumerate() {
        let manifest_t0 = Instant::now();
        let s = run_test_set(path, &mut backends, timeout, &mut all_timeouts, verbose);
        let dt = manifest_t0.elapsed();
        if progress {
            let fname = path.file_name()
                .and_then(|s| s.to_str()).unwrap_or("?");
            // Show running pass counts per backend on the progress line
            // so it's obvious if one is falling behind.
            let pf: Vec<String> = s.per_backend.iter().map(|b| {
                format!("{}+{}+{}/{}",
                    b.schema_pass + b.instance_pass,
                    b.schema_timeout + b.instance_timeout,
                    b.instance_no_schema,
                    s.schema_total + s.instance_total,
                )
            }).collect();
            eprintln!(
                "  [{:>4}/{:<4}]  {:>10}/{:<32}  schema {:>3}  inst {:>4}  {:>10}  [{}]",
                i + 1, total_manifests, contributor, fname,
                s.schema_total, s.instance_total,
                fmt_dur(dt), pf.join("  "),
            );
        }
        totals.add(&s);
        by_contributor_map.entry(contributor.to_string())
            .or_insert_with(|| ContribStats::new(backends.len()))
            .add(&s);
    }
    if progress {
        eprintln!("All manifests complete in {}.", fmt_dur(suite_t0.elapsed()));
    }

    // Render contributors in canonical order, dropping any that produced
    // no tests (e.g. when XSTS_FILTER excludes a whole contributor).
    let order = ["sunMeta", "boeingMeta", "nistMeta", "msMeta"];
    let by_contributor: Vec<(String, ContribStats)> = order.iter()
        .filter_map(|c| by_contributor_map.remove(*c).map(|s| (c.to_string(), s)))
        .collect();

    print_table("schemaTest", &backend_names, &by_contributor, &totals, true);
    print_table("instanceTest", &backend_names, &by_contributor, &totals, false);
    print_timing(&backend_names, &totals);
    print_timeouts(&backend_names, &all_timeouts);

    println!("\nManifests walked: {}", total_manifests);
    if filter.is_some() {
        println!("(filter active — XSTS_FILTER={:?})", std::env::var("XSTS_FILTER").ok());
    }
}

/// Print one of the two tables (schema or instance).  Columns per
/// backend: pass / total / pass-pct / no-schema / timeout.  The progress
/// line on stderr only shows pass — this table is the canonical record.
fn print_table(
    kind: &str,
    backend_names: &[&'static str],
    by_contributor: &[(String, ContribStats)],
    totals: &ContribStats,
    is_schema: bool,
) {
    let total_n = if is_schema { totals.schema_total } else { totals.instance_total };
    if total_n == 0 { return; }

    let n_contributors = by_contributor.iter()
        .filter(|(_, s)| (if is_schema { s.schema_total } else { s.instance_total }) > 0)
        .count();
    println!("\n  XSTS {} conformance — {} tests across {} contributors",
             kind, total_n, n_contributors);

    print!("  {:<14}  {:>6}", "contributor", "n");
    for name in backend_names {
        // Each backend column is wide enough for "1234/1234 99.9% +to:99 +ns:99"
        print!("  {:>34}", name);
    }
    print!("  {:>6}", "agree");
    println!();

    for (name, s) in by_contributor {
        let n = if is_schema { s.schema_total } else { s.instance_total };
        if n == 0 { continue; }
        print!("  {:<14}  {:>6}", name, n);
        for bs in &s.per_backend {
            print!("  {}", fmt_cell(bs, is_schema, n));
        }
        let agree = if is_schema { s.schema_agree } else { s.instance_agree };
        let agree_total = if is_schema { s.schema_total } else { s.instance_total };
        print!("  {:>6}", fmt_pct(agree, agree_total));
        println!();
    }

    print!("  {:<14}  {:>6}", "TOTAL", total_n);
    for bs in &totals.per_backend {
        print!("  {}", fmt_cell(bs, is_schema, total_n));
    }
    let agree = if is_schema { totals.schema_agree } else { totals.instance_agree };
    print!("  {:>6}", fmt_pct(agree, total_n));
    println!();
}

/// One row cell: pass/total + pct, plus `+to:N` if any timeouts and
/// `+ns:N` if any no-schema (instance only).  Fixed-width so the table
/// columns line up.
///
/// The denominator is the COMMON attempted-test count (`scope_total`)
/// across every backend, not this backend's pass+fail tally.  That
/// way "no-schema" outcomes (the schema didn't compile so we couldn't
/// even attempt to validate the instance) count against the backend
/// that produced them, rather than silently shrinking its denominator
/// — otherwise libxml2's higher schema-compile failure rate would
/// flatter its instance percentage.
fn fmt_cell(bs: &BackendStats, is_schema: bool, scope_total: usize) -> String {
    let (pass, tos, ns) = if is_schema {
        (bs.schema_pass, bs.schema_timeout, 0usize)
    } else {
        (bs.instance_pass, bs.instance_timeout, bs.instance_no_schema)
    };
    let core = if scope_total == 0 {
        "—".to_string()
    } else {
        format!("{}/{} {:>5.1}%", pass, scope_total, pass as f64 * 100.0 / scope_total as f64)
    };
    let mut suffix = String::new();
    if tos > 0 { suffix.push_str(&format!(" +to:{}", tos)); }
    if ns  > 0 { suffix.push_str(&format!(" +ns:{}", ns)); }
    format!("{:>34}", format!("{}{}", core, suffix))
}

fn fmt_pct(n: usize, d: usize) -> String {
    if d == 0 { "—".to_string() }
    else { format!("{:>4.1}%", n as f64 * 100.0 / d as f64) }
}

/// Per-backend total wall-clock spent inside compile + validate, plus
/// the combined total.  Timed-out cases are EXCLUDED from these
/// numbers — they'd otherwise contribute ~= the timeout budget each
/// (currently 30 s) and dominate the per-backend totals on a single
/// pathological schema.  The count of excluded cases is appended as
/// `*N timeouts` after the relevant column so the asterisk is
/// visible in-line, and the suite footer prints which tests they were.
fn print_timing(backend_names: &[&'static str], totals: &ContribStats) {
    println!("\n  XSTS wall-clock (completed cases only; timeouts excluded)");
    println!("  {:<14}  {:>20}  {:>20}  {:>20}",
             "backend", "compile", "validate", "total");
    for (i, name) in backend_names.iter().enumerate() {
        let bs = &totals.per_backend[i];
        let total = bs.schema_time + bs.instance_time;
        let compile_cell  = annotate_timeouts(fmt_dur(bs.schema_time),  bs.schema_timeout);
        let validate_cell = annotate_timeouts(fmt_dur(bs.instance_time), bs.instance_timeout);
        let total_to = bs.schema_timeout + bs.instance_timeout;
        let total_cell = annotate_timeouts(fmt_dur(total), total_to);
        println!("  {:<14}  {:>20}  {:>20}  {:>20}",
                 name, compile_cell, validate_cell, total_cell);
    }
}

/// Append `*N timeouts` to a duration string when the backend had
/// timed-out cases that aren't in the duration.  Empty suffix when
/// there were none.  Keeps the asterisk inline so a reader doesn't
/// have to cross-reference the suite footer to spot it.
fn annotate_timeouts(dur: String, count: usize) -> String {
    if count == 0 { dur }
    else { format!("{dur} *{count} timeouts") }
}

/// Enumerate the tests that timed out, grouped by backend.  Caps the
/// per-backend list at 50 entries to keep the bench output bounded
/// when something pathological happens; the total count is always
/// shown so you know how many were elided.
fn print_timeouts(backend_names: &[&'static str], timeouts: &[TimeoutRecord]) {
    if timeouts.is_empty() {
        println!("\n  No tests timed out.");
        return;
    }
    println!("\n  Per-backend timeouts ({} total)", timeouts.len());
    for (i, name) in backend_names.iter().enumerate() {
        let mine: Vec<&TimeoutRecord> = timeouts.iter()
            .filter(|t| t.backend == i).collect();
        if mine.is_empty() { continue; }
        println!("    {} — {}:", name, mine.len());
        for t in mine.iter().take(50) {
            let kind = match t.kind { TestKind::Schema => "schema  ",
                                      TestKind::Instance => "instance" };
            println!("      {}  {}", kind, t.path.display());
        }
        if mine.len() > 50 {
            println!("      … (+{} more)", mine.len() - 50);
        }
    }
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s >= 1.0      { format!("{:>6.2} s",  s) }
    else if s >= 1e-3 { format!("{:>6.2} ms", s * 1e3) }
    else              { format!("{:>6.2} µs", s * 1e6) }
}
