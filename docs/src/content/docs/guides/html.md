---
title: HTML5 parsing
description: Parse real-world HTML5 documents (browser-equivalent recovery) into the same DOM that XML parsing produces.
---

SupXML ships an HTML5 parser built on `html5ever` — Mozilla's WHATWG-
conformant tokenizer + tree builder, the same code Servo uses. It
produces SupXML's `Document` type, so the XPath / XSLT / Schematron /
serializer / serde-de paths work on HTML inputs without translation.

## Enabling

```toml
[dependencies]
sup-xml = { version = "*", features = ["html"] }
```

## Parse a document

```rust
use sup_xml::{parse_html_str, parse_html_bytes};

let doc = parse_html_str(r#"<!doctype html><html><body><p>hi</p></body></html>"#)?;

// From raw bytes (encoding auto-detected via WHATWG sniff)
let doc = parse_html_bytes(include_bytes!("page.html"))?;
```

`parse_html_str` is lenient by default — it'll close unclosed tags,
infer `<html>` / `<body>` wrappers, and recover from malformed markup
the way a browser does. To insist on strict tokenisation, use
`parse_html_str_opts` with `recovery_mode: false`:

```rust
use sup_xml::{parse_html_str_opts, HtmlParseOptions};

let strict = HtmlParseOptions { recovery_mode: false, ..Default::default() };
let doc = parse_html_str_opts(
    "<!doctype html><html><body><p>hi</p></body></html>", &strict)?;
```

Strict mode is for confirming input is already a complete, well-formed
document: it requires a `<!doctype>` and rejects bare fragments
(`<p>hi</p>` on its own errors). Reach for the default lenient parser
when you're cleaning up real-world or partial HTML.

## Query and modify like XML

```rust
use sup_xml::XPathContext;

let doc = parse_html_str(html)?;
let ctx = XPathContext::new(&doc);

// Same XPath surface as XML.
let titles = ctx.eval_strings("//h2/text()")?;
for t in titles { println!("{t}"); }
```

XPath axes, predicates, the EXSLT function library, and namespace
handling all work identically. The DOM is the same `Document`, so
`serialize_to_string` round-trips back to XHTML-shaped output.

## Streaming SAX-style parse

For documents that don't fit in memory, `HtmlSaxParser` runs the
tokenizer + tree builder against an incremental byte feed and emits
events through your `HtmlSaxHandler`:

```rust
use sup_xml::{HtmlSaxParser, HtmlSaxHandler, HtmlAttrs};

struct Counter { p: usize }
impl HtmlSaxHandler for Counter {
    // Every method has a default no-op impl — override only what you need.
    fn start_element(&mut self, name: &str, _attrs: HtmlAttrs<'_>) {
        if name == "p" { self.p += 1; }
    }
}

let mut parser = HtmlSaxParser::new(Counter { p: 0 });
parser.feed("<html><body><p>one</p><p>two</p></body></html>")?;
let counter = parser.finish()?;   // `finish` consumes the parser and returns the handler
assert_eq!(counter.p, 2);
```

Maps onto libxml2's `htmlSAXParseChunk` and lxml's
`HTMLParser(target=...)`. The handler API is push-based, so it's safe
under streaming and async runtimes alike.

## What's covered, what isn't

✅ Full WHATWG HTML5 tokenizer + tree builder via `html5ever`.
✅ Browser-equivalent recovery on malformed input.
✅ Encoding sniff (UTF-8, UTF-16, declared `<meta charset>`, plus the
WHATWG fall-backs via `encoding_rs`).
✅ Boolean attributes, void elements, raw `<script>` / `<style>`
content, attribute-value unquoting.

❌ Encoding re-detection mid-parse — WHATWG allows the parser to detect
a `<meta>` later in the document and restart with a different
encoding; we commit to the sniffed encoding at the first byte.
❌ Pretty-printed HTML output — printer is compact; HTML pretty-printing
needs block-vs-inline awareness that's not in v1.
❌ `parse_html_fragment` (always wraps in implicit `<html>`/`<body>`) —
the fragment-context entry point is on the roadmap.

## Performance

Median throughput **~1.02× faster than libxml2's HTML parser** and
**~1.01× of html5ever** at the matched-contract head-to-head on 9
real-world pages. See the
[performance reference](/reference/performance/#html-parse--matched-against-html5ever-and-libxml2)
for the per-fixture table.
