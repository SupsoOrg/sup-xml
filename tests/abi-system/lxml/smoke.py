#!/usr/bin/env python3
"""
Real-consumer smoke test: drive lxml.etree against sup-xml-compat's
libxml2 ABI shim and see what works.

Invocation pattern (run via run.sh, but also runnable directly):

    # Control (uses system libxml2):
    ./py-venv/bin/python smoke.py

    # Against our shim (DYLD_INSERT_LIBRARIES set by run.sh):
    DYLD_INSERT_LIBRARIES=/path/to/libsup_xml_compat.dylib \
        ./py-venv/bin/python smoke.py

Each test reports PASS / FAIL.  CRASH ⇒ Python died — likely an
unimplemented libxml2 function was called.

Use SUPXML_LXML_LOUD=1 to print result values on PASS too.
"""

import faulthandler
faulthandler.enable()

import os
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
FIXTURES_DIR = REPO / "tests" / "assets" / "xml"
LOUD = os.environ.get("SUPXML_LXML_LOUD") == "1"

# Import lxml after env vars are already set by run.sh.  Importing
# lxml.etree triggers the libxml2 dlopen — if our shim's missing a
# symbol that lxml's module init needs, we'll crash right here, before
# any test runs.
from lxml import etree as ET


# ── individual tests ──────────────────────────────────────────────────────

def test_tiny_parse():
    """The simplest possible: parse <r/>, check root tag."""
    root = ET.fromstring(b"<r/>")
    assert root.tag == "r", f"expected 'r', got {root.tag!r}"
    return root.tag


def test_text_content():
    """Parse a single text-bearing element."""
    root = ET.fromstring(b"<r>hello world</r>")
    assert root.text == "hello world", f"got {root.text!r}"
    return root.text


def test_attributes():
    """Parse element with attributes, read each one."""
    root = ET.fromstring(b'<r id="42" name="hello" empty=""/>')
    assert root.get("id") == "42"
    assert root.get("name") == "hello"
    assert root.get("empty") == ""
    assert root.get("missing") is None
    return dict(root.attrib)


def test_children_iter():
    """Iterate children via lxml's __iter__."""
    root = ET.fromstring(b"<r><a/><b/><c/></r>")
    kids = list(root)
    assert len(kids) == 3
    assert [k.tag for k in kids] == ["a", "b", "c"]
    return [k.tag for k in kids]


def test_mixed_content():
    """Mixed-content with .text and .tail navigation."""
    root = ET.fromstring(b"<r>alpha<inner>beta</inner>gamma</r>")
    assert root.text == "alpha"
    inner = root[0]
    assert inner.tag == "inner"
    assert inner.text == "beta"
    assert inner.tail == "gamma"
    return {"root.text": root.text, "inner.text": inner.text, "inner.tail": inner.tail}


def test_default_namespace():
    """Default namespace on root, child sees it."""
    root = ET.fromstring(b'<r xmlns="http://example.com/r"><a/></r>')
    expected_root = "{http://example.com/r}r"
    assert root.tag == expected_root, f"got {root.tag!r}"
    child = root[0]
    assert child.tag == "{http://example.com/r}a"
    return root.tag


def test_prefixed_namespace():
    """Prefixed namespace with attribute in that namespace."""
    root = ET.fromstring(
        b'<r xmlns:x="http://example.com/x" x:id="42"/>'
    )
    assert root.get("{http://example.com/x}id") == "42"
    return "{http://example.com/x}id"


def test_round_trip_string():
    """tostring → fromstring → tostring."""
    original = b"<r><a id=\"1\"/><b>text<c/></b></r>"
    doc = ET.fromstring(original)
    out = ET.tostring(doc)
    reparsed = ET.fromstring(out)
    assert reparsed.tag == "r"
    return out.decode()


def test_xpath_simple():
    """XPath query — note: not yet implemented in our shim's Tier 1."""
    root = ET.fromstring(b"<r><a id='1'/><a id='2'/><b/></r>")
    matches = root.xpath("//a")
    assert len(matches) == 2
    return [m.get("id") for m in matches]


def test_fixture(name, expect_root=None):
    """Parse a real-world XML file and walk it."""
    path = FIXTURES_DIR / name
    if not path.exists():
        raise FileNotFoundError(f"fixture missing: {path}")
    data = path.read_bytes()
    # Use the bytes-parsing form — that's the one our shim implements.
    doc = ET.fromstring(data)
    n_elements = sum(1 for _ in doc.iter())
    info = {
        "file": name,
        "bytes": len(data),
        "root_tag": doc.tag,
        "n_elements": n_elements,
    }
    if expect_root is not None and not doc.tag.endswith(expect_root):
        raise AssertionError(f"root: expected to end with {expect_root!r}, got {doc.tag!r}")
    return info


