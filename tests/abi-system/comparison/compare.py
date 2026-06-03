#!/usr/bin/env python3
"""
Comparison harness: drive each libxml2 entry point against both the
system `/usr/lib/libxml2.2.dylib` and our `libsup_xml_compat.dylib`,
assert results match, and time both.

Output is a single table per category — one row per function with:
  match   = ✓ if results identical (or a comment if differences are
            known and harmless)
  sys     = ns/call against the system libxml2
  ours    = ns/call against sup-xml-compat
  ratio   = sys / ours  (higher = we're faster)

A line printed in red means: results differ AND the difference is
unexplained — investigate.  A line printed in yellow means: results
differ but the test annotates a known-harmless reason.

The harness uses ctypes so both dylibs coexist in the same Python
process (different handles, different load addresses, no namespace
games).  No DYLD_* tricks needed for this test.
"""

import ctypes
import os
import statistics
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
SYS_LIB  = "/usr/lib/libxml2.2.dylib"
OUR_LIB  = REPO / "target" / "debug" / "libsup_xml_compat.dylib"

if not OUR_LIB.exists():
    print(f"ERROR: build sup-xml-compat first ({OUR_LIB} missing)", file=sys.stderr)
    sys.exit(2)

sys_lib  = ctypes.CDLL(SYS_LIB,  mode=ctypes.RTLD_LOCAL)
our_lib  = ctypes.CDLL(str(OUR_LIB), mode=ctypes.RTLD_LOCAL)

# ── ANSI colors (skip on non-tty) ───────────────────────────────────────────
_C = sys.stdout.isatty()
RED    = "\033[31m" if _C else ""
GREEN  = "\033[32m" if _C else ""
YELLOW = "\033[33m" if _C else ""
DIM    = "\033[2m"  if _C else ""
BOLD   = "\033[1m"  if _C else ""
RESET  = "\033[0m"  if _C else ""

# ── timing helper ──────────────────────────────────────────────────────────

def _time_call(fn, args, iters):
    """Run `fn(*args)` `iters` times, return (last_result, ns_per_call_median)."""
    samples = []
    # 3 warm-up rounds discarded.
    for _ in range(3):
        fn(*args)
    # 5 timed batches; take the median.
    for _ in range(5):
        start = time.perf_counter_ns()
        for _ in range(iters):
            result = fn(*args)
        elapsed = time.perf_counter_ns() - start
        samples.append(elapsed / iters)
    return result, statistics.median(samples)


# ── per-function comparison driver ─────────────────────────────────────────

class TestCase:
    """One row in the comparison table.

    `name`         display label (typically the function name).
    `attr`         attribute on the ctypes lib (e.g. "xmlStrlen").
    `argtypes`     list of ctypes types for the function's args.
    `restype`      ctypes return type.
    `args`         the actual call's argument values.
    `eq`           callable(sys_result, our_result) → (bool, comment).
                   Default: `==` with no comment.  Annotate known
                   differences via the comment.
    `iters`        timing iterations.  Default 100k for cheap calls,
                   smaller for expensive ones.
    `acceptable_slowdown`
                   When set to a non-empty string, slowdowns below
                   0.75x don't get a red flag — the reason is shown
                   inline as an informational tag.  Use for cases
                   where we've decided "slower by design / acceptable
                   tradeoff".
    """
    __slots__ = ("name", "attr", "argtypes", "restype", "args", "eq",
                 "iters", "acceptable_slowdown")

    def __init__(self, name, attr, argtypes, restype, args,
                 eq=None, iters=100_000, acceptable_slowdown=""):
        self.name = name
        self.attr = attr
        self.argtypes = argtypes
        self.restype = restype
        self.args = args
        self.eq = eq or (lambda a, b: (a == b, ""))
        self.iters = iters
        self.acceptable_slowdown = acceptable_slowdown


def configure(lib, tc):
    fn = getattr(lib, tc.attr)
    fn.argtypes = tc.argtypes
    fn.restype  = tc.restype
    return fn


def run_one(tc):
    sys_fn = configure(sys_lib, tc)
    our_fn = configure(our_lib, tc)
    sys_result, sys_ns = _time_call(sys_fn, tc.args, tc.iters)
    our_result, our_ns = _time_call(our_fn, tc.args, tc.iters)
    match, note = tc.eq(sys_result, our_result)
    ratio = sys_ns / our_ns if our_ns > 0 else float("inf")
    return {
        "name":      tc.name,
        "match":     match,
        "note":      note,
        "sys":       sys_ns,
        "ours":      our_ns,
        "ratio":     ratio,
        "ok_slow":   tc.acceptable_slowdown,
    }


