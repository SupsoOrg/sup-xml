---
title: Typed-struct deserialize (serde)
description: Deserialize XML directly into Rust structs via serde — element-as-struct, attribute-as-field, text content via $value.
---

The `serde` feature wires SupXML's parser into serde's `Deserialize`
trait so you can read XML straight into Rust structs without walking the
DOM by hand.

## Enabling

```toml
[dependencies]
sup-xml = { version = "*", features = ["serde"] }
serde = { version = "1", features = ["derive"] }
```

## Basic usage

```rust
use sup_xml::de::from_str;
use serde::Deserialize;

#[derive(Deserialize, Debug, PartialEq)]
struct Book {
    #[serde(rename = "@id")]    // @-prefix = attribute
    id: String,
    title: String,
    price: f64,
}

let xml = r#"
    <book id="b1">
        <title>The Soul of a New Machine</title>
        <price>19.99</price>
    </book>
"#;

let book: Book = from_str(xml)?;
assert_eq!(book.id, "b1");
assert_eq!(book.title, "The Soul of a New Machine");
```

## Conventions

- **`@name`** — attribute. Map to a struct field with `#[serde(rename = "@name")]`.
- **Element content** — a struct field with the element's local name is deserialized
  from that child element's content. A *plainly-named* field is matched against a
  child element of that name — not against the parent's text.
- **`$text`** — to capture an element's free text content (the `hello` in
  `<note lang="en">hello</note>`), name a field `$text`
  (`#[serde(rename = "$text")]`).
- **`$value`** — to capture heterogeneous child elements as a sequence, name a
  field `$value` (`#[serde(rename = "$value")]`); it collects every child element
  that doesn't match a named field. `$value` is for *elements* — stray text
  between them is dropped, so reach for `$text` when you want the text itself.
- **`Vec<T>`** — repeated child elements with the same name collect into the vec.
- **`Option<T>`** — absent attribute or element gives `None`; present gives `Some`.

```rust
#[derive(Deserialize)]
struct Catalog {
    #[serde(rename = "book")]
    books: Vec<Book>,
}

let xml = r#"<catalog><book id="a">A</book><book id="b">B</book></catalog>"#;
let cat: Catalog = from_str(xml)?;
assert_eq!(cat.books.len(), 2);
```

## From bytes

```rust
use sup_xml::de::from_bytes;

let book: Book = from_bytes(xml_bytes)?;   // bytes must be valid UTF-8
```

## Options

```rust
use sup_xml::{ParseOptions, de::{from_str_opts, DeOptions}};

let opts = DeOptions {
    // Parser-level knobs live on the nested `parse` field.
    parse: ParseOptions {
        recovery_mode: true,
        skip_inter_element_whitespace: true,
        ..Default::default()
    },
    ..Default::default()
};
let book: Book = from_str_opts(xml, opts)?;
```

## Errors

`DeError` carries the line/column of the failure point and the serde
path that triggered it (e.g. `book.title: invalid type: expected
integer`). Useful for surfacing schema-style failures to end users
without writing a separate validator.

## Limitations

- **Borrowed `&str` fields** are NOT supported — the deserializer
  allocates per field because XML allows escapes (`&amp;`,
  `&#x20;`) that need decoding into owned storage. Use `String` /
  `Cow<'_, str>`.
- **Mixed content** (a struct with both typed children and inline text)
  is approximated by capturing the inline text into `$value` — fine for
  HTML-ish inputs, awkward for documents where text and elements
  interleave structurally.
- **Namespaces are ignored** by default; the local name is what's
  matched against struct field names. Cross-namespace disambiguation
  needs the lower-level `XmlDeserializer` and a custom visitor.

For full control over the deserialization shape, drop down to
`XmlDeserializer` and implement `Deserialize` by hand against the
SupXML event stream.
