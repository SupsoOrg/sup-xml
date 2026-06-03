#!/usr/bin/env python3
"""
perl_stub_spec.py — spec tests for libxml2 symbols that XML::LibXML
needs but our compat shim currently stubs.

Each row is a behavioural-parity test against the system libxml2 dylib.
Today the rows are RED (stubs return NULL / -1 while libxml2 does the
real thing). As we real-implement each stub, its row turns GREEN.

This file is *both* the work tracker and the regression net:

  - Work tracker: count of red rows = stubs remaining.
  - Regression net: a row that flips GREEN must stay GREEN.

Run after building the shim with `--features cdylib-exports`:

    target/py-venv/bin/python tests/abi-system/comparison/perl_stub_spec.py

Exit code is 0 only if every row passes — so this file is a CI gate
the moment we want to make it one.

Stubs live in `crates/compat/src/perl_stubs.rs`. When a stub becomes
a real implementation, move the code out of perl_stubs.rs and into the
appropriate compat module (`tree.rs`, `parse.rs`, `xsd.rs`, ...).
"""

from __future__ import annotations

import ctypes
import os
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
SYS_LIB = "/usr/lib/libxml2.2.dylib"
OUR_LIB = REPO / "target" / "debug" / "libsup_xml_compat.dylib"

if not OUR_LIB.exists():
    print(f"ERROR: build sup-xml-compat first ({OUR_LIB} missing)", file=sys.stderr)
    sys.exit(2)

sys_lib = ctypes.CDLL(SYS_LIB,  mode=ctypes.RTLD_LOCAL)
our_lib = ctypes.CDLL(str(OUR_LIB), mode=ctypes.RTLD_LOCAL)

_C    = sys.stdout.isatty()
RED   = "\033[31m" if _C else ""
GREEN = "\033[32m" if _C else ""
DIM   = "\033[2m"  if _C else ""
BOLD  = "\033[1m"  if _C else ""
RESET = "\033[0m"  if _C else ""

c_char_p = ctypes.c_char_p
c_int    = ctypes.c_int
c_long   = ctypes.c_long
c_void_p = ctypes.c_void_p
c_size_t = ctypes.c_size_t

XML_ELEMENT_NODE = 1
XML_PI_NODE      = 7
XML_DOCUMENT_FRAGMENT_NODE = 11


def deref_bytes(p):
    """ctypes.string_at(NULL) raises; return None for NULL."""
    if not p:
        return None
    try:
        return ctypes.string_at(p)
    except Exception:
        return None


def configure(lib, attr, argtypes, restype):
    fn = getattr(lib, attr)
    fn.argtypes = argtypes
    fn.restype  = restype
    return fn


# ── case dispatch ──────────────────────────────────────────────────────────

class Case:
    """One spec row.

    `name`     display label.
    `attr`     symbol name in the cdylib.
    `argtypes` ctypes arg types.
    `restype`  ctypes return type.
    `args`     argument values to pass.
    `eq`       comparison: (sys_result, our_result) -> bool.
               Default: equality of the raw return value.
    `notes`    free-form comment shown on the row.
    `skip`     reason string. If set, row is reported as ⊘ (skipped, not red).
    """
    __slots__ = ("name", "attr", "argtypes", "restype", "args", "eq", "notes", "skip")

    def __init__(self, name, attr, argtypes, restype, args, eq=None, notes="", skip=""):
        self.name     = name
        self.attr     = attr
        self.argtypes = argtypes
        self.restype  = restype
        self.args     = args
        self.eq       = eq or (lambda a, b: a == b)
        self.notes    = notes
        self.skip     = skip


def run(case):
    if case.skip:
        return {"name": case.name, "status": "skip",  "note": case.skip}
    try:
        sys_fn = configure(sys_lib, case.attr, case.argtypes, case.restype)
        our_fn = configure(our_lib, case.attr, case.argtypes, case.restype)
    except AttributeError as e:
        return {"name": case.name, "status": "missing", "note": str(e)}
    try:
        sys_r = sys_fn(*case.args)
        our_r = our_fn(*case.args)
    except Exception as e:
        return {"name": case.name, "status": "exc", "note": f"{type(e).__name__}: {e}"}
    try:
        ok = case.eq(sys_r, our_r)
    except Exception as e:
        return {"name": case.name, "status": "exc", "note": f"eq: {e}"}
    return {"name": case.name, "status": "pass" if ok else "fail",
            "note": case.notes if ok else f"sys={sys_r!r} ours={our_r!r}"}


