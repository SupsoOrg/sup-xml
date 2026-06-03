# tests/assets/

Real-world XML / HTML / XSD / XPath / XSLT inputs.  Used by the
performance benches under `crates/bench/benches/` and by the
conformance / cross-implementation tests under `crates/*/tests/`.

Anything large or fetched from upstream is **gitignored** — populate
by running the relevant `fetch.sh` once.  Hand-curated fixtures
(small, committed, named meaningfully) live alongside.

## Subfolders

| Path | What's there | Populated by |
|---|---|---|
| [`xml/`](xml/) | ~30 real-world XML documents from 16 KB up to 93 MB (`swiss_prot.xml`).  Used as the fixture set for parser/DOM/streaming benches and the head-to-head suite. | Committed |
| [`xml/attacks/`](xml/attacks/) | Adversarial inputs — fork bombs, deeply nested elements, malformed encoding declarations.  See its README. | Committed |
| [`html/`](html/) | Real HTML pages (BBC News, Hacker News, etc.) for the `html_parse` bench. | `html/fetch.sh` |
| [`xmlts/`](xmlts/) | W3C XML 1.0 Conformance Test Suite — driven by `crates/bench/benches/xmlts_compliance.rs` to tally per-parser pass rates on engineered `not-wf/**/*.xml` inputs. | (committed subset) |
| [`xsts/`](xsts/) | W3C XML Schema Test Suite (2006-11-06 vendor) — driven by `crates/bench/benches/xsts_compliance.rs` for the XSD conformance table in `COMPARISON.md`. | `xsts/fetch.sh` |
| [`xsts-1.1/`](xsts-1.1/) | The XSD 1.1-specific testSets from W3C's `xsdtests` repo (Saxonica, IBM, Oracle, WG contributions).  Superset of XSTS 2006-11-06 for those vendors. | `xsts-1.1/fetch.sh` |
| [`qt3tests/`](qt3tests/) | XQuery/XPath 3.0 test suite.  See its README. | (committed subset) |
| [`xpath-libxml2-corpus/`](xpath-libxml2-corpus/) | libxml2's own XPath test inputs (expression files + source documents).  Used by `crates/bench/benches/xpath_libxml2_corpus.rs` for differential output checks. | `xpath-libxml2-corpus/fetch.sh` |
| [`xslt30-test/`](xslt30-test/) | W3C XSLT 3.0 Test Suite (~110 MB).  Future xslt-conformance work. | `xslt30-test/fetch.sh` |

## How to populate

Run a subfolder's `fetch.sh` once.  Idempotent — re-running skips
anything already present:

```sh
tests/assets/html/fetch.sh
tests/assets/xsts/fetch.sh
tests/assets/xsts-1.1/fetch.sh
tests/assets/xpath-libxml2-corpus/fetch.sh
tests/assets/xslt30-test/fetch.sh
```

`xml/` and `xml/attacks/` are committed and don't need fetching.

## How the assets get consumed

- **Benches** load specific fixtures by relative path — see e.g.
  `crates/bench/benches/stream.rs:20`, `xsd.rs:689`,
  `xmlts_compliance.rs:59`.
- **Conformance tests** walk an entire subdirectory tree — see
  `crates/bench/benches/xsts_compliance.rs`.
- The fixtures' role is to make the perf and conformance reports
  reproducible across machines.  Don't replace a real-world fixture
  with a synthetic one without updating the dependent bench
  comments — they cite specific file shapes / sizes.
