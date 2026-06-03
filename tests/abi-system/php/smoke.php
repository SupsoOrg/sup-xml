<?php
// Smoke test: drive PHP's libxml2-based XML extensions against
// whatever libxml2 the PHP binary was linked to.  Run twice — once
// against the system/Homebrew libxml2 (control), once against our
// shim (the real test).
//
// Output format mirrors lxml/smoke.py, nokogiri/smoke.rb, perl/smoke.pl.

$results = [];

function check(string $name, callable $body): void {
    global $results;
    $ok = false;
    $detail = null;
    try {
        $ok = (bool) $body();
    } catch (\Throwable $e) {
        $detail = "EXCEPTION: " . get_class($e) . ": " . $e->getMessage();
    }
    $results[] = [$name, $ok, $detail];
}

// Silence libxml's default warning chatter; we surface failures via exceptions.
libxml_use_internal_errors(true);

// ── basic parse / inspect ────────────────────────────────────────────────

check("tiny_parse", function () {
    $d = new DOMDocument();
    $d->loadXML('<r/>');
    return $d->documentElement->nodeName === 'r';
});

check("text_content", function () {
    $d = new DOMDocument();
    $d->loadXML('<r>hello</r>');
    return $d->documentElement->textContent === 'hello';
});

check("attributes", function () {
    $d = new DOMDocument();
    $d->loadXML('<r a="1" b="2"/>');
    return $d->documentElement->getAttribute('a') === '1'
        && $d->documentElement->getAttribute('b') === '2';
});

check("children_iter", function () {
    $d = new DOMDocument();
    $d->loadXML('<r><a/><b/><c/></r>');
    $names = [];
    foreach ($d->documentElement->childNodes as $c) {
        if ($c->nodeType === XML_ELEMENT_NODE) {
            $names[] = $c->nodeName;
        }
    }
    return implode(',', $names) === 'a,b,c';
});

check("mixed_content", function () {
    $d = new DOMDocument();
    $d->loadXML('<r>foo<b>bar</b>baz</r>');
    $texts = 0;
    foreach ($d->documentElement->childNodes as $c) {
        if ($c->nodeType === XML_TEXT_NODE) $texts++;
    }
    return $texts === 2;
});

check("default_namespace", function () {
    $d = new DOMDocument();
    $d->loadXML('<r xmlns="urn:demo"/>');
    return $d->documentElement->namespaceURI === 'urn:demo';
});

check("prefixed_namespace", function () {
    $d = new DOMDocument();
    $d->loadXML('<x:r xmlns:x="urn:demo"/>');
    return $d->documentElement->prefix === 'x';
});

check("round_trip_string", function () {
    $d = new DOMDocument();
    $d->loadXML('<r a="1"><c/></r>');
    $out = $d->saveXML($d->documentElement);
    return strpos($out, 'a="1"') !== false;
});

check("malformed_raises", function () {
    $d = new DOMDocument();
    libxml_clear_errors();
    $loaded = @$d->loadXML('<r><unclosed>');
    return $loaded === false;
});

// ── XPath ────────────────────────────────────────────────────────────────

check("xpath_simple", function () {
    $d = new DOMDocument();
    $d->loadXML('<catalog><book/><book/></catalog>');
    $xp = new DOMXPath($d);
    return (int) $xp->evaluate('count(/catalog/book)') === 2;
});

check("xpath_attribute", function () {
    $d = new DOMDocument();
    $d->loadXML('<r><a id="x"/><a id="y"/></r>');
    $xp = new DOMXPath($d);
    $nodes = $xp->query('//a/@id');
    $vals = [];
    foreach ($nodes as $a) $vals[] = $a->value;
    return implode(',', $vals) === 'x,y';
});

// ── mutation ─────────────────────────────────────────────────────────────

check("set_attribute", function () {
    $d = new DOMDocument();
    $d->loadXML('<r/>');
    $d->documentElement->setAttribute('k', 'v');
    return $d->documentElement->getAttribute('k') === 'v';
});

