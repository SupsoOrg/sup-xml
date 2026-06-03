//! W3C XML Conformance Test Suite (XMLTS) runner.
//!
//! Walks every `not-wf/**/*.xml` file under `tests/assets/xmlts/`,
//! parses it with every XML parser we link against, and reports
//! how many each one correctly rejected.  Each `not-wf` file is
//! engineered to violate one specific XML 1.0 well-formedness
//! rule — a conforming parser MUST reject it.
//!
//! Vendored corpora:
//!   - `xmltest/`  — James Clark's original 1998 suite (200 not-wf
//!     files, organised into `sa/`, `not-sa/`, `ext-sa/`)
//!   - `sun/`      — Sun Microsystems' suite (57 not-wf files)
//!
//! Run with:
//!     cargo bench -p sup-xml-bench --bench xmlts_compliance
//!
//! Pass `XMLTS_VERBOSE=1` to print every individual file/parser
//! verdict (for debugging which files we wrongly accept).
//!
//! Pass `XMLTS_FILTER=<substring>` to only run files whose path
//! contains the substring.

use std::os::raw::{c_char, c_int};
use std::path::PathBuf;

use anyxml::sax::{
    Attributes as AnyAttrs, EntityResolver as AnyEntityResolver,
    ErrorHandler as AnyErrorHandler, SAXHandler as AnySAXHandler,
    XMLReader as AnyXMLReader,
};
use anyxml::sax::error::SAXParseError as AnySAXParseError;
use quick_xml::Reader as QxReader;
use quick_xml::events::Event as QxEvent;
use sup_xml::{BytesEvent, ParseOptions, XmlBytesReader};

#[allow(non_camel_case_types)]
enum XmlDoc {}
unsafe extern "C" {
    fn xmlFreeDoc(doc: *mut XmlDoc);
}

const PARSERS: &[(&str, fn(&[u8], &std::path::Path) -> bool)] = &[
    ("sup-xml",         parser_sup_xml),
    ("sup-xml-libxml2-compat", parser_sup_xml_libxml2_compat),
    ("libxml2",          parser_libxml2),
    ("roxmltree",        parser_roxmltree),
    ("xml-rs",           parser_xml_rs),
    ("quick-xml",        parser_quick_xml),
];

