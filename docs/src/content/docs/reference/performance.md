---
title: Performance
description: Parse throughput, XPath eval, XSD validation, HTML — head-to-head numbers against libxml2, quick-xml, roxmltree, xml-rs.
---

All numbers below come from `cargo bench -p sup-xml-bench` on the
checked-in fixture set. Each table is reproducible with the command
shown above it.

## Parse — DOM (matched contract vs libxml2)

Both columns use SupXML's bumpalo-backed arena DOM (`crates/tree/src/arena.rs`)
and libxml2's `xmlParseMemory`. Both validate UTF-8, validate XML 1.0
§ 2.2 characters, expand general entities, enforce end-tag matching, and
build an owned tree — so the comparison is apples-to-apples.
Throughput, MB/s; ratio = `sup-xml / libxml2`, so >1 means SupXML faster:

| fixture | size | sup-xml | libxml2 | ratio |
|---|---:|---:|---:|---:|
| 321gone | 23 KB | 438 MB/s | 211 MB/s | **2.08×** |
| 1831893 | 15 KB | 570 MB/s | 172 MB/s | **3.32×** |
| chinese1 | 7.9 MB | 346 MB/s | 162 MB/s | **2.13×** |
| customer1 | 503 KB | 370 MB/s | 204 MB/s | **1.82×** |
| ebay | 34 KB | 910 MB/s | 491 MB/s | **1.86×** |
| gazali_maqasid_ar | 599 KB | 539 MB/s | 144 MB/s | **3.75×** |
| nasa | 24 MB | 362 MB/s | 161 MB/s | **2.25×** |
| pubmed | 600 KB | 349 MB/s | 217 MB/s | **1.61×** |
| sitemap | 1.0 MB | 396 MB/s | 196 MB/s | **2.02×** |
| swiss_prot | 95 MB | 177 MB/s | 94 MB/s | **1.88×** |
| wikipedia_ww2 | 252 KB | 1170 MB/s | 395 MB/s | **2.96×** |

SupXML is faster on every fixture; the median ratio across the full
21-fixture set is **~2.1×**, with the spread driven by attribute density
(entity-heavy `gazali_maqasid_ar` is the high end) and validation
intensity (large mostly-ASCII `swiss_prot` is the low end). Reproduce
with:

```bash
cargo bench -p sup-xml-bench --bench head_to_head
```

## Parse — SAX / streaming

The `XmlBytesReader` zero-copy streaming reader, byte events, matched
against quick-xml's `Reader` at quick-xml's own lighter contract (no
end-tag matching, no UTF-8 validation, raw byte slices):

| fixture | sup-xml (bytes) | quick-xml (raw) | ratio |
|---|---:|---:|---:|
| customer1 | 1240 MB/s | 1188 MB/s | **1.04×** |
| ebay | 3094 MB/s | 3160 MB/s | 0.98× |
| nasa | 822 MB/s | 1107 MB/s | 0.74× |
| swiss_prot | 416 MB/s | 705 MB/s | 0.59× |
| wikipedia_ww2 | 3650 MB/s | 32320 MB/s | 0.11× |

Median across all 21 fixtures is **~1.04×** in SupXML's favour at the
matched contract. The wikipedia_ww2 entry is an outlier: the fixture
fits in L1, both parsers ship a fast path that defeats memory bandwidth
estimation, and quick-xml's number there is unrepresentative of any
realistic workload — it just measures how fast you can spin a loop on a
hot byte buffer.

For documents that don't fit in memory, the `XmlBytesReader` reads
bytes with a rolling memory window — see the
[parsing guide](/guides/parsing/) for the API.

## XSD validation — head-to-head conformance + wall-clock

The W3C XSD 1.0 test suite (XSTS 2006-11-06) covers 14,328 schemaTests
and 25,092 instanceTests across four contributors (NIST, Sun, Microsoft,
Boeing). Both backends are scored against the same shared denominator
— `+ns:N` instances mean N cases where that backend couldn't compile
the schema (so it never got to attempt validation); they count against
the backend that produced them rather than being silently dropped.

| dimension | n | SupXML | libxml2 |
|---|---:|---:|---:|
| schemaTest (1.0) | 14328 | **98.9%** | 92.9% +3 timeouts |
| instanceTest (1.0) | 25092 | **98.8%** +78 ns | 98.3% +226 ns |
| schemaTest (1.1) | 1096 | **62.7%** | 45.5% |
| instanceTest (1.1) | 1422 | **47.1%** +592 ns | 18.2% +1081 ns |

SupXML leads libxml2 on every axis. The +110-case gap on instanceTest
(1.0) and the +29-point gap on instanceTest (1.1) are headline-worthy;
the 1.1 deltas reflect that libxml2 doesn't implement XSD 1.1 at all
and falls back to "schema didn't compile" for most cases.

Wall-clock for the same corpus (completed cases only; timed-out cases
excluded so a single pathological schema doesn't dominate the
totals):

| backend | schema compile | instance validate | total |
|---|---:|---:|---:|
| sup-xml | 1.74 s | 1.12 s | **2.86 s** |
| libxml2 | 3.96 s *3 timeouts | 2.85 s | **6.81 s *3 timeouts** |

That's **~2.3× faster on schema compile, parity on validate, ~2.4×
faster end-to-end** on the same corpus, plus 3 Microsoft "particles"
schemas (`particlesZ012` / `Z015` / `Z020`) where libxml2's
`xmlSchemaCheckElementDeclComponent` enters quadratic behaviour and
hits the per-test 30 s timeout — SupXML compiles each in under a
millisecond. Reproduce with:

```bash
cargo bench -p sup-xml-bench --bench xsts_compliance
cargo bench -p sup-xml-bench --bench xsts11_compliance
```

