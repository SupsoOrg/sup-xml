---
title: W3C conformance
description: SupXML's score on the W3C XML Conformance Test Suite.
---

## SupXML's full-catalog score

**SupXML matches the expected outcome on all 2274 deterministic tests
in the W3C XML Conformance Test Suite — zero failures.** The other 21
of the 2295-test catalog are implementation-defined by the spec
(catalog `error` outcome); the breakdown is below.

| Outcome | Count |
|---|---|
| **Deterministic outcome, matched** | **2274** |
| **Implementation-defined** (catalog `error` outcome) | **21** |
| Failed | 0 |
| **Total** | **2295** |

Of the 2274 tests with a deterministic expected outcome (well-formed,
not-well-formed, or invalid), SupXML matches the expected outcome on
every one — **0 failures, 0 known-failing entries on the allowlist**.
The remaining 21 are tagged `error` in the catalog itself, meaning
XML 1.0 explicitly leaves their handling implementation-defined — both
accepting and rejecting them satisfies the spec, so whatever SupXML
does is conformant by definition.

## Cross-parser comparison (not-wf corpus)

The full catalog has different shapes of test (valid / invalid / not-wf),
and xml-rs and quick-xml don't load external DTDs, so a like-for-like
score across every parser only makes sense on the *not-wf* slice —
documents engineered to violate a specific XML 1.0 well-formedness rule.
A conforming parser must reject them. Percentages = correctly rejected:

| Corpus | Files | SupXML | libxml2 | roxmltree | xml-rs | quick-xml |
| --- | --: | --: | --: | --: | --: | --: |
| xmltest (James Clark) | 200 | **99.0%** | 97.0% | 63.5% | 58.5% | 10.0% |
| Sun Microsystems | 57 | **100%** | 98.2% | 31.6% | 33.3% | 8.8% |
| IBM (incl. XML 1.1) | 890 | **94.5%** | 59.6% | 43.5% | 42.7% | 5.2% |
| **All vendors** | **1147** | **95.6%** | **68.0%** | **46.4%** | **45.0%** | **6.2%** |

Reproduce with (absolute path — see the path note below):

```bash
XMLTS_ROOT=$(pwd)/tests/w3c \
    cargo bench -p sup-xml-bench --bench xmlts_compliance
```

The harness lives at `crates/bench/benches/xmlts_compliance.rs`. It walks
every directory named `not-wf` under `XMLTS_ROOT` and asks each parser
the same end-to-end question: did the parser surface an error before
EOF? Timeouts and panics count as wrongly-accepted.

> **Path note**: `XMLTS_ROOT` must be an absolute path because the bench
> binary runs from a cargo-managed working directory, not the repo
> root. Without `XMLTS_ROOT` the bench defaults to
> `tests/assets/xmlts/`, which contains only the James Clark + Sun
> 257-file slice; pointing it at the full `tests/w3c/` tree picks up
> the IBM and other vendor files for the broader 1147-file comparison.

### Where libxml2 beats SupXML

One file:

- `xmltest/not-sa/011` — a markup declaration split across parameter
  entity references in the external subset. libxml2 rejects; SupXML
  accepts. XML 1.0 § 2.8's WFC "PEs in Internal Subset" explicitly
  carves out the external subset, so the construct is well-formed
  per spec — but libxml2's traditional behaviour is to reject it.
  We follow the spec.

### Where SupXML pulls ahead of libxml2

The biggest swing on the 1147-file corpus is `ibm/P85`–`ibm/P87` — tests
for the XML 1.0 `Name` character class. SupXML rejects the malformed
names that those productions target; libxml2 mostly accepts them. The
reason is editions of XML 1.0: the 4th edition (2006) defines
`Name`-allowed characters by tight Unicode ranges; the 5th edition
(2008) loosened that to general Unicode categories. SupXML matches the
catalog's 4th-edition intent (`xml10_fourth_edition = true`); libxml2
implements 5th-edition rules. Both are defensible readings of XML 1.0
— only one matches the catalog.

Six smaller cases (outside the `Name` cluster) where SupXML rejects
files libxml2 accepts:

- `sun/encoding07.xml` — missing-encoding-declaration in an external entity.
- `xmltest/ext-sa/001`, `002`, `003` — external-entity references with
  malformed targets that libxml2 silently skips when it can't load them.
- `xmltest/sa/140`, `141` — invalid `Name`-start characters produced by
  entity expansion.

libxml2's external-DTD loading path has produced multiple XXE CVEs over
the years; SupXML deliberately does not load external DTDs by default,
which side-steps the bug class entirely. See the
[security model](/reference/security/) for the policy.

## About the 21 contested cases

The W3C catalog itself marks these 21 cases with `error` outcome — the
spec documents their handling as implementation-defined, meaning either
accepting or rejecting them is valid per the standard. A subset have test
fixtures that are themselves contrary to the XML 1.0 production rules.
Rather than score them as pass or fail (both of which would be defensible
and neither of which carries useful information), we report them
separately.

For example, `not-wf-not-sa-005` references an undefined parameter entity
from inside an external entity. The XML 1.0 spec's *Entity Declared*
validity constraint says a conforming non-validating processor *may*
report the violation but is not required to — so both raising an error
and silently continuing satisfy the standard. There is no single
"correct" outcome to score against.

XML 1.1 sub-catalogs are also out of scope — we target XML 1.0.

## Where the test suite lives

The W3C XML Conformance Test Suite (revision `xmlts20130923`) ships under
`tests/w3c/` and is exercised by `crates/api/tests/w3c.rs`. The harness:

- Reads every `xmlconf.xml` catalog and walks each `TEST` entry.
- Runs the input through `parse_bytes` with a sandboxed `FixtureResolver`
  that scopes external entity loads to the test directory tree.
- Asserts the outcome matches the catalog's `TYPE=` attribute.

The harness has a `KNOWN_FAILING_IDS` allowlist for surfacing real gaps
without breaking the build. It is currently empty. If a test starts
failing, CI fails — that's the regression gate.

If a test in `KNOWN_FAILING_IDS` *passes* unexpectedly, the build also
fails, forcing the allowlist to stay current.

## Running it yourself

```bash
cargo test --release --test w3c -p sup-xml -- --nocapture
```

Expected output:

```text
W3C XML Conformance: 2274 passed, 0 failed, 0 xfail (allow-listed),
                     21 skipped (of 2295 total)
```

## Versioning

The numbers above are pinned to the **2013-09-23 revision** of the W3C
suite (the most recent release). We re-run the suite in CI on every PR; a
regression that drops the score below 2274 is a release blocker.
