//! Serialization benchmarks: sup-xml's serializer vs libxml2's `xmlDocDumpMemory`.
//!
//! Run with:
//!   cargo bench -p sup-xml-bench --bench serialize
//!
//! HTML reports land in target/criterion/.
//!
//! The serializer's cost is dominated by the text / attribute escaping path
//! (`XmlBuf::push_escaped_text` / `push_escaped_attr`).  The fixtures below
//! isolate the cases that path handles differently:
//!
//! - **clean**   — pure ASCII, no characters need escaping (best case: the
//!                 whole run is one bulk copy).
//! - **prose**   — realistic document text with occasional `&` / `<`.
//! - **cjk**     — multibyte UTF-8 (CJK + emoji) with no escapes; measures
//!                 the per-character UTF-8 decode the old char-loop paid even
//!                 when nothing needed escaping.
//! - **attrs**   — attribute-heavy markup (escaping runs through the attr path).
//! - **escapes** — content that is mostly `<` / `>` / `&` (worst case: a
//!                 replacement on nearly every byte).

use std::os::raw::{c_char, c_int, c_void};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

// ── libxml2 FFI ───────────────────────────────────────────────────────────────
// Linked by build.rs (via pkg-config).  We parse with libxml2 and dump with it
// so the comparison is libxml2-tree → libxml2-serializer, the true baseline for
// "is our serializer competitive with the C reference."

unsafe extern "C" {
    fn xmlParseMemory(buffer: *const c_char, size: c_int) -> *mut c_void;
    fn xmlFreeDoc(cur: *mut c_void);
    fn xmlDocDumpMemory(cur: *mut c_void, mem: *mut *mut c_char, size: *mut c_int);
}

// ── fixture generation ────────────────────────────────────────────────────────

/// Pure-ASCII document with no characters that need escaping — the bulk-copy
/// best case.
fn generate_clean(n: usize) -> String {
    let mut xml = String::with_capacity(n * 120);
    xml.push_str("<catalog>");
    for i in 0..n {
        xml.push_str("<item id=\"");
        xml.push_str(&i.to_string());
        xml.push_str("\"><name>Plain ASCII item name number ");
        xml.push_str(&i.to_string());
        xml.push_str("</name><note>No markup characters appear in this run at all.</note></item>");
    }
    xml.push_str("</catalog>");
    xml
}

/// Realistic prose with occasional escapable characters in text content.
fn generate_prose(n: usize) -> String {
    let mut xml = String::with_capacity(n * 160);
    xml.push_str("<doc>");
    for i in 0..n {
        xml.push_str("<p>Section ");
        xml.push_str(&i.to_string());
        xml.push_str(": research &amp; development of x &lt; y systems, see &lt;ref&gt; for the full \
                      treatment of the topic and its many practical implications.</p>");
    }
    xml.push_str("</doc>");
    xml
}

/// Multibyte UTF-8 (CJK + emoji) text with no escapes.  Isolates the
/// per-character decode the char-loop paid on content that copies verbatim.
fn generate_cjk(n: usize) -> String {
    let mut xml = String::with_capacity(n * 200);
    xml.push_str("<doc>");
    for _ in 0..n {
        xml.push_str("<p>日本語のテキストです。これは多バイト文字のシリアライズ性能を測定します 🦀🚀。\
                      漢字とかなと絵文字が混在する現実的な内容のサンプルになっています。</p>");
    }
    xml.push_str("</doc>");
    xml
}

/// Attribute-heavy markup — escaping runs through the attribute-value path,
/// including the `"` and tab/newline char-refs.
fn generate_attrs(n: usize) -> String {
    let mut xml = String::with_capacity(n * 200);
    xml.push_str("<root>");
    for i in 0..n {
        xml.push_str("<rec a=\"value &amp; more\" b=\"x &lt; y &gt; z\" c=\"quote &quot;here&quot;\" \
                      d=\"plain attribute value with several words\" e=\"");
        xml.push_str(&i.to_string());
        xml.push_str("\"/>");
    }
    xml.push_str("</root>");
    xml
}

/// Content that is mostly escapable characters — the worst case where a
/// replacement fires on nearly every byte.
fn generate_escapes(n: usize) -> String {
    let mut xml = String::with_capacity(n * 120);
    xml.push_str("<doc>");
    for _ in 0..n {
        // Decodes to a run of '<', '>', '&' with little clean content between.
        xml.push_str("<p>&lt;&gt;&amp;&lt;&gt;&amp;&lt;&gt;&amp;&lt;&gt;&amp;&lt;&gt;&amp;&lt;&gt;&amp;</p>");
    }
    xml.push_str("</doc>");
    xml
}

// ── benchmark drivers ─────────────────────────────────────────────────────────

fn bench_libxml2_serialize(c_doc: *mut c_void) {
    let mut mem: *mut c_char = std::ptr::null_mut();
    let mut size: c_int = 0;
    unsafe {
        xmlDocDumpMemory(c_doc, &mut mem, &mut size);
        criterion::black_box(size);
        libc::free(mem as *mut c_void);
    }
}

fn bench_serialize(c: &mut Criterion) {
    let fixtures: &[(&str, fn(usize) -> String)] = &[
        ("clean", generate_clean),
        ("prose", generate_prose),
        ("cjk", generate_cjk),
        ("attrs", generate_attrs),
        ("escapes", generate_escapes),
    ];

    // Per-fixture element count — kept modest so each generated document lands
    // in the same order of magnitude of serialized bytes.
    const N: usize = 2_000;

    for (label, make) in fixtures {
        let xml = make(N);

        let mut group = c.benchmark_group(format!("serialize/{label}"));
        group.throughput(Throughput::Bytes(xml.len() as u64));

        // `Document` is self-contained (owns its arena, borrows nothing from
        // `xml`), so capturing it in the closure keeps the benchmark from
        // having to name the tree crate's type.
        let opts = sup_xml::ParseOptions::default();
        let doc = sup_xml_core::parse_str(&xml, &opts).expect("sup-xml parse failed");
        group.bench_function(BenchmarkId::new("sup-xml", label), |b| {
            b.iter(|| criterion::black_box(sup_xml_core::serialize_to_string(&doc)))
        });

        // libxml2: parse once, dump in the loop, free at the end.
        let c_doc = unsafe { xmlParseMemory(xml.as_ptr() as *const c_char, xml.len() as c_int) };
        if !c_doc.is_null() {
            group.bench_with_input(BenchmarkId::new("libxml2", label), &c_doc, |b, &c_doc| {
                b.iter(|| bench_libxml2_serialize(c_doc))
            });
            unsafe { xmlFreeDoc(c_doc) };
        }

        group.finish();
    }
}

criterion_group!(benches, bench_serialize);
criterion_main!(benches);
