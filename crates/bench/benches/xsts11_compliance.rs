//! W3C XSD 1.1-additions conformance runner.
//!
//! Walks every `.testSet` manifest under
//! `tests/assets/xsts-1.1/{saxon,ibm,oracle,wg}Meta/`, dispatches each
//! `<schemaTest>` to sup-xml (in `SchemaVersion::Xsd11` mode) and
//! libxml2, and reports per-contributor pass rates.  These are the
//! XSD 1.1-specific tests submitted to W3C by Saxonica (2010), IBM
//! (2011), Oracle (2011), and the Working Group.
//!
//! Fetch the corpus first via `tests/assets/xsts-1.1/fetch.sh`.
//!
//! Expectations:
//!
//! * libxml2 doesn't implement XSD 1.1 — every schemaTest that uses a
//!   1.1 construct will fail.  Reported here for grounding only.
//! * sup-xml today implements ~0% of 1.1 — the `SchemaVersion::Xsd11`
//!   API surface is in place but the underlying features
//!   (`xs:assert`, `xs:alternative`, `xs:override`, open content, the
//!   new datatypes, `xs:explicitTimezone`, …) are not yet wired up.
//!   This bench establishes the starting baseline; each phase-1 PR
//!   should move the number up.
//!
//! Same per-test timeout + worker-thread machinery as the 1.0 bench
//! (`xsts_compliance.rs`).  Most of the file is a verbatim adaptation
//! of that bench with the corpus root and sup-xml mode swapped.
//!
//! Run:
//!     cargo bench -p sup-xml-bench --bench xsts11_compliance
//!
//! Env vars:
//!     XSTS11_FILTER=<sub>  only run manifests whose path contains <sub>
//!     XSTS11_QUIET=1       suppress per-manifest progress
//!     XSTS11_TIMEOUT=<N>   per-test wall-clock budget in seconds (default: 30)

#![allow(clippy::missing_safety_doc)]

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use sup_xml::xsd::{FsResolver, Schema, SchemaOptions, SchemaVersion};
use sup_xml::{Event, XmlReader};

const XSTS_ROOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/assets/xsts-1.1"
);

const DEFAULT_TIMEOUT_SECS: u64 = 30;

// ── libxml2 FFI ──────────────────────────────────────────────────────────────
//
// Same shape as the 1.0 bench — XSD parser ctxt + validator ctxt with
// stderr-silenced error handlers.  libxml2 has no XSD 1.1 support, so
// most schemaTests will fail at compile time; we include it anyway so
// the table has a "baseline reference C implementation" column.

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
enum Outcome { Valid, Invalid, NoSchema, Timeout }

trait Backend {
    fn name(&self) -> &'static str;
    fn compile_schema(&mut self, schema_path: &Path, timeout: Duration) -> Outcome;
    fn validate_instance(&mut self, instance_path: &Path, timeout: Duration) -> Outcome;
}

// ── worker thread plumbing (identical to xsts_compliance) ────────────────────

enum Req {
    Compile(PathBuf),
    Validate(PathBuf),
}

enum Reply { Valid, Invalid }

struct Worker {
    tx: mpsc::Sender<Req>,
    rx: mpsc::Receiver<Reply>,
}

// ── sup-xml backend ──────────────────────────────────────────────────────────

struct SupXmlBackend {
    worker:       Option<Worker>,
    schema_ready: bool,
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
        self.worker       = None;
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
                            let opts = SchemaOptions {
                                version: SchemaVersion::Xsd11,
                                ..Default::default()
                            };
                            match Schema::compile_with_options(
                                &src, FsResolver::new(dir), opts,
                            ) {
                                Ok(sch) => { current = Some(sch); Reply::Valid }
                                Err(_)  => Reply::Invalid,
                            }
                        }
                    }
                }
                Req::Validate(p) => match current.as_ref() {
                    None => Reply::Invalid,
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
    fn name(&self) -> &'static str { "sup-xml-1.1" }

    fn compile_schema(&mut self, path: &Path, timeout: Duration) -> Outcome {
        let worker = self.worker_mut();
        if worker.tx.send(Req::Compile(path.to_path_buf())).is_err() {
            self.drop_worker();
            return Outcome::Timeout;
        }
        match worker.rx.recv_timeout(timeout) {
            Ok(Reply::Valid)   => { self.schema_ready = true;  Outcome::Valid   }
            Ok(Reply::Invalid) => { self.schema_ready = false; Outcome::Invalid }
            Err(_) => { self.drop_worker(); Outcome::Timeout }
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
            Err(_) => { self.drop_worker(); Outcome::Timeout }
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
        install_libxml2_silencer();
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
                    None => Reply::Invalid,
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
        if let Some(s) = current { unsafe { xmlSchemaFree(s); } }
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
            Err(_) => { self.drop_worker(); Outcome::Timeout }
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
            Err(_) => { self.drop_worker(); Outcome::Timeout }
        }
    }
}

