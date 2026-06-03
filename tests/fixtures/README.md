# tests/fixtures/

Small, hand-curated XML inputs used by the integration tests under
`crates/api/tests/`.  These are deliberately tiny (a few hundred
bytes each) so each test asserts a precise behavioural property
rather than averaging over a large document.

For real-world-shaped inputs (KB-to-GB sized documents from actual
producers), see [`../assets/xml/`](../assets/xml/) instead.

## Files

| File | What it covers |
|---|---|
| `attributes.xml` | Attribute parsing edge cases: ordering, whitespace, entities in values |
| `cdata.xml` | `<![CDATA[…]]>` sections — round-trip preservation, lookalike `]]>` rejection in text |
| `deep.xml` | Deeply nested element structure — exercises the parser's recursion guard |
| `namespaces.xml` | Default + prefixed namespaces, redefinition, scope inheritance |
| `simple.xml` | Smallest non-trivial document — sanity-check baseline |
| `unicode.xml` | Non-ASCII content + names, surrogate pair handling, encoding edge cases |
| `unusual.xml` | Whitespace placement, comments in weird positions, PIs around the root |
| [`cve/`](cve/) | Attack vectors the parser must reject (billion-laughs etc.) |

## How they're loaded

Each `crates/api/tests/*.rs` file includes its fixtures via:

```rust
macro_rules! fixture {
    ($name:expr) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"),
                             "/../../tests/fixtures/", $name))
    };
}
```

So the fixtures are baked into the test binary at compile time —
no runtime file I/O, no path-resolution flakiness in CI.

## How to run

```sh
cargo test -p sup-xml --tests             # all api integration tests
cargo test -p sup-xml --test parse_fixtures   # just the parse-fixture suite
```

## Adding a new fixture

1. Drop the `.xml` file here with a descriptive name (lowercase,
   underscores or dashes; no hash-named files).
2. Reference it from a test via the `fixture!` macro above.
3. Keep it small — if you need a 10 KB+ real-world document, put
   it in [`../assets/xml/`](../assets/xml/) instead.