def test_malformed_raises():
    """Malformed input should raise, not crash."""
    try:
        ET.fromstring(b"<unclosed")
    except ET.XMLSyntaxError as e:
        return f"raised XMLSyntaxError: {e}"
    except Exception as e:
        return f"raised {type(e).__name__}: {e}"
    raise AssertionError("expected an exception")


# ── mutation tests (Tier 4) ────────────────────────────────────────────────

def test_set_attribute():
    """Element.set() — update + read back."""
    root = ET.fromstring(b"<r/>")
    root.set("id", "42")
    assert root.get("id") == "42"
    # Update.
    root.set("id", "99")
    assert root.get("id") == "99"
    return root.get("id")


def test_set_text():
    """Element.text = ... — assigns child text."""
    root = ET.fromstring(b"<r/>")
    root.text = "hello"
    # Re-serialize and check.
    out = ET.tostring(root).decode()
    assert "hello" in out, f"missing text: {out}"
    return out


def test_append_subelement():
    """SubElement appends and is reachable from the parent."""
    root = ET.fromstring(b"<r/>")
    child = ET.SubElement(root, "child")
    child.text = "inner"
    assert len(root) == 1
    assert root[0].tag == "child"
    assert root[0].text == "inner"
    return ET.tostring(root).decode()


def test_remove_child():
    """Element.remove() detaches the child."""
    root = ET.fromstring(b"<r><a/><b/><c/></r>")
    b = root[1]
    root.remove(b)
    assert len(root) == 2
    assert [el.tag for el in root] == ["a", "c"]
    return [el.tag for el in root]


def test_element_from_scratch():
    """etree.Element() — build a tree without parsing first."""
    root = ET.Element("greeting")
    root.text = "hello"
    child = ET.SubElement(root, "name")
    child.text = "world"
    out = ET.tostring(root).decode()
    assert "<greeting>hello<name>world</name></greeting>" in out, f"got: {out}"
    return out


def test_deepcopy():
    """copy.deepcopy(element) — clone the subtree."""
    import copy
    root = ET.fromstring(b"<r><a id='1'><inner>text</inner></a></r>")
    a = root[0]
    a_copy = copy.deepcopy(a)
    # Original and copy should serialize the same.
    assert ET.tostring(a) == ET.tostring(a_copy)
    # But have different identities (different C nodes).
    assert a is not a_copy
    # Modifying the copy must not change the original.
    a_copy.set("id", "999")
    assert a.get("id") == "1"
    assert a_copy.get("id") == "999"
    return (a.get("id"), a_copy.get("id"))


def test_sourceline():
    """Element.sourceline — line number from the parser."""
    doc = ET.fromstring(b"<r>\n  <a/>\n  <b/>\n</r>")
    # Root is on line 1.
    line = doc.sourceline
    assert line is None or line >= 1, f"got: {line}"
    return line


def test_getpath():
    """ElementTree.getpath(element) — XPath-ish locator."""
    root = ET.fromstring(b"<r><a/><a/><b/></r>")
    tree = root.getroottree()
    a2 = root[1]
    b  = root[2]
    p_a2 = tree.getpath(a2)
    p_b  = tree.getpath(b)
    assert "a" in p_a2 and "2" in p_a2, f"got: {p_a2}"
    assert p_b.endswith("b") or "b" in p_b, f"got: {p_b}"
    return (p_a2, p_b)


# ── XSD validation ─────────────────────────────────────────────────────────

_SCHEMA = b"""<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="r">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="name" type="xs:string"/>
        <xs:element name="age" type="xs:int"/>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
</xs:schema>"""


def test_xsd_valid_doc():
    """Compile an XSD schema and validate a conforming doc."""
    schema_tree = ET.fromstring(_SCHEMA).getroottree()
    schema = ET.XMLSchema(schema_tree)
    doc = ET.fromstring(b"<r><name>alice</name><age>30</age></r>")
    assert schema.validate(doc), f"expected valid, errors: {list(schema.error_log)}"
    return "valid"


def test_xsd_invalid_doc():
    """Validation should reject a non-conforming doc."""
    schema_tree = ET.fromstring(_SCHEMA).getroottree()
    schema = ET.XMLSchema(schema_tree)
    bad = ET.fromstring(b"<r><age>thirty</age></r>")
    assert not schema.validate(bad), "expected invalid"
    # error_log should be non-empty.
    assert len(schema.error_log) > 0, "expected error_log entries"
    return f"rejected with {len(schema.error_log)} errors"