def fmt(r):
    s = r["status"]
    if s == "pass":
        m = f"{GREEN}✓{RESET}"
    elif s == "fail":
        m = f"{RED}✗{RESET}"
    elif s == "skip":
        m = f"{DIM}⊘{RESET}"
    elif s == "missing":
        m = f"{RED}MISSING{RESET}"
    else:
        m = f"{RED}EXC{RESET}"
    note = f"  {DIM}{r['note']}{RESET}" if r["note"] else ""
    return f"  {r['name']:48s}  {m}{note}"


# ── helpers shared across cases ────────────────────────────────────────────

# Both libs need to share comparable inputs.  For pointer-args, NULL is the
# only value safely comparable across libraries (a doc/node built by lib_a
# cannot be passed to lib_b without UB).  When NULL doesn't make sense, the
# case is marked skip="needs cross-lib fixture" and counted separately.

NULL = None
NEEDS_FIXTURE = "needs cross-lib fixture"
TODO_BEHAVIOR = "spec: behaviour parity with libxml2"


def both_null(a, b):           return a is None and b is None
def both_nonnull(a, b):        return (a is not None) and (b is not None)
def both_int_zero(a, b):       return a == 0 and b == 0
def both_int_neg(a, b):        return a < 0 and b < 0
def same_int(a, b):            return a == b
def same_bytes(a, b):          return deref_bytes(a) == deref_bytes(b)


# ── CASES ──────────────────────────────────────────────────────────────────
#
# Grouped by perl_stubs.rs section.  Each case documents the expected
# behaviour by comparing to system libxml2; today the stubs diverge,
# so most rows are RED.  As real implementations land in compat/, the
# corresponding rows turn GREEN.

