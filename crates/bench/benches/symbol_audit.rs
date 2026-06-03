//! Audit `crates/compat/src/symbols.txt` against the symbols
//! actually exported by the system libxml2.dylib **and** against the
//! symbols downstream consumers (`libxslt`, `libexslt`, `xmllint`)
//! actually link against.
//!
//! Drop-in compat means every `dlsym(handle, "xmlFoo")` a consumer
//! issues against `libxml2` should also resolve against our cdylib.
//! But libxml2 exports ~1400 symbols, many of which are deprecated,
//! internal helpers that leaked into the public ABI, or function-
//! pointer macros that nothing in the real world actually calls.
//!
//! So a raw "X% of libxml2's exports covered" number is misleading.
//! The number that matters is "X% of what downstream code actually
//! demands," and the canonical downstream is the libxslt / libexslt /
//! xmllint trio — the things you'd `LD_PRELOAD libsupxml2.so` against.
//!
//! Inputs:
//!   - the *manifest* of symbols we intend to export, vendored as
//!     `crates/compat/src/symbols.txt`,
//!   - the *actual* exports of the system `libxml2.dylib`,
//!   - the *actual* imports of `libxslt.dylib`, `libexslt.dylib`,
//!     and the `xmllint` CLI binary.
//!
//! Prints two coverage numbers — gross (vs all libxml2 exports) and
//! demand-weighted (vs what downstream actually uses) — plus a list
//! of the demand-side gaps grouped by subsystem and consumer.
//!
//! Does **not** `nm` our own cdylib — that would require building
//! `sup-xml-compat` with `--features cdylib-exports` as part of the
//! bench and live-loading the resulting `libsupxml2.dylib`.  Useful
//! follow-up (catches manifest-vs-build drift), but the
//! manifest-vs-libxml2 gap measured here is the bigger lever.
//!
//! Run:
//!     cargo bench -p sup-xml-bench --bench symbol_audit

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

/// Paths to system libxml2 and the canonical downstream consumers.
/// Homebrew-specific; if you're on a different host, point these at
/// your distro's copies.
const LIBXML2_DYLIB:  &str = "/opt/homebrew/Cellar/libxml2/2.15.3/lib/libxml2.dylib";
const LIBXSLT_DYLIB:  &str = "/opt/homebrew/Cellar/libxslt/1.1.45/lib/libxslt.1.dylib";
const LIBEXSLT_DYLIB: &str = "/opt/homebrew/Cellar/libxslt/1.1.45/lib/libexslt.0.dylib";
const XMLLINT_BIN:    &str = "/opt/homebrew/Cellar/libxml2/2.15.3/bin/xmllint";

/// Compat manifest of symbols we intend to export.  Embedded at
/// compile time so the bench is self-contained and doesn't drift
/// when the manifest is rewritten in another working tree.
const COMPAT_MANIFEST: &str = include_str!("../../compat/src/symbols.txt");

/// Run `nm -gU` against a Mach-O image and return the set of
/// exported symbol names.  Mach-O `nm` prefixes external symbols
/// with `_`; we keep the prefix so the comparison is apples-to-apples
/// with `crates/compat/src/symbols.txt` (which also has it).
fn exported_symbols(path: &str) -> BTreeSet<String> {
    let out = Command::new("nm")
        .args(["-gU", path])
        .output()
        .expect("`nm` should be on PATH — required for the symbol audit");
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout.lines().filter_map(|line| {
        let sym = line.split_whitespace().last()?;
        let after_underscore = sym.strip_prefix('_')?;
        let first = after_underscore.chars().next()?;
        if first.is_ascii_alphabetic() {
            Some(sym.to_string())
        } else {
            None
        }
    }).collect()
}

/// Run `nm -u` against a Mach-O image and return the set of
/// *undefined* symbol names — i.e. the symbols this image expects
/// the dynamic linker to resolve from somewhere else.  Intersecting
/// the undefined set of a downstream library with libxml2's exports
/// tells us exactly which libxml2 entry points that downstream
/// actually consumes.
fn imported_symbols(path: &str) -> BTreeSet<String> {
    let out = Command::new("nm")
        .args(["-u", path])
        .output()
        .expect("`nm` should be on PATH — required for the symbol audit");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // `nm -u` on Mach-O prints one bare symbol per line.
    stdout.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.starts_with('_'))
        .map(String::from)
        .collect()
}