// ── manifest parsing (same shape as xsts_compliance) ─────────────────────────

#[derive(Debug)]
struct TestCase {
    kind:     TestKind,
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

// ── stats ────────────────────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct BackendStats {
    schema_pass:        usize,
    schema_fail:        usize,
    schema_timeout:     usize,
    instance_pass:      usize,
    instance_fail:      usize,
    instance_no_schema: usize,
    instance_timeout:   usize,
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

    // schema_total / instance_total — see the matching helpers in
    // xsts_compliance.rs.  Kept for symmetry with the 1.0 bench;
    // the rendered cells use the shared contributor denominator
    // instead so no-schema outcomes count fairly.
    #[allow(dead_code)]
    fn schema_total(&self)   -> usize { self.schema_pass + self.schema_fail }
    #[allow(dead_code)]
    fn instance_total(&self) -> usize { self.instance_pass + self.instance_fail }
}

#[derive(Clone)]
struct ContribStats {
    per_backend:    Vec<BackendStats>,
    schema_total:   usize,
    instance_total: usize,
}

impl ContribStats {
    fn new(n_backends: usize) -> Self {
        Self { per_backend: vec![BackendStats::default(); n_backends],
               schema_total: 0, instance_total: 0 }
    }
    fn add(&mut self, other: &ContribStats) {
        for (a, b) in self.per_backend.iter_mut().zip(other.per_backend.iter()) {
            a.add(b);
        }
        self.schema_total   += other.schema_total;
        self.instance_total += other.instance_total;
    }
}

// ── runner ───────────────────────────────────────────────────────────────────

fn run_test_set(
    manifest:  &Path,
    backends:  &mut [Box<dyn Backend>],
    timeout:   Duration,
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
                for (i, b) in backends.iter_mut().enumerate() {
                    let t0 = Instant::now();
                    let o  = b.compile_schema(&primary_path, timeout);
                    let dt = t0.elapsed();
                    // Same exclusion rule as `xsts_compliance.rs`:
                    // timed-out cases would contribute ~= the
                    // timeout budget each and bloat the totals.
                    // The count is surfaced via the `*N timeouts`
                    // asterisk in `print_timing`.
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
                        Outcome::Timeout => stats.per_backend[i].schema_timeout += 1,
                        Outcome::NoSchema => {}
                    }
                }
            }
            TestKind::Instance => {
                stats.instance_total += 1;
                for (i, b) in backends.iter_mut().enumerate() {
                    let t0 = Instant::now();
                    let o  = b.validate_instance(&primary_path, timeout);
                    let dt = t0.elapsed();
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
                        Outcome::NoSchema => stats.per_backend[i].instance_no_schema += 1,
                        Outcome::Timeout  => stats.per_backend[i].instance_timeout += 1,
                    }
                }
            }
        }
    }
    stats
}

