"""Enumerate every failure/error in lxml's upstream suite against the shim.

Run via run_dump.sh (which sets PYTHONPATH/DYLD/SUPSO_LICENSE_PATH).  Prints one
line per failing test: `MODULE TESTNAME KIND | last-traceback-line`, then a
tally grouped by the trailing exception line so the biggest buckets are
obvious.
"""
import io
import os
import sys
import unittest
from collections import Counter

def load_divergences():
    """Map `module.test_method` -> (category, rationale) from
    known_divergences.tsv: tests we intentionally don't match (see that
    file's header).  Returns {} if the file is absent."""
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                        "known_divergences.tsv")
    out = {}
    try:
        with open(path, encoding="utf-8") as f:
            for line in f:
                line = line.rstrip("\n")
                if not line.strip() or line.lstrip().startswith("#"):
                    continue
                parts = line.split("\t")
                if len(parts) >= 3:
                    out[parts[0].strip()] = (parts[1].strip(), parts[2].strip())
    except FileNotFoundError:
        pass
    return out

TEST_MODULES = [
    "test_annotations", "test_builder", "test_classlookup", "test_css",
    "test_doctestcompare", "test_dtd", "test_elementpath", "test_elementtree",
    "test_errors", "test_etree", "test_external_document", "test_htmlparser",
    "test_http_io", "test_incremental_xmlfile", "test_io", "test_isoschematron",
    "test_nsclasses", "test_objectify", "test_pyclasslookup", "test_relaxng",
    "test_sax", "test_schematron", "test_threading", "test_unicode",
    "test_xmlschema", "test_xpathevaluator", "test_xslt",
]

def last_line(tb: str) -> str:
    lines = [l for l in tb.strip().splitlines() if l.strip()]
    return lines[-1].strip() if lines else "(empty)"

def main():
    only = sys.argv[1:] if len(sys.argv) > 1 else TEST_MODULES
    divergences = load_divergences()
    buckets = Counter()
    rows = []
    diverged = []
    for modname in only:
        try:
            mod = __import__(f"lxml.tests.{modname}", fromlist=[modname])
        except Exception as e:
            rows.append((modname, "<import>", "ERR", f"{type(e).__name__}: {e}"))
            buckets[f"<import> {type(e).__name__}"] += 1
            continue
        # lxml modules expose test_suite() which selects only the concrete,
        # lxml-targeting cases; loadTestsFromModule would also run the
        # abstract bases (etree=None) and the stdlib-ElementTree cases.
        if hasattr(mod, "test_suite"):
            suite = mod.test_suite()
        else:
            suite = unittest.TestLoader().loadTestsFromModule(mod)
        result = unittest.TextTestRunner(
            verbosity=0, stream=io.StringIO(), buffer=True
        ).run(suite)
        for kind, entries in (("ERR", result.errors), ("FAIL", result.failures)):
            for t, tb in entries:
                name = t.id().split(".")[-1]
                key = f"{modname}.{name}"
                if key in divergences:
                    diverged.append((modname, name, divergences[key][0]))
                    continue
                ll = last_line(tb)
                rows.append((modname, name, kind, ll))
                buckets[ll[:70]] += 1

    for modname, name, kind, ll in rows:
        print(f"{modname:28} {name:48} {kind:4} | {ll[:110]}")

    print("\n==== grouped by trailing line (top 40) ====")
    for cause, n in buckets.most_common(40):
        print(f"{n:5}  {cause}")
    print(f"\nTOTAL real fail+err rows: {len(rows)}")

    if diverged:
        print(f"\n==== intentional divergences (not bugs): {len(diverged)} ====")
        for modname, name, cat in diverged:
            print(f"{modname:28} {name:48} [{cat}]")

if __name__ == "__main__":
    main()