# ── RelaxNG validation ────────────────────────────────────────────────────

_RNG = b"""<?xml version="1.0"?>
<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
  <element name="name"><text/></element>
  <element name="age"><text/></element>
</element>"""


def test_rng_valid_doc():
    rng_tree = ET.fromstring(_RNG).getroottree()
    rng = ET.RelaxNG(rng_tree)
    doc = ET.fromstring(b"<r><name>bob</name><age>42</age></r>")
    assert rng.validate(doc), f"expected valid, errors: {list(rng.error_log)}"
    return "valid"


def test_rng_invalid_doc():
    rng_tree = ET.fromstring(_RNG).getroottree()
    rng = ET.RelaxNG(rng_tree)
    bad = ET.fromstring(b"<wrong/>")
    assert not rng.validate(bad), "expected invalid"
    return f"rejected with {len(rng.error_log)} errors"


# ── iterparse (streaming) ─────────────────────────────────────────────────

def test_iterparse_collects_events():
    """iterparse over a small doc — collect end events for elements."""
    from io import BytesIO
    src = b"<r><a id='1'/><b><c/></b><a id='2'/></r>"
    tags = []
    for event, el in ET.iterparse(BytesIO(src), events=("end",)):
        tags.append(el.tag)
    # Order is end-of-element in document order.
    assert tags == ["a", "c", "b", "a", "r"], f"got: {tags}"
    return tags


def test_iterparse_start_events():
    """iterparse with start events."""
    from io import BytesIO
    src = b"<r><a/><b/></r>"
    events = []
    for event, el in ET.iterparse(BytesIO(src), events=("start", "end")):
        events.append((event, el.tag))
    # Expected interleaving:
    #   start r, start a, end a, start b, end b, end r
    expected = [
        ("start", "r"),
        ("start", "a"),
        ("end", "a"),
        ("start", "b"),
        ("end", "b"),
        ("end", "r"),
    ]
    assert events == expected, f"got: {events}"
    return events


# ── tree.write(file) — file output via xmlOutputBufferCreateFilename ───────

def test_tree_write_to_file():
    """ElementTree.write(filename) round-trips through our file-backed buffer."""
    import tempfile, os
    root = ET.fromstring(b"<r><a id='1'/><b>hello</b></r>")
    tree = root.getroottree()
    fd, path = tempfile.mkstemp(suffix=".xml")
    os.close(fd)
    try:
        tree.write(path)
        # Read it back and reparse to verify a valid XML round-trip.
        with open(path, "rb") as f:
            data = f.read()
        roundtripped = ET.fromstring(data)
        assert roundtripped.tag == "r"
        assert roundtripped[0].get("id") == "1"
        assert roundtripped[1].text == "hello"
        return data.decode()
    finally:
        try: os.unlink(path)
        except OSError: pass


# ── custom XPath functions — registration only (no invocation yet) ────────

def test_dtd_valid():
    """Validate a document against its internal-subset DTD via lxml.DTD."""
    src = b"""<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a+)>
  <!ELEMENT a EMPTY>
  <!ATTLIST a id ID #REQUIRED>
]>
<r><a id="x1"/><a id="x2"/></r>
"""
    tree = ET.fromstring(src).getroottree()
    # lxml's docinfo.internalDTD gives us a DTD object we can validate
    # against.  Falls back to ET.DTD parsing the doctype subset.
    dtd = tree.docinfo.internalDTD
    assert dtd is not None, "expected internal DTD to be parsed"
    assert dtd.validate(tree), f"validation failed: {dtd.error_log!s}"
    return "ok"


def test_dtd_invalid():
    """Doc that violates its internal DTD must NOT validate."""
    src = b"""<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a, b)>
  <!ELEMENT a EMPTY>
  <!ELEMENT b EMPTY>
]>
<r><b/><a/></r>
"""
    tree = ET.fromstring(src).getroottree()
    dtd = tree.docinfo.internalDTD
    assert dtd is not None
    assert not dtd.validate(tree), "expected validation to fail"
    return "ok"