def format_row(r):
    if r["match"]:
        match = f"{GREEN}✓{RESET}"
    elif r["note"]:
        match = f"{YELLOW}✗ ({r['note']}){RESET}"
    else:
        match = f"{RED}✗{RESET}"
    # ratio: green when ours is faster (ratio > 1.0), yellow when within
    # 25% either way OR slower-but-acceptable, red when ours is >25%
    # slower AND no acceptable_slowdown annotation.
    ratio = r["ratio"]
    if ratio >= 1.25:
        ratio_s = f"{GREEN}{ratio:5.2f}x{RESET}"
    elif ratio >= 0.75:
        ratio_s = f"{YELLOW}{ratio:5.2f}x{RESET}"
    elif r["ok_slow"]:
        ratio_s = f"{YELLOW}{ratio:5.2f}x ({r['ok_slow']}){RESET}"
    else:
        ratio_s = f"{RED}{ratio:5.2f}x{RESET}"
    return (
        f"  {r['name']:32s}  "
        f"match={match:24s}  "
        f"sys={r['sys']:6.0f}ns  "
        f"ours={r['ours']:6.0f}ns  "
        f"speedup={ratio_s}"
    )


def run_category(title, cases):
    print(f"\n{BOLD}══ {title} ══{RESET}")
    rows = [run_one(c) for c in cases]
    for r in rows:
        print(format_row(r))
    return rows


# ── test cases ─────────────────────────────────────────────────────────────

c_char_p   = ctypes.c_char_p
c_int      = ctypes.c_int
c_void_p   = ctypes.c_void_p