PTR_RETURNING = [
    Case("htmlReadDoc(<html>hi</html>)",
         "htmlReadDoc",
         [c_char_p, c_char_p, c_char_p, c_int], c_void_p,
         (b"<html><body><p>hi</p></body></html>", None, None, 0),
         eq=both_nonnull,
         notes="parse HTML5; both libs return non-NULL doc"),
    Case("htmlReadIO(NULL,NULL,NULL,NULL,NULL,0)",
         "htmlReadIO",
         [c_void_p, c_void_p, c_void_p, c_char_p, c_char_p, c_int], c_void_p,
         (None, None, None, None, None, 0),
         eq=both_null,
         notes="NULL callbacks → both libs return NULL"),

    Case("xmlAddSibling(NULL,NULL)",
         "xmlAddSibling",
         [c_void_p, c_void_p], c_void_p,
         (None, None),
         eq=both_null),
    Case("xmlCopyNamespace(NULL)",
         "xmlCopyNamespace",
         [c_void_p], c_void_p,
         (None,),
         eq=both_null),
    Case("xmlCopyProp(NULL,NULL)",
         "xmlCopyProp",
         [c_void_p, c_void_p], c_void_p,
         (None, None),
         eq=both_null),
    Case("xmlNewDocFragment(NULL)",
         "xmlNewDocFragment",
         [c_void_p], c_void_p,
         (None,),
         eq=both_nonnull,
         notes="NULL doc still produces a free-floating fragment"),
    Case("xmlNewNode(NULL,'foo')",
         "xmlNewNode",
         [c_void_p, c_char_p], c_void_p,
         (None, b"foo"),
         eq=both_nonnull),
    Case("xmlNewNode(NULL,NULL)",
         "xmlNewNode",
         [c_void_p, c_char_p], c_void_p,
         (None, None),
         eq=both_null,
         notes="NULL name → NULL"),
    Case("xmlNewPI('xml-stylesheet','href=...')",
         "xmlNewPI",
         [c_char_p, c_char_p], c_void_p,
         (b"xml-stylesheet", b"href=\"a.xsl\""),
         eq=both_nonnull),

    Case("xmlBufferCreateStatic(NULL,0)",
         "xmlBufferCreateStatic",
         [c_void_p, c_size_t], c_void_p,
         (None, 0),
         eq=both_null,
         notes="zero-length static buffer"),
    Case("xmlCharStrndup('hello',5)",
         "xmlCharStrndup",
         [c_char_p, c_int], c_void_p,
         (b"hello", 5),
         eq=same_bytes,
         notes="allocates copy of first 5 bytes"),
    Case("xmlEncodeEntitiesReentrant(NULL,'a<b')",
         "xmlEncodeEntitiesReentrant",
         [c_void_p, c_char_p], c_void_p,
         (None, b"a<b"),
         eq=same_bytes,
         notes="<→&lt; escaping; doc may be NULL"),
    Case("xmlMemStrdup (fnptr global)",
         "xmlMemStrdup",
         [c_char_p], c_void_p,
         (b"hello",),
         eq=same_bytes,
         skip="xmlMemStrdup is a function-pointer global; needs in_dll() pattern, not ctypes call"),
    Case("xmlSplitQName(NULL,'p:l',&px)",
         "xmlSplitQName",
         [c_void_p, c_char_p, c_void_p], c_void_p,
         (None, b"p:l", None),
         eq=both_nonnull,
         notes="returns local part; prefix returned via out-param",
         skip="needs writable out-param fixture for prefix"),

    Case("xmlCreateFileParserCtxt('/nonexistent')",
         "xmlCreateFileParserCtxt",
         [c_char_p], c_void_p,
         (b"/nonexistent",),
         eq=both_null,
         notes="missing file → NULL on both"),
    Case("xmlCreateMemoryParserCtxt('<r/>',4)",
         "xmlCreateMemoryParserCtxt",
         [c_char_p, c_int], c_void_p,
         (b"<r/>", 4),
         eq=both_nonnull),
    Case("xmlCtxtGetLastError(NULL)",
         "xmlCtxtGetLastError",
         [c_void_p], c_void_p,
         (None,),
         eq=both_null),
    Case("xmlParseFile('/nonexistent')",
         "xmlParseFile",
         [c_char_p], c_void_p,
         (b"/nonexistent",),
         eq=both_null),
    Case("xmlParserInputBufferCreateMem('<r/>',4,1)",
         "xmlParserInputBufferCreateMem",
         [c_char_p, c_int, c_int], c_void_p,
         (b"<r/>", 4, 1),
         eq=both_nonnull,
         notes="XML_CHAR_ENCODING_NONE = 0; here UTF-8 = 1"),

    Case("xmlPatterncompile('a/b',NULL,0,NULL)",
         "xmlPatterncompile",
         [c_char_p, c_void_p, c_int, c_void_p], c_void_p,
         (b"a/b", None, 0, None),
         eq=both_nonnull,
         notes="compile XPath-like pattern"),
    Case("xmlRegexpCompile('a*b')",
         "xmlRegexpCompile",
         [c_char_p], c_void_p,
         (b"a*b",),
         eq=both_nonnull),

    Case("xmlReaderForFd(-1,NULL,NULL,0)",
         "xmlReaderForFd",
         [c_int, c_char_p, c_char_p, c_int], c_void_p,
         (-1, None, None, 0),
         eq=both_null,
         notes="invalid fd → NULL"),
    Case("xmlReaderForIO(NULL,NULL,NULL,NULL,NULL,0)",
         "xmlReaderForIO",
         [c_void_p, c_void_p, c_void_p, c_char_p, c_char_p, c_int], c_void_p,
         (None, None, None, None, None, 0),
         eq=both_null),
    Case("xmlReaderWalker(NULL)",
         "xmlReaderWalker",
         [c_void_p], c_void_p,
         (None,),
         eq=both_null,
         notes="NULL doc → NULL reader"),

    Case("xmlRelaxNGNewMemParserCtxt(NULL,0)",
         "xmlRelaxNGNewMemParserCtxt",
         [c_char_p, c_int], c_void_p,
         (None, 0),
         eq=both_null,
         notes="NULL buffer → NULL ctxt"),
    Case("xmlSchemaNewMemParserCtxt('<bad/>',6)",
         "xmlSchemaNewMemParserCtxt",
         [c_char_p, c_int], c_void_p,
         (b"<bad/>", 6),
         eq=both_nonnull,
         notes="parser ctxt created even if schema invalid"),

    Case("xmlHashCopy(NULL,NULL)",
         "xmlHashCopy",
         [c_void_p, c_void_p], c_void_p,
         (None, None),
         eq=both_null),

    Case("xmlNoNetExternalEntityLoader (fnptr)",
         "xmlNoNetExternalEntityLoader",
         [c_char_p, c_char_p, c_void_p], c_void_p,
         (b"http://example.com/", None, None),
         eq=both_null,
         notes="net loader refuses URLs → NULL"),

    # Reader getters take a reader; with NULL both libs return NULL.
    Case("xmlTextReaderConstBaseUri(NULL)",
         "xmlTextReaderConstBaseUri",
         [c_void_p], c_void_p,
         (None,), eq=both_null),
    Case("xmlTextReaderConstEncoding(NULL)",
         "xmlTextReaderConstEncoding",
         [c_void_p], c_void_p,
         (None,), eq=both_null),
    Case("xmlTextReaderConstXmlLang(NULL)",
         "xmlTextReaderConstXmlLang",
         [c_void_p], c_void_p,
         (None,), eq=both_null),
    Case("xmlTextReaderConstXmlVersion(NULL)",
         "xmlTextReaderConstXmlVersion",
         [c_void_p], c_void_p,
         (None,), eq=both_null),
    Case("xmlTextReaderCurrentDoc(NULL)",
         "xmlTextReaderCurrentDoc",
         [c_void_p], c_void_p,
         (None,), eq=both_null),
    Case("xmlTextReaderGetAttribute(NULL,'x')",
         "xmlTextReaderGetAttribute",
         [c_void_p, c_char_p], c_void_p,
         (None, b"x"), eq=both_null),
    Case("xmlTextReaderGetAttributeNo(NULL,0)",
         "xmlTextReaderGetAttributeNo",
         [c_void_p, c_int], c_void_p,
         (None, 0), eq=both_null),
    Case("xmlTextReaderGetAttributeNs(NULL,'x','u')",
         "xmlTextReaderGetAttributeNs",
         [c_void_p, c_char_p, c_char_p], c_void_p,
         (None, b"x", b"u"), eq=both_null),
    Case("xmlTextReaderLookupNamespace(NULL,'p')",
         "xmlTextReaderLookupNamespace",
         [c_void_p, c_char_p], c_void_p,
         (None, b"p"), eq=both_null),
    Case("xmlTextReaderPreserve(NULL)",
         "xmlTextReaderPreserve",
         [c_void_p], c_void_p,
         (None,), eq=both_null),
    Case("xmlTextReaderReadInnerXml(NULL)",
         "xmlTextReaderReadInnerXml",
         [c_void_p], c_void_p,
         (None,), eq=both_null),
    Case("xmlTextReaderReadOuterXml(NULL)",
         "xmlTextReaderReadOuterXml",
         [c_void_p], c_void_p,
         (None,), eq=both_null),
]