fn main() {
    let verbose       = std::env::var("XMLTS_VERBOSE").is_ok();
    let filter        = std::env::var("XMLTS_FILTER").ok();
    let only_sup_xml = std::env::var("XMLTS_ONLY_SUPXML").is_ok();
    let progress      = std::env::var("XMLTS_PROGRESS").is_ok();

    let root = match std::env::var("XMLTS_ROOT") {
        Ok(p) => PathBuf::from(p),
        Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/assets/xmlts"),
    };

    // Group files by category — the second-to-last directory
    // segment (e.g. `xmltest/not-wf/sa` → `xmltest/sa`).
    let mut categories: std::collections::BTreeMap<String, Vec<PathBuf>> =
        Default::default();

    walk_not_wf(&root, &mut categories);

    if let Some(f) = &filter {
        for v in categories.values_mut() {
            v.retain(|p| p.to_string_lossy().contains(f));
        }
        categories.retain(|_, v| !v.is_empty());
    }

    let total_files: usize = categories.values().map(|v| v.len()).sum();
    println!("\nW3C XML Conformance Test Suite — not-wf runner");
    println!("Loaded {} files in {} categories from {}\n",
             total_files, categories.len(), root.display());

    // Per-parser tallies, plus per-category breakdown.
    let mut per_parser: Vec<(usize, usize)> = vec![(0, 0); PARSERS.len()]; // (rejected, total)
    let mut wrongly_accepted: Vec<Vec<(String, String)>> = vec![Vec::new(); PARSERS.len()];

    println!("{:<28}{:>10}{:>10}{:>23}{:>10}{:>11}{:>9}{:>11}",
             "category", "files", "sup-xml", "supx-ml-libxml2-compat", "libxml2", "roxmltree", "xml-rs", "quick-xml");
    println!("{:-<28}{:->10}{:->10}{:->23}{:->10}{:->11}{:->9}{:->11}",
             "", "", "", "", "", "", "", "");

    for (cat, files) in &categories {
        let mut row = vec![0usize; PARSERS.len()]; // rejected count per parser
        for path in files {
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            for (i, (name, run)) in PARSERS.iter().enumerate() {
                if only_sup_xml && i != 0 { continue; }
                if progress {
                    eprintln!("[{}] {}", name, path.display());
                }
                // Each parser runs on its own thread with a hard
                // 1-second budget — guards against hangs / infinite
                // loops in any parser without aborting the whole
                // bench.  Timed-out files count as "wrongly accepted"
                // for the tally (the parser failed to make a verdict
                // either way) so sup-xml hangs show up in the
                // wrong-accept list and we can dig into them.
                let bytes_arc = std::sync::Arc::new(bytes.clone());
                let bytes_for_thread = bytes_arc.clone();
                let path_for_thread = path.clone();
                let run_fn = *run;
                let (tx, rx) = std::sync::mpsc::channel();
                let t0 = std::time::Instant::now();
                std::thread::spawn(move || {
                    let r = run_fn(&bytes_for_thread, &path_for_thread);
                    let _ = tx.send(r);
                });
                let result = rx.recv_timeout(std::time::Duration::from_secs(1));
                let dt = t0.elapsed();
                let rejected = match result {
                    Ok(accepted) => !accepted,
                    Err(_) => {
                        eprintln!("  TIMEOUT: {} parser={} (>1s)",
                                  path.display(), name);
                        false   // count as wrongly accepted
                    }
                };
                if dt > std::time::Duration::from_millis(50) {
                    eprintln!("  SLOW: {} parser={} took {:?}",
                              path.display(), name, dt);
                }
                if rejected {
                    row[i] += 1;
                    per_parser[i].0 += 1;
                } else {
                    wrongly_accepted[i].push((cat.clone(), path.file_name().unwrap().to_string_lossy().into_owned()));
                    if verbose {
                        eprintln!("  {} accepted: {}", name, path.display());
                    }
                }
                per_parser[i].1 += 1;
            }
        }
        let total = files.len();
        println!("{:<28}{:>10}{:>10}{:>14}{:>10}{:>11}{:>9}{:>11}",
                 cat, total,
                 fmt_score(row[0], total),
                 fmt_score(row[1], total),
                 fmt_score(row[2], total),
                 fmt_score(row[3], total),
                 fmt_score(row[4], total),
                 fmt_score(row[5], total));
    }

    println!("{:-<28}{:->10}{:->10}{:->14}{:->10}{:->11}{:->9}{:->11}",
             "", "", "", "", "", "", "", "");
    println!("{:<28}{:>10}{:>10}{:>14}{:>10}{:>11}{:>9}{:>11}",
             "TOTAL", total_files,
             fmt_score(per_parser[0].0, per_parser[0].1),
             fmt_score(per_parser[1].0, per_parser[1].1),
             fmt_score(per_parser[2].0, per_parser[2].1),
             fmt_score(per_parser[3].0, per_parser[3].1),
             fmt_score(per_parser[4].0, per_parser[4].1),
             fmt_score(per_parser[5].0, per_parser[5].1));

    // Cross-tabulation: how each variant of sup-xml stacks up
    // against the libxml2 reference.  "files libxml2 wrongly
    // accepts but X rejects" = we're STRICTER than libxml2.
    // "files X wrongly accepts but libxml2 rejects" = we're LAXER.
    // The compat-mode row should ideally have zero stricter-than-
    // libxml2 entries — that's the whole point of the flag.
    let lx_idx     = PARSERS.iter().position(|(n, _)| *n == "libxml2").unwrap();
    let cmp_idx    = PARSERS.iter().position(|(n, _)| *n == "sup-xml-libxml2-compat").unwrap();
    let lx_set: std::collections::BTreeSet<(String, String)> =
        wrongly_accepted[lx_idx].iter().cloned().collect();
    for (label, idx) in [("strict sup-xml", 0), ("sup-xml-libxml2-compat", cmp_idx)] {
        let sx_set: std::collections::BTreeSet<(String, String)> =
            wrongly_accepted[idx].iter().cloned().collect();
        let lx_only: Vec<_> = lx_set.difference(&sx_set).cloned().collect();
        let sx_only: Vec<_> = sx_set.difference(&lx_set).cloned().collect();
        let both:    Vec<_> = sx_set.intersection(&lx_set).cloned().collect();
        println!("\n── libxml2 vs {label} cross-tab ──");
        println!("  files BOTH wrongly accept:  {} (compat-mode parity)", both.len());
        println!("  files libxml2 wrongly accepts but {label} rejects: {} (we're stricter)",
                 lx_only.len());
        for (cat, file) in &lx_only {
            println!("    + {}/{}", cat, file);
        }
        println!("  files {label} wrongly accepts but libxml2 rejects: {} (we're laxer)",
                 sx_only.len());
        for (cat, file) in &sx_only {
            println!("    - {}/{}", cat, file);
        }
    }

    // List every file sup-xml wrongly accepts, grouped by the
    // spec section it tests (parsed out of the suite's manifest).
    // This is the punch list for the next bug-fix pass.
    let sx_misses = &wrongly_accepted[0];
    if sx_misses.is_empty() {
        println!("\nsup-xml correctly rejected every file. ✅");
    } else {
        let manifest = load_manifest(&root);
        let mut by_section: std::collections::BTreeMap<String, Vec<(String, String)>> =
            Default::default();
        for (cat, file) in sx_misses {
            let id = derive_id(cat, file);
            let entry = manifest.get(&id);
            let section = entry.map(|e| e.section.clone()).unwrap_or_else(|| "?".into());
            by_section.entry(section).or_default().push((cat.clone(), file.clone()));
        }
        println!("\n── sup-xml wrongly accepts: {} files, grouped by XML 1.0 spec section ──",
                 sx_misses.len());
        for (section, files) in &by_section {
            println!("\n[§ {section}]  {} files", files.len());
            for (cat, file) in files {
                let id = derive_id(cat, file);
                let desc = manifest.get(&id).map(|e| e.desc.as_str()).unwrap_or("");
                println!("    {}/{:<14}  {}",
                    cat, file, desc.lines().next().unwrap_or("").trim());
            }
        }
    }
}