STRING_TESTS = [
    TestCase(
        "xmlStrlen('hello')",
        attr="xmlStrlen",
        argtypes=[c_char_p], restype=c_int,
        args=(b"hello",),
    ),
    TestCase(
        "xmlStrlen(NULL)",
        attr="xmlStrlen",
        argtypes=[c_char_p], restype=c_int,
        args=(None,),
    ),
    TestCase(
        "xmlStrcmp(eq)",
        attr="xmlStrcmp",
        argtypes=[c_char_p, c_char_p], restype=c_int,
        args=(b"alpha", b"alpha"),
    ),
    TestCase(
        "xmlStrcmp(a<b)",
        attr="xmlStrcmp",
        argtypes=[c_char_p, c_char_p], restype=c_int,
        # libxml2 returns the byte difference (any negative int);
        # we normalize to -1.  Compare just the sign.
        eq=lambda a, b: (((a < 0) == (b < 0)) and ((a > 0) == (b > 0)),
                         "byte-diff vs ±1 — both report 'less'"),
        args=(b"alpha", b"beta"),
    ),
    TestCase(
        "xmlStrcasecmp(eq)",
        attr="xmlStrcasecmp",
        argtypes=[c_char_p, c_char_p], restype=c_int,
        args=(b"HELLO", b"hello"),
        # Our impl uses a clear per-byte ascii-fold loop;
        # libxml2 has a tighter C inner loop.  Could optimize with
        # SIMD/memchr but not worth it at sub-microsecond scale.
        acceptable_slowdown="per-byte fold loop",
    ),
    TestCase(
        "xmlStrncmp(prefix)",
        attr="xmlStrncmp",
        argtypes=[c_char_p, c_char_p, c_int], restype=c_int,
        args=(b"abcdef", b"abcxyz", 3),
    ),
    TestCase(
        "xmlStrchr(found)",
        attr="xmlStrchr",
        argtypes=[c_char_p, c_int], restype=c_void_p,
        # Both return non-NULL pointers into their respective input
        # strings (which happen to be the same byte string in Python's
        # memory because we pass the same bytes object).  Compare for
        # both-non-NULL.
        eq=lambda a, b: (((a is None) == (b is None)),
                         "both find/miss; addresses differ by allocator"),
        args=(b"hello world", ord('w')),
    ),
    TestCase(
        "xmlStrchr(missing)",
        attr="xmlStrchr",
        argtypes=[c_char_p, c_int], restype=c_void_p,
        args=(b"hello", ord('z')),
    ),
    TestCase(
        "xmlStrstr(found)",
        attr="xmlStrstr",
        argtypes=[c_char_p, c_char_p], restype=c_void_p,
        eq=lambda a, b: (((a is None) == (b is None)),
                         "both find/miss; addresses differ"),
        args=(b"the quick brown fox", b"brown"),
        # Naive O(n*m) `windows().position()`; libxml2 has a tighter
        # byte-scan inner loop.  Would need `memchr::memmem` for parity;
        # haystacks are short in practice so not on the critical path.
        acceptable_slowdown="naive substring search",
    ),
    TestCase(
        "xmlStrdup(short)",
        attr="xmlStrdup",
        argtypes=[c_char_p], restype=c_void_p,
        # Both allocate; bytes match.  We compare via dereferenced string.
        eq=lambda a, b: (
            (a is not None) and (b is not None)
            and ctypes.string_at(a) == ctypes.string_at(b),
            "compare dereferenced contents; addresses differ"),
        args=(b"hello",),
        # xmlStrdup allocates → also test that.  Lower iters so we don't
        # leak too much memory (no xmlFree in the loop).
        iters=10_000,
        # 4× slower because we register every allocation in a
        # `Mutex<HashSet<usize>>` so xmlFree can distinguish our heap
        # allocations from arena pointers.  libxml2's xmlStrdup is just
        # malloc+memcpy.  Tradeoff: we gain "xmlFree-on-arena-pointer
        # is a safe no-op" (T-MEM-02); they don't have that.
        # Future optimization: lock-free registry or magic-prefix.
        acceptable_slowdown="alloc registry (Mutex<HashSet>::insert)",
    ),
    TestCase(
        "xmlStrEqual(eq)",
        attr="xmlStrEqual",
        argtypes=[c_char_p, c_char_p], restype=c_int,
        args=(b"alpha", b"alpha"),
    ),
    TestCase(
        "xmlStrEqual(neq)",
        attr="xmlStrEqual",
        argtypes=[c_char_p, c_char_p], restype=c_int,
        args=(b"alpha", b"beta"),
    ),
    TestCase(
        "xmlStrcat(NULL,bar)",
        attr="xmlStrcat",
        argtypes=[c_void_p, c_char_p], restype=c_void_p,
        # xmlStrcat(NULL, add) = xmlStrdup(add) per libxml2 contract;
        # using NULL avoids the must-be-allocated-cur requirement
        # that would otherwise segfault on a const Python bytes obj.
        eq=lambda a, b: (
            (a is not None) and (b is not None)
            and ctypes.string_at(a) == ctypes.string_at(b),
            "compare dereferenced contents; addresses differ"),
        args=(None, b"bar"),
        iters=10_000,
        acceptable_slowdown="alloc registry",
    ),
    TestCase(
        "xmlStrsub(mid)",
        attr="xmlStrsub",
        argtypes=[c_char_p, c_int, c_int], restype=c_void_p,
        eq=lambda a, b: (
            (a is not None) and (b is not None)
            and ctypes.string_at(a) == ctypes.string_at(b),
            "compare dereferenced contents; addresses differ"),
        args=(b"abcdefghij", 2, 4),
        iters=10_000,
        acceptable_slowdown="alloc registry",
    ),
    TestCase(
        "xmlUTF8Strlen('hello')",
        attr="xmlUTF8Strlen",
        argtypes=[c_char_p], restype=c_int,
        args=(b"hello",),
    ),
    TestCase(
        "xmlUTF8Strlen(2-byte)",
        attr="xmlUTF8Strlen",
        argtypes=[c_char_p], restype=c_int,
        # "café" = 4 codepoints, 5 bytes (é is 2 bytes)
        args=("café".encode("utf-8"),),
    ),
    TestCase(
        "xmlUTF8Strsize('café', 4)",
        attr="xmlUTF8Strsize",
        argtypes=[c_char_p, c_int], restype=c_int,
        args=("café".encode("utf-8"), 4),
    ),
    TestCase(
        "xmlUTF8Charcmp(eq)",
        attr="xmlUTF8Charcmp",
        argtypes=[c_char_p, c_char_p], restype=c_int,
        args=("é".encode("utf-8"), "é".encode("utf-8")),
    ),
]