fn main() {
    install_libxml2_silencer();

    let filter   = std::env::var("XSTS11_FILTER").ok();
    let progress = std::env::var("XSTS11_QUIET").is_err();
    let timeout  = Duration::from_secs(
        std::env::var("XSTS11_TIMEOUT").ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
    );

    let root = PathBuf::from(XSTS_ROOT);
    if !root.exists() || !root.join("saxonMeta").exists() {
        eprintln!(
            "XSTS 1.1 corpus not present at {}\n\
             Run `tests/assets/xsts-1.1/fetch.sh` to download (~11 MB).",
            root.display()
        );
        return;
    }

    let mut backends: Vec<Box<dyn Backend>> = vec![
        Box::new(SupXmlBackend::new()),
        Box::new(Libxml2Backend::new()),
    ];
    let backend_names: Vec<&'static str> = backends.iter().map(|b| b.name()).collect();

    println!("\nW3C XML Schema 1.1 — additions-only corpus");
    println!("Backends: {}", backend_names.join(", "));
    println!("Per-test timeout: {:?}\n", timeout);

    let mut planned: Vec<(&str, PathBuf)> = Vec::new();
    for contributor in &["saxonMeta", "ibmMeta", "oracleMeta", "wgMeta"] {
        let dir = root.join(contributor);
        if !dir.exists() { continue; }
        let Ok(read_dir) = std::fs::read_dir(&dir) else { continue };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("testSet") { continue; }
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
    let mut by_contributor: std::collections::BTreeMap<String, ContribStats> =
        std::collections::BTreeMap::new();
    let mut by_testset: Vec<(String, ContribStats)> = Vec::new();
    let per_testset = std::env::var("XSTS11_PER_SET").is_ok();
    let suite_t0 = Instant::now();

    for (i, (contributor, path)) in planned.iter().enumerate() {
        let manifest_t0 = Instant::now();
        let s = run_test_set(path, &mut backends, timeout);
        if progress {
            let fname = path.file_name()
                .and_then(|s| s.to_str()).unwrap_or("?");
            eprintln!(
                "  [{:>3}/{:<3}]  {:>11}/{:<32}  schema {:>3}  inst {:>4}  {:>10}",
                i + 1, total_manifests, contributor, fname,
                s.schema_total, s.instance_total,
                fmt_dur(manifest_t0.elapsed()),
            );
        }
        if per_testset {
            let label = format!(
                "{}/{}",
                contributor,
                path.file_name().and_then(|s| s.to_str()).unwrap_or("?"),
            );
            by_testset.push((label, s.clone()));
        }
        totals.add(&s);
        by_contributor.entry(contributor.to_string())
            .or_insert_with(|| ContribStats::new(backends.len()))
            .add(&s);
    }
    if progress {
        eprintln!("All manifests complete in {}.", fmt_dur(suite_t0.elapsed()));
    }

    let order = ["saxonMeta", "ibmMeta", "oracleMeta", "wgMeta"];
    let by_contributor: Vec<(String, ContribStats)> = order.iter()
        .filter_map(|c| by_contributor.remove(*c).map(|s| (c.to_string(), s)))
        .collect();

    print_table("schemaTest", &backend_names, &by_contributor, &totals, true);
    print_table("instanceTest", &backend_names, &by_contributor, &totals, false);

    if per_testset {
        println!("\n  ── per-testSet schemaTest breakdown (sup-xml only) ──");
        println!("  {:<50}  {:>6}  {:>8}", "testSet", "n", "pass%");
        // Sort by descending failure count so the biggest gaps land at the top.
        let mut rows: Vec<(String, usize, usize)> = by_testset.iter()
            .filter_map(|(name, s)| {
                let n = s.schema_total;
                if n == 0 { return None; }
                let p = s.per_backend.first()
                    .map(|b| b.schema_pass)
                    .unwrap_or(0);
                Some((name.clone(), n, p))
            })
            .collect();
        rows.sort_by_key(|(_, n, p)| (n - p) as i64 * -1);
        for (name, n, p) in &rows {
            let rate = if *n == 0 { 0.0 } else { 100.0 * *p as f64 / *n as f64 };
            println!("  {:<50}  {:>6}  {:>7.1}%", name, n, rate);
        }
    }

    println!("\nManifests walked: {}", total_manifests);
}

fn print_table(
    kind: &str,
    backend_names: &[&'static str],
    by_contributor: &[(String, ContribStats)],
    totals: &ContribStats,
    is_schema: bool,
) {
    let total_n = if is_schema { totals.schema_total } else { totals.instance_total };
    if total_n == 0 { return; }

    println!("\n  XSD 1.1 {} conformance — {} tests", kind, total_n);
    print!("  {:<14}  {:>6}", "contributor", "n");
    for name in backend_names {
        print!("  {:>22}", name);
    }
    println!();

    for (name, s) in by_contributor {
        let n = if is_schema { s.schema_total } else { s.instance_total };
        if n == 0 { continue; }
        print!("  {:<14}  {:>6}", name, n);
        for bs in &s.per_backend {
            print!("  {}", fmt_cell(bs, is_schema, n));
        }
        println!();
    }

    print!("  {:<14}  {:>6}", "TOTAL", total_n);
    for bs in &totals.per_backend {
        print!("  {}", fmt_cell(bs, is_schema, total_n));
    }
    println!();
}

/// `scope_total` is the contributor's total attempted-test count
/// (passed to every backend column in the same row).  See the
/// matching comment in `xsts_compliance.rs::fmt_cell` for why we
/// use a shared denominator rather than each backend's own
/// `pass+fail` — no-schema outcomes count against the backend
/// that couldn't compile the schema.
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
    format!("{:>22}", format!("{}{}", core, suffix))
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s >= 1.0      { format!("{:>6.2} s",  s) }
    else if s >= 1e-3 { format!("{:>6.2} ms", s * 1e3) }
    else              { format!("{:>6.2} µs", s * 1e6) }
}