check("append_subelement", function () {
    $d = new DOMDocument();
    $d->loadXML('<r/>');
    $a = $d->createElement('a');
    $d->documentElement->appendChild($a);
    return $d->documentElement->firstChild->nodeName === 'a';
});

check("remove_child", function () {
    $d = new DOMDocument();
    $d->loadXML('<r><a/><b/></r>');
    $first = $d->documentElement->firstChild;
    $d->documentElement->removeChild($first);
    return $d->documentElement->firstChild->nodeName === 'b';
});

// ── XSD validation ───────────────────────────────────────────────────────

$XSD_SRC = <<<XSD
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:demo" xmlns="urn:demo">
  <xs:element name="port" type="xs:int"/>
</xs:schema>
XSD;

check("xsd_valid_doc", function () use ($XSD_SRC) {
    $d = new DOMDocument();
    $d->loadXML('<port xmlns="urn:demo">8080</port>');
    return $d->schemaValidateSource($XSD_SRC);
});

check("xsd_invalid_doc", function () use ($XSD_SRC) {
    $d = new DOMDocument();
    $d->loadXML('<port xmlns="urn:demo">not-a-number</port>');
    libxml_clear_errors();
    return !$d->schemaValidateSource($XSD_SRC);
});

// ── SimpleXML round-trip ────────────────────────────────────────────────

check("simplexml_basic", function () {
    $sx = simplexml_load_string('<r><a>1</a><a>2</a></r>');
    if (!$sx) return false;
    return (string) $sx->a[0] === '1' && (string) $sx->a[1] === '2';
});

// ── HTML parsing ─────────────────────────────────────────────────────────

check("html_basic_lifecycle", function () {
    $d = new DOMDocument();
    libxml_clear_errors();
    @$d->loadHTML('<html><body><p>hi</p></body></html>');
    $xp = new DOMXPath($d);
    $p = $xp->query('//p')->item(0);
    return $p && trim($p->textContent) === 'hi';
});

// ── XMLWriter (libxml2 xmlTextWriter API) ───────────────────────────────

check("xmlwriter_basic", function () {
    $w = new XMLWriter();
    $w->openMemory();
    $w->startDocument('1.0', 'UTF-8');
    $w->startElement('catalog');
    $w->writeAttribute('count', '2');
    $w->writeElement('book', 'Dune');
    $w->writeElement('book', 'Foundation');
    $w->endElement();
    $w->endDocument();
    $out = $w->outputMemory();
    return strpos($out, '<?xml') !== false
        && strpos($out, '<catalog count="2">') !== false
        && strpos($out, '<book>Dune</book>') !== false
        && strpos($out, '</catalog>') !== false;
});

check("xmlwriter_piecewise_attribute", function () {
    $w = new XMLWriter();
    $w->openMemory();
    $w->startElement('r');
    $w->startAttribute('greeting');
    $w->text('hello');
    $w->text(' world');
    $w->endAttribute();
    $w->endElement();
    $out = $w->outputMemory();
    return strpos($out, '<r greeting="hello world"/>') !== false;
});

check("xmlwriter_cdata_comment_pi", function () {
    $w = new XMLWriter();
    $w->openMemory();
    $w->startElement('r');
    $w->writeCdata('a < b');
    $w->writeComment('note');
    $w->writePI('php', 'echo 1');
    $w->endElement();
    $out = $w->outputMemory();
    return strpos($out, '<![CDATA[a < b]]>') !== false
        && strpos($out, '<!--note-->') !== false
        && strpos($out, '<?php echo 1?>') !== false;
});

// ── summary ──────────────────────────────────────────────────────────────

$pass = 0; $fail = 0;
foreach ($results as [$name, $ok, $detail]) {
    if ($ok) {
        $pass++;
        echo "PASS $name\n";
    } else {
        $fail++;
        $msg = $detail !== null ? " ($detail)" : "";
        echo "FAIL $name$msg\n";
    }
}
$total = $pass + $fail;
echo "\nsummary: $pass passed, $fail failed of $total total\n";
exit($fail === 0 ? 0 : 1);