INT_RETURNING = [
    Case("xmlCharEncInFunc(NULL,NULL,NULL)",
         "xmlCharEncInFunc",
         [c_void_p, c_void_p, c_void_p], c_int,
         (None, None, None),
         eq=both_int_neg,
         notes="invalid args → both return negative error"),
    Case("xmlCharEncOutFunc(NULL,NULL,NULL)",
         "xmlCharEncOutFunc",
         [c_void_p, c_void_p, c_void_p], c_int,
         (None, None, None),
         eq=both_int_neg),

    Case("xmlGetDocCompressMode(NULL)",
         "xmlGetDocCompressMode",
         [c_void_p], c_int,
         (None,),
         eq=both_int_neg,
         notes="NULL doc → -1 per libxml2"),
    Case("xmlKeepBlanksDefault(1) twice (idempotent)",
         "xmlKeepBlanksDefault",
         [c_int], c_int,
         (1,),
         eq=same_int,
         notes="returns previous flag value; should be equal between libs"),

    Case("xmlLoadCatalog('/nonexistent')",
         "xmlLoadCatalog",
         [c_char_p], c_int,
         (b"/nonexistent",),
         eq=same_int,
         notes="libxml2 returns 0 even for missing file (lenient)"),
    Case("xmlParseBalancedChunkMemory(NULL,NULL,NULL,0,'<a/>',NULL)",
         "xmlParseBalancedChunkMemory",
         [c_void_p, c_void_p, c_void_p, c_int, c_char_p, c_void_p], c_int,
         (None, None, None, 0, b"<a/>", None),
         eq=same_int,
         notes="libxml2 returns 0 (success) without a ctxt or list"),
    Case("xmlParseCharEncoding('UTF-8')",
         "xmlParseCharEncoding",
         [c_char_p], c_int,
         (b"UTF-8",),
         eq=same_int,
         notes="returns XML_CHAR_ENCODING_UTF8 = 1"),
    Case("xmlParseDocument(NULL)",
         "xmlParseDocument",
         [c_void_p], c_int,
         (None,),
         eq=both_int_neg,
         notes="NULL ctxt → both return -1"),
    Case("xmlParserInputBufferPush(NULL,0,NULL)",
         "xmlParserInputBufferPush",
         [c_void_p, c_int, c_char_p], c_int,
         (None, 0, None),
         eq=both_int_neg),

    Case("xmlPatternMatch(NULL,NULL)",
         "xmlPatternMatch",
         [c_void_p, c_void_p], c_int,
         (None, None),
         eq=both_int_neg),

    Case("xmlReconciliateNs(NULL,NULL)",
         "xmlReconciliateNs",
         [c_void_p, c_void_p], c_int,
         (None, None),
         eq=both_int_neg,
         notes="NULL doc/node → -1 (or 0 — both libs should agree)"),

    Case("xmlRegexpExec(NULL,NULL)",
         "xmlRegexpExec",
         [c_void_p, c_char_p], c_int,
         (None, None),
         eq=both_int_neg),
    Case("xmlRegexpIsDeterminist(NULL)",
         "xmlRegexpIsDeterminist",
         [c_void_p], c_int,
         (None,),
         eq=both_int_neg),
    Case("xmlRegisterInputCallbacks(NULL,NULL,NULL,NULL)",
         "xmlRegisterInputCallbacks",
         [c_void_p, c_void_p, c_void_p, c_void_p], c_int,
         (None, None, None, None),
         eq=same_int,
         notes="libxml2 returns current slot index (4, after default callbacks)"),

    Case("xmlSaveFile('/dev/null',NULL)",
         "xmlSaveFile",
         [c_char_p, c_void_p], c_int,
         (b"/dev/null", None),
         eq=both_int_neg,
         notes="NULL doc → -1 on both"),
    Case("xmlSaveFormatFile('/dev/null',NULL,0)",
         "xmlSaveFormatFile",
         [c_char_p, c_void_p, c_int], c_int,
         (b"/dev/null", None, 0),
         eq=both_int_neg),
    Case("xmlSaveFormatFileTo(NULL,NULL,NULL,0)",
         "xmlSaveFormatFileTo",
         [c_void_p, c_void_p, c_char_p, c_int], c_int,
         (None, None, None, 0),
         eq=both_int_neg),

    Case("xmlTextConcat(NULL,'x',1)",
         "xmlTextConcat",
         [c_void_p, c_char_p, c_int], c_int,
         (None, b"x", 1),
         eq=both_int_neg,
         notes="NULL node → -1"),

    Case("xmlGcMemSetup(NULL,NULL,NULL,NULL,NULL)",
         "xmlGcMemSetup",
         [c_void_p, c_void_p, c_void_p, c_void_p, c_void_p], c_int,
         (None, None, None, None, None),
         eq=both_int_neg,
         notes="NULL alloc fns → -1"),

    # Reader-state ints with NULL reader → -1.
    Case("xmlTextReaderGetParserColumnNumber(NULL)",
         "xmlTextReaderGetParserColumnNumber",
         [c_void_p], c_int, (None,), eq=same_int,
         notes="libxml2 returns 0 (not -1) for NULL reader"),
    Case("xmlTextReaderGetParserLineNumber(NULL)",
         "xmlTextReaderGetParserLineNumber",
         [c_void_p], c_int, (None,), eq=same_int,
         notes="libxml2 returns 0 (not -1) for NULL reader"),
    Case("xmlTextReaderGetParserProp(NULL,0)",
         "xmlTextReaderGetParserProp",
         [c_void_p, c_int], c_int, (None, 0), eq=both_int_neg),
    Case("xmlTextReaderHasAttributes(NULL)",
         "xmlTextReaderHasAttributes",
         [c_void_p], c_int, (None,), eq=both_int_neg),
    Case("xmlTextReaderIsDefault(NULL)",
         "xmlTextReaderIsDefault",
         [c_void_p], c_int, (None,), eq=both_int_neg),
    Case("xmlTextReaderIsNamespaceDecl(NULL)",
         "xmlTextReaderIsNamespaceDecl",
         [c_void_p], c_int, (None,), eq=both_int_neg),
    Case("xmlTextReaderIsValid(NULL)",
         "xmlTextReaderIsValid",
         [c_void_p], c_int, (None,), eq=both_int_neg),
    Case("xmlTextReaderMoveToAttribute(NULL,'x')",
         "xmlTextReaderMoveToAttribute",
         [c_void_p, c_char_p], c_int, (None, b"x"), eq=both_int_neg),
    Case("xmlTextReaderMoveToAttributeNo(NULL,0)",
         "xmlTextReaderMoveToAttributeNo",
         [c_void_p, c_int], c_int, (None, 0), eq=both_int_neg),
    Case("xmlTextReaderMoveToAttributeNs(NULL,'x','u')",
         "xmlTextReaderMoveToAttributeNs",
         [c_void_p, c_char_p, c_char_p], c_int, (None, b"x", b"u"), eq=both_int_neg),
    Case("xmlTextReaderNext(NULL)",
         "xmlTextReaderNext",
         [c_void_p], c_int, (None,), eq=both_int_neg),
    Case("xmlTextReaderNextSibling(NULL)",
         "xmlTextReaderNextSibling",
         [c_void_p], c_int, (None,), eq=both_int_neg),
    Case("xmlTextReaderPreservePattern(NULL,NULL,NULL)",
         "xmlTextReaderPreservePattern",
         [c_void_p, c_char_p, c_void_p], c_int, (None, None, None), eq=both_int_neg),
    Case("xmlTextReaderQuoteChar(NULL)",
         "xmlTextReaderQuoteChar",
         [c_void_p], c_int, (None,), eq=both_int_neg),
    Case("xmlTextReaderReadAttributeValue(NULL)",
         "xmlTextReaderReadAttributeValue",
         [c_void_p], c_int, (None,), eq=both_int_neg),
    Case("xmlTextReaderRelaxNGSetSchema(NULL,NULL)",
         "xmlTextReaderRelaxNGSetSchema",
         [c_void_p, c_void_p], c_int, (None, None), eq=both_int_neg),
    Case("xmlTextReaderRelaxNGValidate(NULL,NULL)",
         "xmlTextReaderRelaxNGValidate",
         [c_void_p, c_char_p], c_int, (None, None), eq=both_int_neg),
    Case("xmlTextReaderSchemaValidate(NULL,NULL)",
         "xmlTextReaderSchemaValidate",
         [c_void_p, c_char_p], c_int, (None, None), eq=both_int_neg),
    Case("xmlTextReaderSetParserProp(NULL,0,0)",
         "xmlTextReaderSetParserProp",
         [c_void_p, c_int, c_int], c_int, (None, 0, 0), eq=both_int_neg),
    Case("xmlTextReaderSetSchema(NULL,NULL)",
         "xmlTextReaderSetSchema",
         [c_void_p, c_void_p], c_int, (None, None), eq=both_int_neg),
    Case("xmlTextReaderStandalone(NULL)",
         "xmlTextReaderStandalone",
         [c_void_p], c_int, (None,), eq=both_int_neg),
]