/// Parse the compat manifest into a set, ignoring blank lines and
/// `#`-prefixed comments.
fn compat_manifest() -> BTreeSet<String> {
    COMPAT_MANIFEST.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// Bucket a symbol name into a coarse subsystem by its libxml2-style
/// prefix.  The buckets aren't authoritative — libxml2 doesn't enforce
/// them in code — but they map well to the docs/manpages and let the
/// gap report read at a glance.
fn bucket(sym: &str) -> &'static str {
    // Strip the leading `_` Mach-O adds to external symbols.
    let name = sym.trim_start_matches('_');
    // Some symbols use a `__` prefix for vars (xmlGenericError etc.).
    let name = name.trim_start_matches('_');
    match name {
        n if n.starts_with("xmlSchema")  || n.starts_with("xmlSchemas")  => "xsd",
        n if n.starts_with("xmlRelaxNG")                                  => "relaxng",
        n if n.starts_with("xmlSchematron")                               => "schematron",
        n if n.starts_with("xmlXPath")   || n.starts_with("xmlXPtr")      => "xpath",
        n if n.starts_with("xmlXInclude")                                 => "xinclude",
        n if n.starts_with("xmlReader")  || n.starts_with("xmlTextReader") => "reader",
        n if n.starts_with("xmlWriter")  || n.starts_with("xmlTextWriter") => "writer",
        n if n.starts_with("xmlSAX")                                       => "sax",
        n if n.starts_with("xmlC14N")    || n.starts_with("xmlExc")        => "c14n",
        n if n.starts_with("xmlCatalog") || n.starts_with("xmlACatalog")   => "catalog",
        n if n.starts_with("xmlURI")                                       => "uri",
        n if n.starts_with("xmlRegexp")  || n.starts_with("xmlExp")        => "regexp",
        n if n.starts_with("xmlAutomata")                                  => "automata",
        n if n.starts_with("xmlBuffer")  || n.starts_with("xmlBuf")        => "buffer",
        n if n.starts_with("xmlIO")      || n.starts_with("xmlInputBuffer")
                                         || n.starts_with("xmlOutputBuffer") => "io",
        n if n.starts_with("xmlMem")     || n.starts_with("xmlMalloc")
                                         || n.starts_with("xmlRealloc")
                                         || n.starts_with("xmlFree")
                                         || n.starts_with("xmlStrdup")     => "alloc",
        n if n.starts_with("xmlChar")    || n.starts_with("xmlStr")
                                         || n.starts_with("xmlCheckUTF8")  => "string",
        n if n.starts_with("xmlHash")                                       => "hash",
        n if n.starts_with("xmlDict")                                       => "dict",
        n if n.starts_with("xmlList")                                       => "list",
        n if n.starts_with("xmlRMutex")  || n.starts_with("xmlMutex")
                                         || n.starts_with("xmlThr")        => "thread",
        n if n.starts_with("xmlDoc")     || n.starts_with("xmlNode")
                                         || n.starts_with("xmlNew")
                                         || n.starts_with("xmlFree")
                                         || n.starts_with("xmlCopy")
                                         || n.starts_with("xmlReplace")
                                         || n.starts_with("xmlAdd")
                                         || n.starts_with("xmlUnlinkNode")
                                         || n.starts_with("xmlGetProp")    => "tree",
        n if n.starts_with("xmlAttr")                                       => "tree",
        n if n.starts_with("xmlNs")                                         => "namespace",
        n if n.starts_with("xmlValid")   || n.starts_with("xmlIsID")
                                         || n.starts_with("xmlGetID")
                                         || n.starts_with("xmlGetRefs")    => "validation",
        n if n.starts_with("xmlError")   || n.starts_with("xmlGetLastError")
                                         || n.starts_with("xmlSetGeneric")
                                         || n.starts_with("xmlSetStructured")
                                         || n.starts_with("xmlReset")
                                         || n.starts_with("xmlParserErr")
                                         || n.starts_with("xmlParserWarning")
                                         || n.starts_with("xmlParserValidity") => "error",
        n if n.starts_with("xmlEntity")  || n.starts_with("xmlGetParameter")
                                         || n.starts_with("xmlGetPredefined")
                                         || n.starts_with("xmlAddDocEntity") => "entity",
        n if n.starts_with("xmlEncoding") || n.starts_with("xmlGetCharEncoding")
                                          || n.starts_with("xmlDetectCharEncoding")
                                          || n.starts_with("xmlAddEncodingAlias")
                                          || n.starts_with("xmlDelEncodingAlias")
                                          || n.starts_with("xmlCleanupCharEncodingHandlers")
                                          || n.starts_with("xmlCleanupEncodingAliases") => "encoding",
        n if n.starts_with("html")      || n.starts_with("htmlAttr")       => "html",
        n if n.starts_with("xmlParse")  || n.starts_with("xmlRead")
                                        || n.starts_with("xmlCtxt")
                                        || n.starts_with("xmlNew")
                                        || n.starts_with("xmlInitParser")
                                        || n.starts_with("xmlCleanupParser") => "parse",
        n if n.starts_with("exslt")                                         => "exslt",
        _ => "misc",
    }
}