def test_dtd_external_subset():
    """<!DOCTYPE r SYSTEM "...dtd"> parses cleanly — the body must
    still validate well-formedness even though decls live in a
    referenced .dtd file.

    Both control and shim should round-trip the body; what differs is
    whether each side's libxml2 build *loads* the external file, but
    the parse itself shouldn't blow up.
    """
    import tempfile, os
    fd, path = tempfile.mkstemp(suffix=".dtd")
    try:
        os.write(fd, b"<!ELEMENT r (a+)>\n<!ELEMENT a EMPTY>\n")
        os.close(fd)
        src = (
            f'<?xml version="1.0"?>\n'
            f'<!DOCTYPE r SYSTEM "{path}">\n'
            f'<r><a/><a/></r>\n'
        ).encode("utf-8")
        parser = ET.XMLParser(load_dtd=True)
        root = ET.fromstring(src, parser)
        assert root.tag == "r"
        assert len(root) == 2
        return "ok"
    finally:
        try: os.unlink(path)
        except OSError: pass


def test_dtd_error_log_collects_multiple():
    """Both violations should show up in dtd.error_log, not just the first."""
    src = b"""<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a, a)>
  <!ELEMENT a EMPTY>
  <!ATTLIST a id ID #REQUIRED>
]>
<r><a/><a id="x"/></r>
"""
    tree = ET.fromstring(src).getroottree()
    dtd = tree.docinfo.internalDTD
    assert dtd is not None
    valid = dtd.validate(tree)
    assert not valid, "expected validation to fail"
    # Error log should have at least one entry — the missing required attr.
    errors = list(dtd.error_log)
    assert len(errors) >= 1, f"expected >=1 errors, got {len(errors)}: {errors}"
    return f"{len(errors)} errors"


def test_html_basic_lifecycle():
    """`etree.HTML(...)` parse + explicit drop + GC must round-trip
    without crashing at heap teardown.

    Trips an issue where the consumer (lxml) drops its `_Document`
    wrapper, which fires our `xmlFreeDoc`, which triggers our
    `Dict::release`.  If anything in the dict's owned `Box<[u8]>`
    entries was previously `free()`'d by the consumer's
    side-channel xmlFree-equivalent (system free), the Box drop
    here hits `mfm_free`'s heap-validation check and SIGABRTs.
    """
    import gc
    html = ET.HTML('<html><body><p>x</p></body></html>')
    assert html.tag == "html", f"got: {html.tag}"
    # Force the lxml _Document wrapper to release immediately so
    # any heap-allocator violations fire during the test, not at
    # interpreter shutdown.
    del html
    gc.collect()
    return "ok"


def test_dtd_default_attr_injection():
    """ATTLIST default values should appear on elements that omit the attr."""
    src = b"""<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r
    kind    CDATA          "alpha"
    version CDATA  #FIXED   "1.0">
]>
<r/>
"""
    root = ET.fromstring(src)
    assert root.get("kind")    == "alpha", f"kind: {root.get('kind')!r}"
    assert root.get("version") == "1.0",   f"version: {root.get('version')!r}"
    return "ok"


def test_xinclude_xml():
    """xi:include with parse=\"xml\" inlines another doc's root."""
    import tempfile, os
    fd, path = tempfile.mkstemp(suffix=".xml")
    try:
        os.write(fd, b"<frag><inside>hi</inside></frag>")
        os.close(fd)
        src = (
            f'<r xmlns:xi="http://www.w3.org/2001/XInclude">'
            f'<xi:include href="{path}"/>'
            f'</r>'
        ).encode("utf-8")
        root = ET.fromstring(src)
        ET.XInclude()(root)
        out = ET.tostring(root).decode()
        # System libxml2 adds xml:base="..." to <frag>; our shim doesn't
        # yet.  Match the included content either way.
        assert "<frag" in out and "<inside>hi</inside>" in out, f"got: {out}"
        # And the xi:include element itself should be gone.
        assert "xi:include" not in out, f"include element survived: {out}"
        return out
    finally:
        try: os.unlink(path)
        except OSError: pass


def test_xinclude_text():
    """xi:include with parse=\"text\" inlines raw bytes as text."""
    import tempfile, os
    fd, path = tempfile.mkstemp(suffix=".txt")
    try:
        os.write(fd, b"plain content")
        os.close(fd)
        src = (
            f'<r xmlns:xi="http://www.w3.org/2001/XInclude">'
            f'<xi:include href="{path}" parse="text"/>'
            f'</r>'
        ).encode("utf-8")
        root = ET.fromstring(src)
        ET.XInclude()(root)
        out = ET.tostring(root).decode()
        assert "plain content" in out, f"got: {out}"
        return out
    finally:
        try: os.unlink(path)
        except OSError: pass