#[derive(Default)]
struct ManifestEntry { section: String, desc: String }

/// Parse `xmltest.xml` and any other manifest files we vendored,
/// returning a map `test_id → (section, description)`.  Best-effort
/// — used only for human-readable annotations in the failing-file
/// list, so missing entries are fine.
fn load_manifest(root: &PathBuf) -> std::collections::HashMap<String, ManifestEntry> {
    let mut out = std::collections::HashMap::new();
    for path in glob_xml(root) {
        let Ok(s) = std::fs::read_to_string(&path) else { continue };
        // Each TEST element is one line-ish with attribute soup.  Cheap
        // string scan is fine — these manifests are <50KB total.
        for cap in s.split("<TEST ").skip(1) {
            let id = pull_attr(cap, "ID=").map(str::to_string);
            let sec = pull_attr(cap, "SECTIONS=").map(str::to_string).unwrap_or_default();
            let desc = cap.split('>').nth(1)
                .and_then(|tail| tail.split("</TEST>").next())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if let Some(id) = id {
                out.insert(id, ManifestEntry { section: sec, desc });
            }
        }
    }
    out
}

fn glob_xml(root: &PathBuf) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(d: &std::path::Path, out: &mut Vec<PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(d) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.is_dir() { walk(&p, out); }
                else if p.file_name().and_then(|s| s.to_str())
                        .map(|s| s.ends_with(".xml") && (s.contains("test") || s.contains("conf")))
                        .unwrap_or(false) {
                    // Top-level manifests like xmltest.xml / xmlconf.xml
                    out.push(p);
                }
            }
        }
    }
    walk(root, &mut out);
    out
}

fn pull_attr<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let i = s.find(key)?;
    let rest = &s[i + key.len()..];
    let q = rest.chars().next()?;
    if q != '"' && q != '\'' { return None; }
    let end = rest[1..].find(q)?;
    Some(&rest[1..=end])
}

