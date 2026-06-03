# tests/w3c/

The W3C XML Conformance Test Suite — a curated set of XML 1.0
documents engineered by Sun, OASIS, James Clark, IBM, and others to
test every well-formedness and validity rule in the spec.  Mirrored
from <https://www.w3.org/XML/Test/>.

Driven by `crates/api/tests/w3c.rs`.

## Layout

| Path | Origin |
|---|---|
| `xmlconf.xml` | Top-level test catalog.  Lists every `*.xml` testcase below by entity reference and tags each with `TYPE="valid|invalid|not-wf|error"`. |
| `testcases.dtd` | DTD used by `xmlconf.xml` itself. |
| `sun/` | Sun Microsystems testcases (the original 1998 contribution). |
| `oasis/` | OASIS additions. |
| `xmltest/` | James Clark's "XMLTEST" suite. |
| `ibm/` | IBM-contributed cases. |
| `eduni/` | University of Edinburgh additions (lots of XML 1.1 specifics). |
| `japanese/` | Japanese encoding and character-class tests. |
| `files/` | Supporting files referenced by testcases. |

## What each test type means

| `TYPE=` | Expected parser behaviour |
|---|---|
| `valid` | Parses + validates against the embedded DTD without error |
| `invalid` | Parses (well-formed) but the DTD rejects it as invalid |
| `not-wf` | Parser must reject as not-well-formed |
| `error` | Parser may emit a warning but should not be fatal |

## How to run

```sh
cargo test -p sup-xml --test w3c
```

This walks `xmlconf.xml`, drives each referenced testcase through
`sup_xml::parse_str`, and asserts the verdict matches `TYPE`.

A small **allowlist** in `crates/api/tests/w3c.rs` documents cases
where we deliberately diverge from the spec (or where the test
itself is contentious in the implementer community).  When fixing a
spec-compliance bug, remove the corresponding entry from the
allowlist; the test will then enforce the fix from then on.

## How to update

This directory is checked in as-is from upstream.  To refresh:

1. Download the latest tarball from <https://www.w3.org/XML/Test/>.
2. Replace the contents here (preserve only structural changes —
   the file layout is referenced by `xmlconf.xml`'s entity decls).
3. Re-run the test — newly added cases may need allowlist entries
   if they regress; document each one with a comment in
   `crates/api/tests/w3c.rs`.
