---
title: Parsing & serialization
description: Parse XML from strings, bytes, or any byte source; serialize back with control over formatting, encoding, and namespaces.
---

## From a `&str`

```rust
use sup_xml::{parse_str, ParseOptions};

let doc = parse_str("<r/>", &ParseOptions::default())?;
```

`parse_str` is the simplest entry point — input must be valid UTF-8
(Rust enforces that on `&str`). For documents whose declared encoding
isn't UTF-8, use `parse_bytes`.

## From bytes (any encoding)

```rust
use sup_xml::{parse_bytes, ParseOptions};

// Encoding auto-detected from the XML declaration / BOM / WHATWG sniff.
let doc = parse_bytes(include_bytes!("doc.xml"), &ParseOptions::default())?;

// Or with explicit options
let opts = ParseOptions {
    recovery_mode: true,
    skip_inter_element_whitespace: true,
    ..Default::default()
};
let doc = parse_bytes(include_bytes!("doc.xml"), &opts)?;
```

Supported encodings out of the box: UTF-8, UTF-16 (BE/LE), UTF-32, ASCII,
and any encoding label in the
[WHATWG encoding spec](https://encoding.spec.whatwg.org/#names-and-labels)
(Shift_JIS, EUC-JP, GB18030, Windows-125x, ISO-8859-x, …) when the
`full-encodings` feature is on. See the
[character encodings guide](/guides/encodings/) for the full matrix.

## Common `ParseOptions`

| Field | Default | Effect |
|---|---|---|
| `namespace_aware` | `false` | Resolve `xmlns:`/`xmlns=` declarations into qualified names (set `true` for namespaced documents) |
| `recovery_mode` | `false` | Continue past non-fatal errors; surface them via `recovered_errors()` |
| `skip_inter_element_whitespace` | `false` | Drop whitespace between element tags ("ignorable" per XML 1.0 §2.10) |
| `max_entity_expansion_bytes` | `1_000_000` | Defuse billion-laughs attacks; raise for trusted large entities |
| `max_element_depth` | `256` | Stack-overflow defence against pathologically nested input |
| `external_resolver` | `None` | Opt-in for DTD / external-entity loads — see [security model](/reference/security/) |
| `load_external_dtd` | `false` | Fetch + parse external DTD subsets when a resolver is present |
| `validating` | `false` | Run DTD validation alongside well-formedness |

## Serialization

```rust
use sup_xml::{serialize_to_string, serialize_formatted, serialize_with, SerializeOptions};

// Compact (one line, no inter-element whitespace)
let xml: String = serialize_to_string(&doc);

// Pretty-printed (newlines + two-space indent)
let pretty: String = serialize_formatted(&doc);

// Full control over the XML declaration, indentation, and HTML mode
let opts = SerializeOptions {
    write_xml_decl: true,
    format: true,
    indent: "    ".to_string(),   // four-space indent
    ..Default::default()
};
let xml: String = serialize_with(&doc, &opts);
```

Output round-trips byte-stable through `parse_*` → `serialize_*` for
inputs that don't carry redundant whitespace or alternate
attribute-quote / numeric-character-reference encodings. (The XML spec
allows several valid representations of the same document; we
normalise to the canonical one.)

## Streaming — SAX-style events

Two readers process XML as an event stream instead of building a DOM, so
you control how much state to retain. Both surface the same `BytesEvent`s;
they differ in where the bytes come from.

### In memory — `XmlBytesReader`

When the whole document is already in memory (a `&[u8]`), `XmlBytesReader`
is a zero-copy SAX reader over it — same parser core as the DOM path, no
tree allocation:

```rust
use sup_xml::{XmlBytesReader, BytesEvent};

let mut r = XmlBytesReader::from_bytes(b"<r><a/><b>hi</b></r>")?;
loop {
    match r.next()? {
        BytesEvent::Eof => break,
        BytesEvent::StartElement(t) =>
            println!("<{}>", String::from_utf8_lossy(t.name())),
        BytesEvent::EndElement(t) =>
            println!("</{}>", String::from_utf8_lossy(t.name())),
        BytesEvent::Text(t) => {
            let bytes = t.as_bytes();
            if !bytes.iter().all(u8::is_ascii_whitespace) {
                println!("  text: {:?}", String::from_utf8_lossy(bytes));
            }
        }
        _ => {}
    }
}
```

Tag names and text expose borrowed byte slices (`&[u8]`) into the input
buffer, so the inner loop allocates only when it explicitly converts
(e.g. `String::from_utf8_lossy(t.as_bytes())`). That's what lets the
streaming benches hit 3+ GB/s on hot fixtures —
see the [performance reference](/reference/performance/#parse--sax--streaming).

### Larger than memory — `XmlByteStreamReader`

For documents too large to hold in memory, `XmlByteStreamReader` pulls
from any `io::Read` (a file, socket, decompressing reader, stdin…)
through a **rolling buffer**, so peak memory stays bounded by the buffer
size — roughly constant no matter how large the input. Drive it with
`next_event`:

```rust
use std::fs::File;
use sup_xml::{XmlByteStreamReader, BytesEvent, DEFAULT_BUFFER_SIZE};

let file = File::open("catalog.xml")?;
let mut reader = XmlByteStreamReader::new(file, DEFAULT_BUFFER_SIZE)?;

let mut titles = Vec::new();
loop {
    match reader.next_event()? {
        BytesEvent::Eof => break,
        // Each event borrows the rolling buffer and is valid only until
        // the next pull — copy out what you need before looping.
        BytesEvent::Text(t) =>
            titles.push(String::from_utf8_lossy(t.as_bytes()).into_owned()),
        _ => {}
    }
}
```

`next_event` yields the same `BytesEvent`s as `XmlBytesReader::next`; the
borrow checker ties each event to `&mut self`, so you must consume it
before pulling the next (the same zero-copy contract as quick-xml's
`read_event`). Peak memory is ~2× the buffer size — `DEFAULT_BUFFER_SIZE`
is 10 MB; pass `HUGE_BUFFER_SIZE` (1 GB) or a custom size to
`XmlByteStreamReader::new` if a single **atomic** token (an element name
or attribute value — text content is not atomic and splits across events)
exceeds the buffer. Streaming is UTF-8 only.

For async sources (network sockets, async file handles), use the
`tokio` feature and `parse_async` instead — see the
[async guide](/guides/async/).

## Working with the parsed tree

```rust
use sup_xml::Document;

let doc: Document = parse_str("<r a='1'><b/></r>", &Default::default())?;
let root = doc.root();
println!("root: <{}>", root.name());
for attr in root.attributes() {
    println!("  @{}={:?}", attr.name(), attr.value());
}
for child in root.children() {
    println!("  child: <{}>", child.name());
}
```

The DOM is the same `Document` type that XPath, XSLT, Schematron, and
serde-de all operate on, so no translation between modes — parse once,
query / transform / serialize from the same structure.

## Common parse errors

`parse_str` / `parse_bytes` return `Result<Document, XmlError>`.
`XmlError` carries the source-location of the failure (`line` /
`column`) and a structured error code (`XmlErrorKind`) for switching
on the failure cause without parsing the message string. Recovery
mode lets you walk past the failure and collect a list of recovered
errors via `reader.recovered_errors()` — see the
[recovery guide](/guides/recovery/).