/// Reconstruct the manifest test ID from category + filename.
/// Pattern: `xmltest/sa/001.xml` → `not-wf-sa-001`.
fn derive_id(cat: &str, file: &str) -> String {
    let suite = cat.split('/').next().unwrap_or("");
    let sub   = cat.split('/').nth(1).unwrap_or("");
    let stem  = file.strip_suffix(".xml").unwrap_or(file);
    if suite == "xmltest" {
        format!("not-wf-{sub}-{stem}")
    } else if suite == "sun" {
        format!("not-wf-sun-{}", stem)
    } else {
        format!("{suite}-{sub}-{stem}")
    }
}

/// Recursively walk `root` looking for `not-wf` directories; group
/// the leaf files by the path containing them (e.g.
/// `xmltest/not-wf/sa` → `xmltest/sa`).
fn walk_not_wf(
    root: &PathBuf,
    out: &mut std::collections::BTreeMap<String, Vec<PathBuf>>,
) {
    fn walk(
        path: &std::path::Path,
        in_not_wf: Option<String>,
        out: &mut std::collections::BTreeMap<String, Vec<PathBuf>>,
    ) {
        let Ok(entries) = std::fs::read_dir(path) else { return };
        for ent in entries.flatten() {
            let p = ent.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name == "not-wf" {
                    // Enter a not-wf subtree.  Use the parent dir name
                    // (e.g. "xmltest") as the category prefix.
                    let prefix = p.parent()
                        .and_then(|q| q.file_name())
                        .and_then(|s| s.to_str())
                        .unwrap_or("?")
                        .to_string();
                    walk(&p, Some(prefix), out);
                } else if let Some(prefix) = &in_not_wf {
                    let cat = format!("{}/{}", prefix, name);
                    walk(&p, Some(cat), out);
                } else {
                    walk(&p, None, out);
                }
            } else if let Some(cat) = &in_not_wf {
                if p.extension().is_some_and(|e| e == "xml") {
                    out.entry(cat.clone()).or_default().push(p);
                }
            }
        }
    }
    walk(root, None, out);
}

fn fmt_score(rej: usize, total: usize) -> String {
    if total == 0 { "—".to_string() }
    else { format!("{}/{}", rej, total) }
}

// ── parser runners ── return `true` if accepted, `false` if rejected ─

fn parser_sup_xml(bytes: &[u8], path: &std::path::Path) -> bool {
    let opts = supxml_opts(path, /*libxml2_compat=*/ false);
    let mut r = match XmlBytesReader::from_bytes(bytes) {
        Ok(r) => r.with_options(opts),
        Err(_) => return false,    // bytes weren't UTF-8 → rejected
    };
    loop {
        match r.next() {
            Ok(BytesEvent::Eof) => return true,
            Ok(_)  => continue,
            Err(_) => return false,
        }
    }
}

/// SupXML with `libxml2_compat: true`.  Same engine as
/// `parser_sup_xml` but with the spec-strictness checks libxml2
/// doesn't enforce (external-entity reference + char-ref pre-
/// expansion in entity values) relaxed.  Should match libxml2's
/// accept/reject pattern on this bench's fixtures more closely.
fn parser_sup_xml_libxml2_compat(bytes: &[u8], path: &std::path::Path) -> bool {
    let opts = supxml_opts(path, /*libxml2_compat=*/ true);
    let mut r = match XmlBytesReader::from_bytes(bytes) {
        Ok(r) => r.with_options(opts),
        Err(_) => return false,
    };
    loop {
        match r.next() {
            Ok(BytesEvent::Eof) => return true,
            Ok(_)  => continue,
            Err(_) => return false,
        }
    }
}