NAME_VALIDATOR_TESTS = [

    TestCase(
        "xmlValidateNameValue('hello')",
        attr="xmlValidateNameValue",
        argtypes=[c_char_p], restype=c_int,
        args=(b"hello",),
    ),
    TestCase(
        "xmlValidateNameValue('3bad')",
        attr="xmlValidateNameValue",
        argtypes=[c_char_p], restype=c_int,
        # Name must not start with a digit.
        args=(b"3bad",),
    ),
    TestCase(
        "xmlValidateNameValue('foo:bar')",
        attr="xmlValidateNameValue",
        argtypes=[c_char_p], restype=c_int,
        # `:` is valid in Name production (it's the QName that splits).
        args=(b"foo:bar",),
    ),
    TestCase(
        "xmlValidateNCName('foo')",
        attr="xmlValidateNCName",
        argtypes=[c_char_p, c_int], restype=c_int,
        args=(b"foo", 0),
    ),
    TestCase(
        "xmlValidateNCName('foo:bar')",
        attr="xmlValidateNCName",
        argtypes=[c_char_p, c_int], restype=c_int,
        # NCName disallows colons.
        args=(b"foo:bar", 0),
    ),
    TestCase(
        "xmlValidateQName('foo:bar')",
        attr="xmlValidateQName",
        argtypes=[c_char_p, c_int], restype=c_int,
        args=(b"foo:bar", 0),
    ),
    TestCase(
        "xmlValidateQName('foo:3bad')",
        attr="xmlValidateQName",
        argtypes=[c_char_p, c_int], restype=c_int,
        # Local part can't start with a digit.
        args=(b"foo:3bad", 0),
    ),
    TestCase(
        "xmlCheckUTF8('hello')",
        attr="xmlCheckUTF8",
        argtypes=[c_char_p], restype=c_int,
        args=(b"hello",),
    ),
    TestCase(
        "xmlCheckUTF8(invalid)",
        attr="xmlCheckUTF8",
        argtypes=[c_char_p], restype=c_int,
        # Lone continuation byte: never valid UTF-8.
        args=(b"\x80\x80",),
    ),
]

URI_TESTS = [
    TestCase(
        "xmlBuildURI(rel,base)",
        attr="xmlBuildURI",
        argtypes=[c_char_p, c_char_p], restype=c_void_p,
        eq=lambda a, b: (
            (a is not None) and (b is not None)
            and ctypes.string_at(a) == ctypes.string_at(b),
            "compare dereferenced contents; addresses differ"),
        args=(b"foo.xml", b"http://example.com/dir/"),
        iters=10_000,
        acceptable_slowdown="alloc registry",
    ),
    TestCase(
        "xmlURIUnescapeString",
        attr="xmlURIUnescapeString",
        argtypes=[c_char_p, c_int, c_char_p], restype=c_void_p,
        eq=lambda a, b: (
            (a is not None) and (b is not None)
            and ctypes.string_at(a) == ctypes.string_at(b),
            "compare dereferenced contents; addresses differ"),
        args=(b"hello%20world", 0, None),
        iters=10_000,
        acceptable_slowdown="alloc registry",
    ),
]

MEMORY_ACCT_TESTS = [
    TestCase(
        "xmlMemSize(NULL)",
        attr="xmlMemSize",
        argtypes=[c_void_p], restype=ctypes.c_size_t,
        # Both return 0 — libxml2 returns 0 without WITH_MEM_DEBUG;
        # we return 0 unconditionally.
        args=(None,),
    ),
    TestCase(
        "xmlMemUsed()",
        attr="xmlMemUsed",
        argtypes=[], restype=c_int,
        # Both 0 in the absence of WITH_MEM_DEBUG.
        args=(),
    ),
    TestCase(
        "xmlMemBlocks()",
        attr="xmlMemBlocks",
        argtypes=[], restype=c_int,
        args=(),
    ),
]