## XPath 1.0 — correctness

Two corpora — a 87-test hand-curated spec baseline and libxml2's own
327-test corpus (vendored at `tests/assets/xpath-libxml2-corpus/`):

| corpus | n | SupXML strict | SupXML compat | libxml2 |
|---|---:|---:|---:|---:|
| Hand-curated spec baseline | 87 | **87/87 (100%)** | — | 87/87 (100%) |
| libxml2's own corpus | 327 | **327/327 (100%)** | 325/327 (99.4%) | 312/327 (95.4%) |

On its own test corpus libxml2 fails 15 expressions against the XPath
1.0 spec — bugs that we annotated in the bench's spec-graded override
table (exponent in number literals, decimal-only `string()` output for
big numbers, `number('-')` should be `NaN` not `-0`, IEEE round-to-
nearest-even). For migrations from libxml2 pipelines, SupXML exposes
`XPathOptions { libxml2_compatible: true }` that closes 13 of the 15
cases by relaxing the lexer and matching libxml2's bignum formatting.
The remaining 2 are a real IEEE rounding bug in libxml2's number parser
that we deliberately do not replicate even in compat mode. Reproduce
with:

```bash
cargo bench -p sup-xml-bench --bench xpath_compliance
cargo bench -p sup-xml-bench --bench xpath_libxml2_corpus
```

## HTML parse — matched against html5ever and libxml2

SupXML's HTML5 parser is built on `html5ever`; the comparison below
uses the same fixture set as parse-XML. `html5ever*` is the same
tokenizer driven into a no-op `TreeSink` (discards every node) — a
calibration baseline showing how much of SupXML's runtime is sink
overhead vs html5ever's own work.

| fixture | sup-xml | html5ever* | libxml2 | sx vs lx | sx vs h5e* |
|---|---:|---:|---:|---:|---:|
| hn | 48 MB/s | 46 MB/s | 49 MB/s | 0.97× | 1.03× |
| mdn_table | 69 MB/s | 68 MB/s | 81 MB/s | 0.85× | 1.01× |
| bbc_news | 140 MB/s | 135 MB/s | 113 MB/s | **1.24×** | 1.04× |
| github_rust | 75 MB/s | 75 MB/s | 73 MB/s | **1.02×** | 1.00× |
| wikipedia_ww2 | 71 MB/s | 71 MB/s | 67 MB/s | **1.07×** | 1.01× |
| guardian | 118 MB/s | 114 MB/s | 119 MB/s | 0.99× | 1.04× |
| **geomean (9 fixtures)** | | | | **1.02×** | **1.01×** |

Median geomean against libxml2 is **~1.02×** in SupXML's favour;
matched-contract head-to-head HTML throughput is essentially at parity
with both libxml2 (whose HTML parser is HTML4-era and laxer) and a no-
sink html5ever calibration baseline. The full HTML methodology is in
the [migrating-from-libxml2 guide](/guides/migrating-from-libxml2/).
Reproduce with:

```bash
cargo bench -p sup-xml-bench --bench html_parse
```

## Reproducing locally

```bash
# Clone
git clone https://github.com/SupsoOrg/sup-xml
cd sup-xml

# Full bench suite (~30 minutes)
cargo bench -p sup-xml-bench

# A specific bench (each is independent)
cargo bench -p sup-xml-bench --bench head_to_head
cargo bench -p sup-xml-bench --bench xsts_compliance
cargo bench -p sup-xml-bench --bench xpath_libxml2_corpus
cargo bench -p sup-xml-bench --bench html_parse
```

## Bench inventory

| Bench file | What it measures |
|---|---|
| `head_to_head.rs` | Parse throughput vs libxml2, quick-xml, roxmltree, xml-rs (matched contract) |
| `mini.rs` | Fast smoke bench across 12 parser configurations |
| `parse.rs` | Criterion-style parse throughput |
| `in_place.rs` / `in_place_vs_expat.rs` | Destructive in-place parsing vs Expat SAX |
| `html_parse.rs` | HTML5 parse throughput vs html5ever and libxml2 |
| `stream.rs` / `stream_libxml2.rs` | Streaming parse throughput |
| `xpath_compliance.rs` | XPath 1.0 hand-curated spec baseline |
| `xpath_libxml2_corpus.rs` | XPath 1.0 conformance on libxml2's own corpus |
| `xsd.rs` | XSD validation micro-throughput |
| `xsts_compliance.rs` | XSD 1.0 W3C test-suite pass rate + wall-clock |
| `xsts11_compliance.rs` | XSD 1.1 W3C test-suite pass rate |
| `xmlts_compliance.rs` | XML 1.0 not-wf conformance |
| `exslt_head_to_head.rs` | EXSLT function throughput |
| `recovery_check.rs` | Recovery-mode round-trip checks |
| `libxml2_recovery_inspector.rs` | Observes libxml2's recovery behaviour for diffing |
| `event_counts.rs` | SAX event counts, validates contract parity |

## Methodology

The harness enforces a **matched contract** — all parsers under test
must validate UTF-8, reject malformed structure, resolve the five
predefined entities, and normalise attributes per XML 1.0 § 3.3.3.
Parsers that expose only a looser contract (e.g. `quick-xml` with
`check_end_names: false`) get an asterisk on the comparison and a
note in `crates/bench/benches/head_to_head.rs` documenting what flags
were flipped.

XSD wall-clock numbers exclude per-case timeouts so a single
pathological schema can't dominate the totals; the timeout count is
surfaced inline as `*N timeouts`. XSTS conformance percentages use a
shared denominator across backends so backends with more schema-
compile failures can't flatter their headline percentage by silently
shrinking their own denominator.