LONG_RETURNING = [
    Case("xmlTextReaderByteConsumed(NULL)",
         "xmlTextReaderByteConsumed",
         [c_void_p], c_long, (None,),
         eq=both_int_neg,
         notes="NULL reader → -1"),
]


VOID_RETURNING = [
    # Void functions: verify they don't crash on NULL or trivial args.
    # We can't compare return values; the pass condition is "neither lib
    # raised an exception."  ctypes will surface segfaults as a Python
    # crash, which is a hard fail.
    Case("htmlDocDumpMemory(NULL,&buf,&n)",
         "htmlDocDumpMemory",
         [c_void_p, c_void_p, c_void_p], None,
         (None, None, None),
         eq=lambda a, b: True,
         notes="must not segfault on NULL doc"),
    Case("xmlAttrSerializeTxtContent(NULL,NULL,NULL,NULL)",
         "xmlAttrSerializeTxtContent",
         [c_void_p, c_void_p, c_void_p, c_char_p], None,
         (None, None, None, None),
         eq=lambda a, b: True),
    Case("xmlCheckVersion(20913)",
         "xmlCheckVersion",
         [c_int], None, (20913,),
         eq=lambda a, b: True,
         notes="major version check"),
    Case("xmlCleanupInputCallbacks()",
         "xmlCleanupInputCallbacks",
         [], None, (),
         eq=lambda a, b: True),
    Case("xmlFreePattern(NULL)",
         "xmlFreePattern",
         [c_void_p], None, (None,),
         eq=lambda a, b: True,
         notes="NULL-safe free"),
    Case("xmlFreeProp(NULL)",
         "xmlFreeProp",
         [c_void_p], None, (None,),
         eq=lambda a, b: True),
    Case("xmlInitializeCatalog()",
         "xmlInitializeCatalog",
         [], None, (),
         eq=lambda a, b: True),
    Case("xmlRegFreeRegexp(NULL)",
         "xmlRegFreeRegexp",
         [c_void_p], None, (None,),
         eq=lambda a, b: True),
    Case("xmlRegisterDefaultInputCallbacks()",
         "xmlRegisterDefaultInputCallbacks",
         [], None, (),
         eq=lambda a, b: True),
    Case("xmlRegisterDefaultOutputCallbacks()",
         "xmlRegisterDefaultOutputCallbacks",
         [], None, (),
         eq=lambda a, b: True),
    Case("xmlSchemaSetParserErrors(NULL,NULL,NULL,NULL)",
         "xmlSchemaSetParserErrors",
         [c_void_p, c_void_p, c_void_p, c_void_p], None,
         (None, None, None, None),
         eq=lambda a, b: True,
         notes="NULL ctxt — both libs no-op"),
    Case("xmlSchemaSetValidErrors(NULL,NULL,NULL,NULL)",
         "xmlSchemaSetValidErrors",
         [c_void_p, c_void_p, c_void_p, c_void_p], None,
         (None, None, None, None),
         eq=lambda a, b: True),
    Case("xmlSetDocCompressMode(NULL,5)",
         "xmlSetDocCompressMode",
         [c_void_p, c_int], None, (None, 5),
         eq=lambda a, b: True,
         notes="NULL-safe setter"),
    Case("xmlSetListDoc(NULL,NULL)",
         "xmlSetListDoc",
         [c_void_p, c_void_p], None, (None, None),
         eq=lambda a, b: True),
    Case("xmlSetTreeDoc(NULL,NULL)",
         "xmlSetTreeDoc",
         [c_void_p, c_void_p], None, (None, None),
         eq=lambda a, b: True),
    Case("xmlTextReaderGetErrorHandler(NULL,NULL,NULL)",
         "xmlTextReaderGetErrorHandler",
         [c_void_p, c_void_p, c_void_p], None,
         (None, None, None),
         eq=lambda a, b: True),
]


