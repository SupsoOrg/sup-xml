#!/usr/bin/env perl
# Smoke test: drive XML::LibXML against the libxml2 it links to.
# Run twice — once via system libxml2 (control), once via our shim
# (the real test).  Output format mirrors lxml/smoke.py and
# nokogiri/smoke.rb.

use strict;
use warnings;
use XML::LibXML;

my @results;

sub check {
    my ($name, $sub) = @_;
    my $ok = 0;
    my $detail;
    my $rv = eval { $sub->() };
    if ($@) {
        $detail = "EXCEPTION: $@";
        $detail =~ s/\s+at\s+\S+\s+line\s+\d+\.?\s*$//;
    } else {
        $ok = $rv ? 1 : 0;
    }
    push @results, [ $name, $ok, $detail ];
}

# ── basic parse / inspect ────────────────────────────────────────────────

check "tiny_parse", sub {
    my $doc = XML::LibXML->load_xml(string => "<r/>");
    $doc->documentElement->nodeName eq "r";
};

check "text_content", sub {
    my $doc = XML::LibXML->load_xml(string => "<r>hello</r>");
    $doc->documentElement->textContent eq "hello";
};

check "attributes", sub {
    my $doc = XML::LibXML->load_xml(string => qq{<r a="1" b="2"/>});
    $doc->documentElement->getAttribute("a") eq "1"
      && $doc->documentElement->getAttribute("b") eq "2";
};

check "children_iter", sub {
    my $doc = XML::LibXML->load_xml(string => "<r><a/><b/><c/></r>");
    my @names = map { $_->nodeName } $doc->documentElement->childNodes;
    "@names" eq "a b c";
};

check "mixed_content", sub {
    my $doc = XML::LibXML->load_xml(string => "<r>foo<b>bar</b>baz</r>");
    my @txt = grep { $_->nodeType == XML_TEXT_NODE } $doc->documentElement->childNodes;
    scalar(@txt) == 2;
};

check "default_namespace", sub {
    my $doc = XML::LibXML->load_xml(string => qq{<r xmlns="urn:demo"/>});
    $doc->documentElement->namespaceURI eq "urn:demo";
};

check "prefixed_namespace", sub {
    my $doc = XML::LibXML->load_xml(string => qq{<x:r xmlns:x="urn:demo"/>});
    $doc->documentElement->prefix eq "x";
};

check "round_trip_string", sub {
    my $doc = XML::LibXML->load_xml(string => qq{<r a="1"><c/></r>});
    $doc->documentElement->toString =~ /a="1"/;
};

check "malformed_raises", sub {
    eval {
        XML::LibXML->load_xml(string => "<r><unclosed>");
    };
    $@ ? 1 : 0;
};

# ── XPath ────────────────────────────────────────────────────────────────

check "xpath_simple", sub {
    my $doc = XML::LibXML->load_xml(string => "<catalog><book/><book/></catalog>");
    $doc->findvalue("count(/catalog/book)") == 2;
};

check "xpath_attribute", sub {
    my $doc = XML::LibXML->load_xml(string => qq{<r><a id="x"/><a id="y"/></r>});
    my @ids = map { $_->getValue } $doc->findnodes("//a/\@id");
    "@ids" eq "x y";
};

# ── mutation ─────────────────────────────────────────────────────────────

check "set_attribute", sub {
    my $doc = XML::LibXML->load_xml(string => "<r/>");
    $doc->documentElement->setAttribute("k", "v");
    $doc->documentElement->getAttribute("k") eq "v";
};

check "append_subelement", sub {
    my $doc = XML::LibXML->load_xml(string => "<r/>");
    my $child = $doc->createElement("a");
    $doc->documentElement->appendChild($child);
    ($doc->documentElement->childNodes)[0]->nodeName eq "a";
};

check "remove_child", sub {
    my $doc = XML::LibXML->load_xml(string => "<r><a/><b/></r>");
    my @kids = $doc->documentElement->childNodes;
    $doc->documentElement->removeChild($kids[0]);
    my @after = map { $_->nodeName } $doc->documentElement->childNodes;
    "@after" eq "b";
};

# ── XSD validation ───────────────────────────────────────────────────────

my $XSD_SRC = <<'XSD';
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:demo" xmlns="urn:demo">
  <xs:element name="port" type="xs:int"/>
</xs:schema>
XSD

check "xsd_valid_doc", sub {
    my $schema = XML::LibXML::Schema->new(string => $XSD_SRC);
    my $doc = XML::LibXML->load_xml(
        string => qq{<port xmlns="urn:demo">8080</port>});
    eval { $schema->validate($doc) };
    !$@;
};

check "xsd_invalid_doc", sub {
    my $schema = XML::LibXML::Schema->new(string => $XSD_SRC);
    my $doc = XML::LibXML->load_xml(
        string => qq{<port xmlns="urn:demo">not-a-number</port>});
    eval { $schema->validate($doc) };
    $@ ? 1 : 0;
};

# ── HTML parsing ─────────────────────────────────────────────────────────

check "html_basic_lifecycle", sub {
    my $doc = XML::LibXML->load_html(
        string => "<html><body><p>hi</p></body></html>",
        recover => 1, suppress_warnings => 1, suppress_errors => 1,
    );
    my ($p) = $doc->findnodes("//p");
    $p && $p->textContent eq "hi";
};

# ── summary ──────────────────────────────────────────────────────────────

my ($pass, $fail) = (0, 0);
for my $r (@results) {
    my ($name, $ok, $detail) = @$r;
    if ($ok) {
        $pass++;
        print "PASS $name\n";
    } else {
        $fail++;
        print "FAIL $name", ($detail ? " ($detail)" : ""), "\n";
    }
}
print "\nsummary: $pass passed, $fail failed of ", $pass + $fail, " total\n";
exit($fail == 0 ? 0 : 1);
