//! EXSLT throughput head-to-head: sup-xml vs xsltproc (libxslt+libexslt).
//!
//! Runs a tokenize-heavy and a regex-match-heavy XSLT transform at
//! several input sizes.  sup-xml times only the `apply` step (the
//! stylesheet is compiled once, the source is parsed once).
//! xsltproc is invoked as a subprocess; each iteration pays process
//! startup, so we run a single large batch and divide.  Smaller per-
//! op time = faster.
//!
//! Why xsltproc rather than FFI: the libxslt C ABI is sensitive to
//! header / runtime version mismatch on macOS, where the system
//! /usr/lib copy diverges from Homebrew's keg-only install.  The
//! subprocess path uses xsltproc straight from its own install, so
//! versions match by construction.
//!
//! The xsltproc column carries the subprocess-startup tax (a few ms
//! per invocation), so it's pessimistic for small workloads.  We
//! call out per-row when the tax is a meaningful fraction of total.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Instant;

// ── workloads ─────────────────────────────────────────────────────

fn tokenize_stylesheet() -> &'static str {
    r#"<?xml version="1.0"?>
<xsl:stylesheet version="1.0"
                xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                xmlns:str="http://exslt.org/strings"
                extension-element-prefixes="str">
  <xsl:output method="xml" omit-xml-declaration="yes"/>
  <xsl:template match="/r">
    <out>
      <xsl:for-each select="str:tokenize(s, ',')">
        <t><xsl:value-of select="."/></t>
      </xsl:for-each>
    </out>
  </xsl:template>
</xsl:stylesheet>"#
}

fn match_stylesheet() -> &'static str {
    r#"<?xml version="1.0"?>
<xsl:stylesheet version="1.0"
                xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                xmlns:regexp="http://exslt.org/regular-expressions"
                extension-element-prefixes="regexp">
  <xsl:output method="xml" omit-xml-declaration="yes"/>
  <xsl:template match="/r">
    <out>
      <xsl:for-each select="regexp:match(s, '[a-z]+[0-9]+', 'g')">
        <m><xsl:value-of select="."/></m>
      </xsl:for-each>
    </out>
  </xsl:template>
</xsl:stylesheet>"#
}

fn build_source(payload: &str) -> String {
    format!("<r><s>{}</s></r>", payload)
}

fn tokenize_payload(n: usize) -> String {
    (0..n).map(|i| format!("tok{i}")).collect::<Vec<_>>().join(",")
}

fn match_payload(n: usize) -> String {
    (0..n).map(|i| format!("word{i}")).collect::<Vec<_>>().join(" ")
}

fn payload_count(label: &str, payload: &str) -> usize {
    match label {
        "tokenize" => payload.split(',').count(),
        "match"    => payload.split_whitespace().count(),
        _          => 0,
    }
}

// ── runners ───────────────────────────────────────────────────────

fn supxml_loop(stylesheet_xml: &str, source_xml: &str, iters: u32) -> f64 {
    use sup_xml::xslt::Stylesheet;
    use sup_xml::{parse_str, ParseOptions};
    let style = Stylesheet::compile_str(stylesheet_xml).expect("compile stylesheet");
    let opts  = ParseOptions { namespace_aware: true, ..ParseOptions::default() };
    let src   = parse_str(source_xml, &opts).expect("parse source");
    let _ = style.apply(&src).expect("warmup apply").to_string().unwrap();
    let t0 = Instant::now();
    for _ in 0..iters {
        let r = style.apply(&src).expect("apply");
        let s = r.to_string().expect("serialise");
        std::hint::black_box(s);
    }
    t0.elapsed().as_secs_f64()
}