CATEGORIES = [
    ("Pointer-returning stubs", PTR_RETURNING),
    ("Int-returning stubs",     INT_RETURNING),
    ("Long-returning stubs",    LONG_RETURNING),
    ("Void-returning stubs",    VOID_RETURNING),
]


def main():
    print(f"{BOLD}perl_stub_spec — parity vs system libxml2{RESET}")
    print(f"  system:  {SYS_LIB}")
    print(f"  ours:    {OUR_LIB}")
    print(f"  Today's expectation: most rows {RED}RED{RESET} until we "
          f"real-implement the corresponding stub.")
    print(f"  Track at {DIM}crates/compat/src/perl_stubs.rs{RESET}.")
    total = 0
    by_status = {"pass": 0, "fail": 0, "skip": 0, "missing": 0, "exc": 0}
    for title, cases in CATEGORIES:
        print(f"\n{BOLD}══ {title} ({len(cases)}) ══{RESET}")
        for c in cases:
            r = run(c)
            print(fmt(r))
            by_status[r["status"]] = by_status.get(r["status"], 0) + 1
            total += 1

    print(f"\n{BOLD}── summary ──{RESET}")
    print(f"  total:    {total}")
    print(f"  {GREEN}passing{RESET}:  {by_status['pass']}")
    print(f"  {RED}failing{RESET}:  {by_status['fail']}")
    print(f"  {DIM}skipped{RESET}:  {by_status['skip']}")
    print(f"  {RED}missing{RESET}:  {by_status['missing']}")
    print(f"  {RED}exc{RESET}:      {by_status['exc']}")

    progress = by_status["pass"] / total if total else 0
    print(f"\n  progress: {by_status['pass']}/{total} = {progress * 100:.0f}%")
    if by_status["fail"] == 0 and by_status["missing"] == 0 and by_status["exc"] == 0:
        print(f"  {GREEN}all spec rows green — perl_stubs.rs can be deleted.{RESET}")
        sys.exit(0)
    sys.exit(1)


if __name__ == "__main__":
    main()