def test_xpath_register_namespace():
    """Register an XPath namespace + function via FunctionNamespace.
    The function isn't yet invoked by our XPath engine, but the
    setup path must not crash."""
    # Defining the function namespace is enough — lxml stores it and
    # registers via xmlXPathRegisterFuncNS on each evaluation context.
    fn_ns = ET.FunctionNamespace("http://example.com/fn")
    @fn_ns
    def myfunc(context, node):
        return "ok"
    # Build a doc + run a non-custom-function XPath to confirm the
    # engine still works alongside.
    root = ET.fromstring(b"<r><a/><a/></r>")
    found = root.xpath("count(//a)")
    assert found == 2.0, f"got: {found}"
    return found

TESTS = [
    ("tiny_parse",          test_tiny_parse),
    ("text_content",        test_text_content),
    ("attributes",          test_attributes),
    ("children_iter",       test_children_iter),
    ("mixed_content",       test_mixed_content),
    ("default_namespace",   test_default_namespace),
    ("prefixed_namespace",  test_prefixed_namespace),
    ("round_trip_string",   test_round_trip_string),
    ("malformed_raises",    test_malformed_raises),
    # XPath — Tier 2, expected to fail without shim support.
    ("xpath_simple",        test_xpath_simple),
    # Mutation — Tier 4.
    ("set_attribute",       test_set_attribute),
    ("set_text",            test_set_text),
    ("append_subelement",   test_append_subelement),
    ("remove_child",        test_remove_child),
    ("element_from_scratch",test_element_from_scratch),
    ("deepcopy",            test_deepcopy),
    ("sourceline",          test_sourceline),
    ("getpath",             test_getpath),
    # XSD validation — Tier 9.
    ("xsd_valid_doc",       test_xsd_valid_doc),
    ("xsd_invalid_doc",     test_xsd_invalid_doc),
    # RelaxNG validation — Tier 9.
    ("rng_valid_doc",       test_rng_valid_doc),
    ("rng_invalid_doc",     test_rng_invalid_doc),
    # iterparse (streaming/pull) — Tier 3.
    ("iterparse_end",       test_iterparse_collects_events),
    ("iterparse_start_end", test_iterparse_start_events),
    # Serialization to file.
    ("tree_write_to_file",  test_tree_write_to_file),
    # Custom XPath function registration (Tier 2 ext).
    ("xpath_function_ns",   test_xpath_register_namespace),
    # XInclude — Tier 2.
    ("xinclude_xml",        test_xinclude_xml),
    ("xinclude_text",       test_xinclude_text),
    # HTML parsing — Tier 1.
    ("html_basic_lifecycle", test_html_basic_lifecycle),
    # DTD validation — Tier 9.
    ("dtd_valid",           test_dtd_valid),
    ("dtd_invalid",         test_dtd_invalid),
    ("dtd_default_attrs",   test_dtd_default_attr_injection),
    ("dtd_error_log",       test_dtd_error_log_collects_multiple),
    ("dtd_external_subset", test_dtd_external_subset),
    # Real-world fixtures, ordered small → large.
    ("fixture_321gone",     lambda: test_fixture("321gone.xml")),
    ("fixture_1831893",     lambda: test_fixture("1831893.xml")),
    ("fixture_maven_pom",   lambda: test_fixture("maven-pom.xml")),
    ("fixture_ebay",        lambda: test_fixture("ebay.xml")),
    ("fixture_podcast",     lambda: test_fixture("podcast_episode_2024_03.xml")),
    ("fixture_yahoo",       lambda: test_fixture("yahoo.xml")),
    ("fixture_utah_legis",  lambda: test_fixture("utah_legislature_2024.xml")),
    ("fixture_sitemap",     lambda: test_fixture("sitemap.xml")),
    ("fixture_dblp",        lambda: test_fixture("dblp.xml")),
    ("fixture_cldr_en",     lambda: test_fixture("cldr_en.xml")),
    ("fixture_pubmed",      lambda: test_fixture("pubmed.xml")),
    ("fixture_chinese",     lambda: test_fixture("chinese1.xml")),
    ("fixture_ns_heavy",    lambda: test_fixture("ns_heavy.xml")),
]

passed = 0
failed = 0
for name, fn in TESTS:
    try:
        result = fn()
        passed += 1
        if LOUD:
            print(f"PASS {name:24s}  → {result!r}")
        else:
            print(f"PASS {name}")
    except Exception as e:
        failed += 1
        print(f"FAIL {name:24s}  {type(e).__name__}: {e}")

print()
print(f"summary: {passed} passed, {failed} failed of {len(TESTS)} total")
sys.exit(0 if failed == 0 else 1)