/// One xsltproc invocation per iteration.  Pays process startup
/// (~3–6 ms on macOS) each call, so this is pessimistic for small
/// workloads — useful as a rough ceiling, not a precise op cost.
fn xsltproc_loop(
    xsltproc: &str, stylesheet_xml: &str, source_xml: &str, iters: u32,
) -> Option<f64> {
    // Write stylesheet to a temp file once — xsltproc takes file paths.
    let dir = std::env::temp_dir();
    let sty_path = dir.join("supxml_bench_style.xsl");
    let src_path = dir.join("supxml_bench_src.xml");
    std::fs::write(&sty_path, stylesheet_xml).ok()?;
    std::fs::write(&src_path, source_xml).ok()?;

    // Warm-up — discard.
    let _ = Command::new(xsltproc)
        .arg(&sty_path).arg(&src_path)
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status().ok()?;

    let t0 = Instant::now();
    for _ in 0..iters {
        let out = Command::new(xsltproc)
            .arg(&sty_path).arg(&src_path)
            .stdout(Stdio::piped()).stderr(Stdio::null())
            .output().ok()?;
        std::hint::black_box(out.stdout);
    }
    Some(t0.elapsed().as_secs_f64())
}

/// Measure xsltproc subprocess-startup overhead by running an
/// identity stylesheet on an empty document — the apply step is
/// O(1), so what remains is process spawn + dynamic linker work.
/// Subtract this from the timed measurements to estimate
/// xsltproc's *actual* transform cost.
fn xsltproc_startup_us(xsltproc: &str) -> Option<f64> {
    let dir = std::env::temp_dir();
    let sty_path = dir.join("supxml_bench_startup.xsl");
    let src_path = dir.join("supxml_bench_startup.xml");
    std::fs::write(&sty_path, r#"<?xml version="1.0"?>
<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
  <xsl:template match="/"/>
</xsl:stylesheet>"#).ok()?;
    std::fs::write(&src_path, "<r/>").ok()?;
    // Warm-up.
    let _ = Command::new(xsltproc)
        .arg(&sty_path).arg(&src_path)
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status().ok()?;
    let iters = 20u32;
    let t0 = Instant::now();
    for _ in 0..iters {
        let _ = Command::new(xsltproc)
            .arg(&sty_path).arg(&src_path)
            .stdout(Stdio::null()).stderr(Stdio::null())
            .status().ok()?;
    }
    Some((t0.elapsed().as_secs_f64() / iters as f64) * 1e6)
}

fn xsltproc_path() -> Option<String> {
    // Prefer the Homebrew install when present — it ships newer
    // libxslt / libexslt and has the correct EXSLT registration.
    for p in ["/opt/homebrew/opt/libxslt/bin/xsltproc",
              "/usr/local/opt/libxslt/bin/xsltproc",
              "/usr/bin/xsltproc"] {
        if std::path::Path::new(p).exists() {
            return Some(p.to_string());
        }
    }
    None
}

// ── runner ────────────────────────────────────────────────────────

struct Row {
    label:        &'static str,
    size:         usize,
    iters_sx:     u32,
    supxml_us:    f64,
    xsltproc_us:  Option<f64>,
}

fn run_size(
    label: &'static str, stylesheet: &str, payload: String,
    iters_sx: u32, iters_xp: u32, xsltproc: Option<&str>,
) -> Row {
    let source = build_source(&payload);
    let size   = payload_count(label, &payload);
    let supxml = supxml_loop(stylesheet, &source, iters_sx);
    let xsltproc_dur = xsltproc.and_then(|p| xsltproc_loop(p, stylesheet, &source, iters_xp));
    Row {
        label, size, iters_sx,
        supxml_us:   (supxml / iters_sx as f64) * 1e6,
        xsltproc_us: xsltproc_dur.map(|d| (d / iters_xp as f64) * 1e6),
    }
}

fn print_table(rows: &[Row], xsltproc: Option<&str>, startup_us: Option<f64>) {
    let title = "EXSLT throughput: sup-xml XSLT engine vs xsltproc";
    println!("\n{title}");
    println!("{}", "=".repeat(title.len()));
    println!();
    println!("Each row applies a tokenize-heavy or regex-match-heavy");
    println!("stylesheet to a single source document.  sup-xml times the");
    println!("`apply` step only (stylesheet + source pre-built).  xsltproc");
    println!("is invoked once per iteration as a subprocess; the `(-spawn)`");
    println!("column subtracts measured spawn overhead from xsltproc's");
    println!("total, isolating the actual transform cost.");
    if let Some(p) = xsltproc {
        println!("xsltproc: {p}");
        if let Some(us) = startup_us {
            println!("xsltproc spawn overhead (measured): {us:.0} μs/call");
        }
    } else {
        println!("xsltproc not found — sup-xml-only run.");
    }
    println!();
    let header = format!("{:<10} {:>8} {:>8} {:>13} {:>13} {:>14} {:>15}",
                         "workload", "size", "iters", "sup-xml μs", "xsltproc μs",
                         "xp-(spawn) μs", "ratio (work)");
    println!("{header}");
    println!("{}", "-".repeat(header.len()));
    for r in rows {
        let xp_raw = r.xsltproc_us.map(|us| format!("{us:>13.1}")).unwrap_or_else(|| "n/a".into());
        // Subtract the spawn-cost baseline.  When the result is small
        // relative to the noise floor (< 5% of spawn), the timing is
        // dominated by spawn and the ratio is meaningless — show a
        // dash and flag the row.
        let xp_work = match (r.xsltproc_us, startup_us) {
            (Some(xp), Some(s)) => Some(xp - s),
            _ => None,
        };
        // Noise floor: spawn timing varies ~10% sample-to-sample, so
        // anything within 10% of the spawn cost (positive or negative
        // after subtraction) is indistinguishable from the spawn
        // baseline.  Mark these rows "≈ spawn-bound" — the workload
        // is too small for libxslt's actual transform cost to surface.
        let noise_floor = startup_us.unwrap_or(0.0) * 0.10;
        let xp_work_s = xp_work.map(|w| {
            if w <= noise_floor { "≈ spawn-bound".into() }
            else                { format!("{w:>13.1}") }
        }).unwrap_or_else(|| "n/a".into());
        let ratio = match xp_work {
            Some(w) if w <= noise_floor    => "—".into(),
            Some(w) if w > r.supxml_us     => format!("{:.2}× faster", w / r.supxml_us),
            Some(w) if w < r.supxml_us     => format!("{:.2}× slower", r.supxml_us / w),
            Some(_)                        => "≈ same".into(),
            None                           => "—".into(),
        };
        println!("{:<10} {:>8} {:>8} {:>13.1} {:>13} {:>14} {:>15}",
                 r.label, r.size, r.iters_sx, r.supxml_us, xp_raw, xp_work_s, ratio);
    }
    let _ = std::io::stdout().flush();
    println!();
}

fn main() {
    let xsltproc = xsltproc_path();
    let xp = xsltproc.as_deref();
    let startup_us = xp.and_then(xsltproc_startup_us);
    if let Some(us) = startup_us {
        eprintln!("xsltproc spawn baseline: {us:.0} μs/call");
    }

    let mut rows: Vec<Row> = Vec::new();
    let sty_tok = tokenize_stylesheet();
    // (sup-xml iters, xsltproc iters).  xsltproc loops less because
    // each call costs a process spawn.
    rows.push(run_size("tokenize", sty_tok, tokenize_payload(10),     5000, 50, xp));
    rows.push(run_size("tokenize", sty_tok, tokenize_payload(100),    2000, 50, xp));
    rows.push(run_size("tokenize", sty_tok, tokenize_payload(1000),    400, 50, xp));
    rows.push(run_size("tokenize", sty_tok, tokenize_payload(10000),    40, 20, xp));

    let sty_match = match_stylesheet();
    rows.push(run_size("match",    sty_match, match_payload(10),     5000, 50, xp));
    rows.push(run_size("match",    sty_match, match_payload(100),    2000, 50, xp));
    rows.push(run_size("match",    sty_match, match_payload(1000),    400, 50, xp));

    print_table(&rows, xp, startup_us);
}