ENCODING_TESTS = [
    TestCase(
        "xmlGetCharEncodingName(UTF8)",
        attr="xmlGetCharEncodingName",
        argtypes=[c_int], restype=c_char_p,
        args=(1,),  # XML_CHAR_ENCODING_UTF8
    ),
    TestCase(
        "xmlGetCharEncodingName(UTF16LE)",
        attr="xmlGetCharEncodingName",
        argtypes=[c_int], restype=c_char_p,
        args=(2,),
    ),
    TestCase(
        "xmlGetCharEncodingName(ASCII)",
        attr="xmlGetCharEncodingName",
        argtypes=[c_int], restype=c_char_p,
        args=(22,),
    ),
    TestCase(
        "xmlGetCharEncodingName(unknown)",
        attr="xmlGetCharEncodingName",
        argtypes=[c_int], restype=c_char_p,
        # Both should return NULL.  ctypes maps NULL c_char_p to None.
        args=(9999,),
    ),
    TestCase(
        "xmlDetectCharEncoding(UTF8 BOM)",
        attr="xmlDetectCharEncoding",
        argtypes=[c_char_p, c_int], restype=c_int,
        args=(b"\xef\xbb\xbf<root/>", 7),
    ),
    TestCase(
        "xmlDetectCharEncoding(UTF16LE BOM)",
        attr="xmlDetectCharEncoding",
        argtypes=[c_char_p, c_int], restype=c_int,
        args=(b"\xff\xfe<\x00r\x00", 6),
    ),
    TestCase(
        "xmlDetectCharEncoding(no BOM)",
        attr="xmlDetectCharEncoding",
        argtypes=[c_char_p, c_int], restype=c_int,
        args=(b"<root/>", 7),
    ),
]


# ── public-variable comparison ─────────────────────────────────────────────
#
# libxml2 exposes globals that consumers read or write directly
# (`xmlStringText`, `xmlParserVersion`, allocator function pointers).
# These don't fit the function-call TestCase shape — there's no return
# value to time and no arguments to vary.  We compare them by reading
# the bytes / pointer value from both dylibs and asserting equality
# (or, for opaque pointers, both non-NULL).

class VariableCase:
    """One row in the public-variables table.

    `name`     display label.
    `attr`     symbol name as exported by the dylib.
    `reader`   callable(lib) → value.  Use `_bytes_global(size)` for
               fixed-size byte arrays, `_cstr_global()` for
               NUL-terminated strings reached via a pointer slot,
               `_fnptr_nonnull_global()` for function-pointer slots.
    `note`     when present, a short reason describing an annotated
               (acceptable) difference between sys/our values.
    """
    __slots__ = ("name", "attr", "reader", "note")

    def __init__(self, name, attr, reader, note=""):
        self.name   = name
        self.attr   = attr
        self.reader = reader
        self.note   = note


def _bytes_global(size):
    """Read `attr` as a `c_char * size` byte array.  Returns the bytes
    payload (trims at the first NUL — same convention libxml2 uses)."""
    def read(lib, attr):
        arr = (ctypes.c_char * size).in_dll(lib, attr)
        return bytes(arr).rstrip(b"\x00")
    return read


def _cstr_global():
    """Read `attr` as a `c_char_p` slot — the slot holds a pointer to a
    NUL-terminated string elsewhere in the dylib's data section.
    Dereferences the pointer and returns the string bytes (or `None`)."""
    def read(lib, attr):
        slot = ctypes.c_char_p.in_dll(lib, attr)
        return slot.value
    return read


def _fnptr_nonnull_global():
    """Read `attr` as a `c_void_p` slot and return whether it's
    non-NULL.  Useful for the `xmlMalloc` / `xmlRealloc` / `xmlFree`
    function-pointer globals — we can't sensibly compare their actual
    addresses (each dylib has its own malloc), but we can confirm
    both libs initialize the slot."""
    def read(lib, attr):
        slot = ctypes.c_void_p.in_dll(lib, attr)
        return slot.value is not None and slot.value != 0
    return read


def run_variable_one(vc):
    try:
        sys_val = vc.reader(sys_lib, vc.attr)
    except (ValueError, AttributeError) as e:
        return {"name": vc.name, "match": False, "note": f"sys missing: {e}",
                "sys_val": None, "our_val": None}
    try:
        our_val = vc.reader(our_lib, vc.attr)
    except (ValueError, AttributeError) as e:
        return {"name": vc.name, "match": False, "note": f"ours missing: {e}",
                "sys_val": sys_val, "our_val": None}
    if sys_val == our_val:
        return {"name": vc.name, "match": True, "note": "",
                "sys_val": sys_val, "our_val": our_val}
    return {"name": vc.name, "match": False, "note": vc.note,
            "sys_val": sys_val, "our_val": our_val}


