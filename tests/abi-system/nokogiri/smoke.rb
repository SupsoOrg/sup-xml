#!/usr/bin/env ruby
# Smoke test: drive nokogiri against the libxml2 it links against.
# Run twice — once via system libxml2 (control), once via our shim
# (the real test).  Each row is one focused operation.
#
# Output format mirrors tests/abi-system/lxml/smoke.py.

require "nokogiri"

PASS = "PASS"
FAIL = "FAIL"

results = []

def check(name, &block)
  ok = false
  detail = nil
  begin
    ok = block.call
  rescue => e
    detail = "#{e.class}: #{e.message}"
  end
  [name, ok, detail]
end

# ── basic parse / inspect ───────────────────────────────────────────────────

results << check("tiny_parse") do
  doc = Nokogiri::XML("<r/>")
  doc.root.name == "r"
end

results << check("text_content") do
  doc = Nokogiri::XML("<r>hello</r>")
  doc.root.content == "hello"
end

results << check("attributes") do
  doc = Nokogiri::XML(%(<r a="1" b="2"/>))
  doc.root["a"] == "1" && doc.root["b"] == "2"
end

results << check("children_iter") do
  doc = Nokogiri::XML("<r><a/><b/><c/></r>")
  doc.root.element_children.map(&:name) == %w[a b c]
end

results << check("mixed_content") do
  doc = Nokogiri::XML("<r>foo<b>bar</b>baz</r>")
  doc.root.children.map(&:text?).count(true) == 2
end

results << check("default_namespace") do
  doc = Nokogiri::XML(%(<r xmlns="urn:demo"/>))
  doc.root.namespace.href == "urn:demo"
end

results << check("prefixed_namespace") do
  doc = Nokogiri::XML(%(<x:r xmlns:x="urn:demo"/>))
  doc.root.namespace.prefix == "x"
end

results << check("round_trip_string") do
  s = %(<r a="1"><c/></r>)
  Nokogiri::XML(s).root.to_s.include?(%(a="1"))
end

results << check("malformed_raises") do
  Nokogiri::XML("<r><unclosed>") { |c| c.strict }
  false  # should have raised
rescue Nokogiri::XML::SyntaxError
  true
end

# ── XPath ──────────────────────────────────────────────────────────────────

results << check("xpath_simple") do
  doc = Nokogiri::XML("<catalog><book/><book/></catalog>")
  doc.xpath("count(/catalog/book)") == 2.0
end

results << check("xpath_attribute") do
  doc = Nokogiri::XML(%(<r><a id="x"/><a id="y"/></r>))
  doc.xpath("//a/@id").map(&:value) == %w[x y]
end

# ── mutation ───────────────────────────────────────────────────────────────

results << check("set_attribute") do
  doc = Nokogiri::XML("<r/>")
  doc.root["k"] = "v"
  doc.root["k"] == "v"
end

results << check("append_subelement") do
  doc = Nokogiri::XML("<r/>")
  doc.root.add_child("<a/>")
  doc.root.element_children.first.name == "a"
end

results << check("remove_child") do
  doc = Nokogiri::XML("<r><a/><b/></r>")
  doc.root.element_children.first.remove
  doc.root.element_children.map(&:name) == %w[b]
end

# ── XSD validation ─────────────────────────────────────────────────────────

XSD_SRC = <<~XSD
  <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
             targetNamespace="urn:demo" xmlns="urn:demo">
    <xs:element name="port" type="xs:int"/>
  </xs:schema>
XSD

results << check("xsd_valid_doc") do
  schema = Nokogiri::XML::Schema(XSD_SRC)
  schema.validate(Nokogiri::XML(%(<port xmlns="urn:demo">8080</port>))).empty?
end

results << check("xsd_invalid_doc") do
  schema = Nokogiri::XML::Schema(XSD_SRC)
  !schema.validate(Nokogiri::XML(%(<port xmlns="urn:demo">not-a-number</port>))).empty?
end

# ── HTML5 ──────────────────────────────────────────────────────────────────

results << check("html_basic_lifecycle") do
  d = Nokogiri::HTML("<html><body><p>hi</p></body></html>")
  d.at("p").content == "hi"
end

# ── summary ────────────────────────────────────────────────────────────────

pass = 0; fail = 0
results.each do |name, ok, detail|
  if ok
    pass += 1
    puts "#{PASS} #{name}"
  else
    fail += 1
    msg = detail ? " (#{detail})" : ""
    puts "#{FAIL} #{name}#{msg}"
  end
end
puts
puts "summary: #{pass} passed, #{fail} failed of #{pass + fail} total"
exit(fail == 0 ? 0 : 1)
