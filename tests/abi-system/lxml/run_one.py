"""Run a single lxml suite test (selected from the module's own
test_suite()) in isolation and print one machine-parseable result line:

    STATUS<TAB>test-id<TAB>cause

STATUS is PASS, FAIL, or ERROR.  Selection: the test whose id's last
segment equals <name>, else any test whose id contains <name>.

Usage: run_one.py <module> <name>
"""
import io
import sys
import unittest


def iter_tests(suite):
    for t in suite:
        if isinstance(t, unittest.TestSuite):
            yield from iter_tests(t)
        else:
            yield t


def main():
    modname, name = sys.argv[1], sys.argv[2]
    try:
        mod = __import__(f"lxml.tests.{modname}", fromlist=[modname])
        suite = mod.test_suite() if hasattr(mod, "test_suite") else \
            unittest.TestLoader().loadTestsFromModule(mod)
    except Exception as e:
        print(f"ERROR\t{modname}.{name}\t<build> {type(e).__name__}: {e}")
        return
    tests = list(iter_tests(suite))
    exact = [t for t in tests if t.id().split(".")[-1] == name]
    matches = exact or [t for t in tests if name in t.id()]
    if not matches:
        print(f"MISSING\t{modname}.{name}\tno such test")
        return
    for t in matches:
        r = unittest.TextTestRunner(
            verbosity=0, stream=io.StringIO(), buffer=True
        ).run(unittest.TestSuite([t]))
        if r.wasSuccessful():
            print(f"PASS\t{t.id()}\t")
        else:
            kind = "ERROR" if r.errors else "FAIL"
            _, tb = (r.errors + r.failures)[0]
            lines = [l for l in tb.strip().splitlines() if l.strip()]
            print(f"{kind}\t{t.id()}\t{lines[-1][:120]}")


if __name__ == "__main__":
    main()