def format_variable_row(r):
    if r["match"]:
        match = f"{GREEN}✓{RESET}"
    elif r["note"]:
        match = f"{YELLOW}✗ ({r['note']}){RESET}"
    else:
        match = f"{RED}✗{RESET}"
    sys_s  = repr(r["sys_val"]) if not isinstance(r["sys_val"], bytes) else r["sys_val"]
    our_s  = repr(r["our_val"]) if not isinstance(r["our_val"], bytes) else r["our_val"]
    return (
        f"  {r['name']:32s}  "
        f"match={match:24s}  "
        f"sys={sys_s!r}  ours={our_s!r}"
    )


def run_variable_category(title, cases):
    print(f"\n{BOLD}══ {title} ══{RESET}")
    rows = [run_variable_one(c) for c in cases]
    for r in rows:
        print(format_variable_row(r))
    return rows


VARIABLE_TESTS = [
    VariableCase(
        "xmlStringText",
        attr="xmlStringText",
        reader=_bytes_global(5),  # b"text\0"
    ),
    VariableCase(
        "xmlStringTextNoenc",
        attr="xmlStringTextNoenc",
        reader=_bytes_global(10),  # b"textnoenc\0"
    ),
    VariableCase(
        "xmlParserVersion",
        attr="xmlParserVersion",
        reader=_cstr_global(),
        # Our version string deliberately differs from libxml2's;
        # presence is what matters here.
        note="version string differs by design",
    ),
    VariableCase(
        "xmlMalloc (fnptr non-NULL)",
        attr="xmlMalloc",
        reader=_fnptr_nonnull_global(),
    ),
    VariableCase(
        "xmlRealloc (fnptr non-NULL)",
        attr="xmlRealloc",
        reader=_fnptr_nonnull_global(),
    ),
    VariableCase(
        "xmlMallocAtomic (fnptr non-NULL)",
        attr="xmlMallocAtomic",
        reader=_fnptr_nonnull_global(),
    ),
    VariableCase(
        "xmlFree (fnptr non-NULL)",
        attr="xmlFree",
        reader=_fnptr_nonnull_global(),
    ),
]


# ── main ──────────────────────────────────────────────────────────────────

CATEGORIES = [
    ("xmlStr* string utilities", STRING_TESTS),
    ("Name validators",          NAME_VALIDATOR_TESTS),
    ("URI helpers",              URI_TESTS),
    ("Memory accounting",        MEMORY_ACCT_TESTS),
    ("Encoding helpers",         ENCODING_TESTS),
]
VARIABLE_CATEGORIES = [
    ("Public variables",         VARIABLE_TESTS),
]

def main():
    print(f"{BOLD}libxml2 (system) vs sup-xml-compat — comparison run{RESET}")
    print(f"  system:  {SYS_LIB}")
    print(f"  ours:    {OUR_LIB}")
    n_match = n_mismatch_known = n_mismatch_bad = 0
    slow_rows = []
    for title, cases in CATEGORIES:
        rows = run_category(title, cases)
        for r in rows:
            if r["match"]:
                n_match += 1
            elif r["note"]:
                n_mismatch_known += 1
            else:
                n_mismatch_bad += 1
            # Flag anything where ours is more than 25 % slower AND
            # there's no annotated explanation.
            if r["ratio"] < 0.75 and not r["ok_slow"]:
                slow_rows.append(r)
    for title, cases in VARIABLE_CATEGORIES:
        rows = run_variable_category(title, cases)
        for r in rows:
            if r["match"]:
                n_match += 1
            elif r["note"]:
                n_mismatch_known += 1
            else:
                n_mismatch_bad += 1

    print()
    print(f"{BOLD}── summary ──{RESET}")
    print(f"  identical:        {GREEN}{n_match}{RESET}")
    print(f"  diff (annotated): {YELLOW}{n_mismatch_known}{RESET}")
    print(f"  diff (unknown):   {RED}{n_mismatch_bad}{RESET}")
    if slow_rows:
        print(f"  {RED}slowdowns (>25% slower than system libxml2):{RESET}")
        for r in slow_rows:
            print(f"    {r['name']}: {r['ratio']:.2f}x")
    sys.exit(1 if n_mismatch_bad else 0)


if __name__ == "__main__":
    main()