/// Shared options for the two sup-xml runners: enable external-DTD
/// loading sandboxed to the test file's directory so well-formedness
/// violations *inside* external entities (the not-sa/ext-sa
/// corpora) actually surface during parse.  Without this the
/// catalog's expectation that these files be rejected can't be met
/// — the externals are never read.
fn supxml_opts(path: &std::path::Path, libxml2_compat: bool) -> ParseOptions {
    let dir = path.parent().map(std::path::PathBuf::from);
    let mut opts = ParseOptions::default();
    opts.libxml2_compat = libxml2_compat;
    opts.load_external_dtd = true;
    // James Clark's xmltest predates XML 1.0 5th edition (2008),
    // which loosened Name productions to Unicode-category rules.
    // The catalog marks character-class tests with `EDITION="1 2 3 4"`
    // to flag this — to be measured against the catalog's intent
    // we use 4th-edition rules here, same as libxml2.
    opts.xml10_fourth_edition = true;
    opts.base_url = Some(path.to_string_lossy().into_owned());
    if let Some(d) = dir {
        opts.external_resolver = Some(std::sync::Arc::new(
            sup_xml::FilesystemResolver::new(vec![d]),
        ));
    }
    opts
}

fn parser_libxml2(bytes: &[u8], path: &std::path::Path) -> bool {
    // libxml2's `xmlParseMemory` doesn't accept a URL, so external-
    // entity resolution can't see relative SYSTEM literals.  Use
    // `xmlReadMemory` with the file path as the URL so libxml2's
    // default resolver can find adjacent `.ent` / `.dtd` files —
    // matches how real callers configure it.
    unsafe extern "C" {
        fn xmlReadMemory(
            buffer: *const c_char, size: c_int,
            url: *const c_char, encoding: *const c_char, options: c_int,
        ) -> *mut XmlDoc;
    }
    let url = std::ffi::CString::new(path.to_string_lossy().as_bytes()).ok();
    let url_ptr = url.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
    // 0x08 XML_PARSE_DTDLOAD — load the external DTD so well-formedness
    // checks inside it run.  Without this libxml2 silently skips the
    // external subset on `xmlReadMemory`, masking violations our
    // fixtures depend on.
    const XML_PARSE_DTDLOAD: c_int = 0x08;
    // SAFETY: bytes borrow + libxml2 owns the resulting doc.
    unsafe {
        let doc = xmlReadMemory(
            bytes.as_ptr() as *const c_char,
            bytes.len() as c_int,
            url_ptr, std::ptr::null(), XML_PARSE_DTDLOAD,
        );
        if doc.is_null() {
            false
        } else {
            xmlFreeDoc(doc);
            true
        }
    }
}

fn parser_roxmltree(bytes: &[u8], _: &std::path::Path) -> bool {
    let Ok(s) = std::str::from_utf8(bytes) else { return false };
    let opt = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    roxmltree::Document::parse_with_options(s, opt).is_ok()
}

fn parser_xml_rs(bytes: &[u8], _: &std::path::Path) -> bool {
    use xml::reader::EventReader;
    for ev in EventReader::new(bytes) {
        if ev.is_err() { return false; }
    }
    true
}

fn parser_quick_xml(bytes: &[u8], _: &std::path::Path) -> bool {
    let mut r = QxReader::from_reader(bytes);
    loop {
        match r.read_event() {
            Ok(QxEvent::Eof) => return true,
            Ok(_)  => continue,
            Err(_) => return false,
        }
    }
}

// anyxml requires `&str` and uses a SAX handler — kept as a lone
// extra here for cross-checking, even though we don't surface it
// in COMPARISON.md (low-popularity crate).
#[allow(dead_code)]
fn parser_anyxml(bytes: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(bytes) else { return false };
    #[derive(Default)]
    struct Probe { errored: bool }
    impl AnyEntityResolver for Probe {}
    impl AnyErrorHandler for Probe {
        fn error(&mut self, _: AnySAXParseError)       { self.errored = true; }
        fn fatal_error(&mut self, _: AnySAXParseError) { self.errored = true; }
    }
    impl AnySAXHandler for Probe {
        fn start_element(&mut self, _: Option<&str>, _: Option<&str>, _: &str, _: &AnyAttrs) {}
    }
    let mut reader = AnyXMLReader::builder().set_handler(Probe::default()).build();
    let res = reader.parse_str(s, None);
    !reader.handler.errored && res.is_ok()
}