fn main() {
    let exports  = exported_symbols(LIBXML2_DYLIB);
    let manifest = compat_manifest();

    let missing: BTreeSet<&String> = exports.difference(&manifest).collect();
    let extra:   BTreeSet<&String> = manifest.difference(&exports).collect();
    let common:  BTreeSet<&String> = exports.intersection(&manifest).collect();

    // ── real-world demand intersection ────────────────────────────────────
    //
    // What does the canonical downstream actually pull from libxml2?
    // For each consumer, intersect its undefined-symbol set with
    // libxml2's exports — that's the libxml2 surface it cares about.
    let xslt_demand  = &imported_symbols(LIBXSLT_DYLIB)  & &exports;
    let exslt_demand = &imported_symbols(LIBEXSLT_DYLIB) & &exports;
    let xmllint_demand = &imported_symbols(XMLLINT_BIN)  & &exports;
    let union_demand: BTreeSet<String> =
        xslt_demand.union(&exslt_demand)
                   .cloned()
                   .collect::<BTreeSet<_>>()
                   .union(&xmllint_demand)
                   .cloned()
                   .collect();
    let demand_covered: BTreeSet<_> = union_demand.intersection(&manifest).collect();
    let demand_missing: BTreeSet<_> = union_demand.difference(&manifest).collect();

    println!();
    println!("Symbol export audit: compat manifest vs libxml2 + downstream demand");
    println!("===================================================================");
    println!();
    println!("libxml2 dylib       : {LIBXML2_DYLIB}");
    println!("libxslt dylib       : {LIBXSLT_DYLIB}");
    println!("libexslt dylib      : {LIBEXSLT_DYLIB}");
    println!("xmllint binary      : {XMLLINT_BIN}");
    println!("compat manifest     : crates/compat/src/symbols.txt");
    println!();
    println!("Gross coverage (vs all libxml2 exports — most are unused dead weight):");
    println!("  libxml2 exports     : {:>5}", exports.len());
    println!("  compat manifest     : {:>5}", manifest.len());
    println!("  overlap             : {:>5}  ({:.0}% of libxml2)",
             common.len(),
             100.0 * common.len() as f64 / exports.len().max(1) as f64);
    println!("  missing from compat : {:>5}  (mostly deprecated/internal — see demand below)", missing.len());
    println!("  only in compat      : {:>5}  (extensions / sup-xml-specific)", extra.len());
    println!();
    println!("Demand-weighted coverage (vs symbols actually used by downstream):");
    println!("  libxslt   demands   : {:>5}  ({:>3}% covered)",
             xslt_demand.len(),
             100 * (&xslt_demand & &manifest).len() / xslt_demand.len().max(1));
    println!("  libexslt  demands   : {:>5}  ({:>3}% covered)",
             exslt_demand.len(),
             100 * (&exslt_demand & &manifest).len() / exslt_demand.len().max(1));
    println!("  xmllint   demands   : {:>5}  ({:>3}% covered)",
             xmllint_demand.len(),
             100 * (&xmllint_demand & &manifest).len() / xmllint_demand.len().max(1));
    println!("  union     demands   : {:>5}  ({:>3}% covered)  ← the number that matters",
             union_demand.len(),
             100 * demand_covered.len() / union_demand.len().max(1));
    println!();

    // ── per-bucket coverage table ──────────────────────────────────────────
    //
    // For each subsystem prefix we recognise, how many of libxml2's symbols
    // do we cover and how many are missing?  Surfaces the gap pattern at a
    // glance — usually a whole subsystem is missing (e.g. all of xmlReader)
    // rather than a scattered handful.
    let mut by_bucket: BTreeMap<&'static str, (u32, u32)> = BTreeMap::new();
    for sym in &exports {
        let b = bucket(sym);
        let entry = by_bucket.entry(b).or_insert((0, 0));
        entry.0 += 1;
        if manifest.contains(sym) {
            entry.1 += 1;
        }
    }

    println!("Coverage by subsystem");
    println!("---------------------");
    println!("  {:<14}  {:>10}  {:>10}  {}", "bucket", "libxml2", "covered", "%");
    println!("  {:<14}  {:>10}  {:>10}  {}", "------", "-------", "-------", "-");
    for (b, (total, covered)) in &by_bucket {
        let pct = if *total == 0 { 0 } else { 100 * covered / total };
        println!("  {:<14}  {:>10}  {:>10}  {pct}%", b, total, covered);
    }
    println!();

    // ── top missing symbols per bucket ────────────────────────────────────
    //
    // Print up to 5 missing names per bucket — enough to characterise the
    // gap without dumping the full diff to stdout.  The full diff is
    // exports.difference(manifest); rerun with stdout redirection for
    // the complete list if needed.
    let mut missing_by_bucket: BTreeMap<&'static str, Vec<&String>> = BTreeMap::new();
    for sym in &missing {
        missing_by_bucket.entry(bucket(sym)).or_default().push(*sym);
    }
    println!("Top missing symbols (≤5 per bucket)");
    println!("-----------------------------------");
    for (b, syms) in &missing_by_bucket {
        if syms.is_empty() { continue; }
        let sample: Vec<&str> = syms.iter().take(5).map(|s| s.as_str()).collect();
        println!("  {:<14}  ({:>3} missing) {}", b, syms.len(), sample.join(", "));
    }
    println!();

    if !extra.is_empty() {
        println!("Symbols in our manifest but NOT in upstream libxml2:");
        for s in extra.iter().take(20) {
            println!("  {s}");
        }
        if extra.len() > 20 {
            println!("  ... and {} more", extra.len() - 20);
        }
        println!();
        println!("These are either sup-xml extensions or stale entries; audit the");
        println!("list against `crates/compat/src/symbols.txt` to decide.");
        println!();
    }

    // ── demand-side gaps (the real prioritisation list) ──────────────────
    if !demand_missing.is_empty() {
        println!("Demand-side gaps — libxml2 symbols missing from our manifest");
        println!("that at least one downstream consumer actually links against:");
        println!();
        // Group by subsystem, annotate with which consumer(s) want it.
        let mut by_bucket: BTreeMap<&'static str, Vec<(&String, Vec<&'static str>)>> = BTreeMap::new();
        for sym in &demand_missing {
            let mut who = Vec::new();
            if xslt_demand.contains(*sym)    { who.push("xslt"); }
            if exslt_demand.contains(*sym)   { who.push("exslt"); }
            if xmllint_demand.contains(*sym) { who.push("xmllint"); }
            by_bucket.entry(bucket(sym)).or_default().push((*sym, who));
        }
        for (b, syms) in &by_bucket {
            println!("  [{b}]");
            for (s, who) in syms {
                println!("    {s:<42}  ({})", who.join(","));
            }
        }
        println!();
        println!("Each of these is a `dlopen(libsupxml2)` user whose specific");
        println!("downstream call returns NULL.  Wiring an existing libxml2 symbol");
        println!("is usually a per-function shim in `crates/compat/src/<subsystem>.rs`");
        println!("plus an entry in `symbols.txt`.");
    }
}
