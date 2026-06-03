---
title: Migrating from libxml2
description: Function-by-function mapping from libxml2 to SupXML, ABI compatibility, behaviour differences.
---

SupXML is designed as a drop-in replacement for libxml2, both at the Rust
API level (idiomatic Rust) and at the C ABI level (the `sup-xml-compat`
crate ships `libsupxml2.so` with byte-compatible struct layouts).

## Two migration paths

### Path 1: C/C++ code → libsupxml2

If you have C/C++ code that links libxml2, and you don't want to change any
source, you can use our ABI as a drop-in replacement. 
Build `sup-xml-compat` as a cdylib, drop `libsupxml2.so` in your
loader path, and adjust the linker flags from `-lxml2` to `-lsupxml2`. The
struct layouts (`_xmlNode`, `_xmlAttr`, `_xmlNs`) are byte-compatible,
verified at compile time with `offset_of!` assertions and `_Static_assert`
in our C test harness.

Validated end-to-end against the two most demanding libxml2 consumers in
the wild:

- **Python `lxml`**: 47 / 47 operations behaviourally identical between
  system libxml2 and our shim (`tests/abi-system/lxml/`).
- **Ruby `nokogiri`**: 17 / 17 operations identical against
  Homebrew's libxml2 (v3, `libxml2.16.dylib`) — major-version differences
  in the dylib don't break ABI compatibility for the surface nokogiri uses
  (`tests/abi-system/nokogiri/`).

Function-by-function ctypes comparison harness against the system
libxml2: 45 calls / globals matched, 1 annotated diff (version string
differs by design), 0 unknown failures (`tests/abi-system/comparison/`).

### Path 2: Rewriting in Rust

If you're rewriting from C to Rust, use the `sup-xml` crate directly. The
API mapping below covers the common cases.

## Binding install steps

Most precompiled language bindings (Python `lxml` wheel for macOS, Ruby
`nokogiri` `arm64-darwin` gem, Node.js `libxmljs` binary release)
**statically bundle libxml2 inside their native extension**. That means
they carry their own private copy of libxml2 and ignore the system one
entirely — swapping `/usr/lib/libxml2.2.dylib` for our shim does nothing
to a precompiled binding.

To get the binding to dynamically link libxml2 (so it can be redirected
to our shim), do a **one-time rebuild against system libraries** when
installing:

### Python — `lxml`

```bash
STATIC_DEPS=false pip install --no-binary lxml lxml
```

The `STATIC_DEPS=false` env var disables lxml's bundled-deps build path;
`--no-binary lxml` forces a source install rather than the precompiled
wheel. Result: `etree.so` dynamically links the system `libxml2.2.dylib`.

### Ruby — `nokogiri`

```bash
gem install nokogiri --platform=ruby -- --use-system-libraries
```

`--platform=ruby` overrides the precompiled `arm64-darwin` (or
`x86_64-linux`) gem and pulls the source gem instead. `--use-system-libraries`
makes nokogiri link to the system's libxml2/libxslt/libexslt at build
time rather than bundling them.

On macOS with strict modern clang you may also need:

```bash
CFLAGS="-Wno-error -Wno-unused-command-line-argument" \
gem install nokogiri --platform=ruby -- \
    --use-system-libraries \
    --with-xml2-dir=/opt/homebrew/opt/libxml2
```

The `CFLAGS` overrides defeat mkmf's strict-mode self-tests that
otherwise fail and skip the libgumbo include path.

### Node.js — `libxmljs`

`libxmljs` typically dynamically links libxml2 by default on macOS/Linux
when built from source. The npm precompiled binaries may bundle —
forcing a source build with `npm install libxmljs --build-from-source`
ensures dynamic linkage.

### Perl — `XML::LibXML`, R — `xml2`

Both dynamically link the system libxml2 by default during `cpan` /
`install.packages` (and macOS's system Perl ships `XML::LibXML`
pre-linked), so no special install flag is needed. **However**, the
symbol surface XML::LibXML uses is substantially larger than lxml or
nokogiri — including the `xmlTextReader` pull-parser API, schema
error callbacks, namespace reconciliation, and several save-formatter
variants. The compat shim doesn't cover all of these yet. See
`tests/abi-system/perl/` for the current scaffolding + gap list.

### Once the binding is dynamically linked

After the binding is built against the system libxml2, swapping in our
shim is the same `install_name_tool -change` dance (macOS) or
`LD_LIBRARY_PATH` setup (Linux) that you'd use for any libxml2 swap.

For a worked example of the full install + redirect + verify pipeline, see
`tests/abi-system/lxml/run.sh` and `tests/abi-system/nokogiri/run.sh` in
the source tree.

## Function mapping

| libxml2 (C) | SupXML (Rust) |
|---|---|
| `xmlReadMemory` | `parse_bytes` (takes `&ParseOptions`) |
| `xmlReadDoc` | `parse_str` (takes `&ParseOptions`) |
| `xmlReadFile` | `parse_bytes` + `std::fs::read` |
| `xmlDocDumpMemory` | `serialize_to_string` |
| `xmlDocDumpFormatMemory` | `serialize_formatted` |
| `xmlSaveFormatFileEnc` | `serialize_with` (output options) |
| `xmlXPathNewContext` | `XPathContext::new(&doc)` |
| `xmlXPathEvalExpression` | `XPathContext::eval` |
| `xmlSchemaParse` | `Schema::compile_str`, `Schema::compile_bytes` |
| `xmlSchemaValidateDoc` | `Schema::validate_doc` |
| `xmlSchematronParse` | `Schematron::compile_str` |
| `xsltParseStylesheetDoc` | `Stylesheet::compile_str` |
| `xsltApplyStylesheet` | `Stylesheet::apply` |
| `xmlC14NDocSaveTo` | `canonicalize_to_bytes` |

## Parsing options

`ParseOptions` field names (verify against
[options.rs](https://github.com/SupsoOrg/sup-xml/blob/main/crates/core/src/options.rs)):

| libxml2 flag | SupXML field |
|---|---|
| `XML_PARSE_NOENT` | `resolve_entities: true` (default) |
| `XML_PARSE_DTDLOAD` | `load_external_dtd: true` |
| `XML_PARSE_DTDVALID` | `validating: true` |
| `XML_PARSE_NOERROR` | (errors returned as `Result`, never printed) |
| `XML_PARSE_RECOVER` | `recovery_mode: true` |
| `XML_PARSE_NOBLANKS` | `skip_inter_element_whitespace: true` |
| `XML_PARSE_NONET` | `external_resolver: None` (default — no resolver = no external loads) |
| `XML_PARSE_HUGE` | `max_entity_expansion_bytes: u64::MAX` (default is 1 MB) |

## Behaviour differences

### Memory model

libxml2 has process-global state: `xmlInitParser`, a global error handler, a
shared dictionary, global default-loader hooks. SupXML has none of these —
every `parse_*` call allocates a fresh arena, errors are returned as `Result`,
and external-entity loaders are passed in via `ParseOptions::external_resolver`.

If you were relying on `xmlSetExternalEntityLoader` to redirect all entity
loads globally, you now configure an `EntityResolver` per parse call.

### Error reporting

libxml2's errors are emitted via callback (`xmlSetStructuredErrorFunc`) and
optionally printed to stderr. SupXML returns the first error from
`parse_str` and surfaces additional recovered errors via
`reader.recovered_errors()` when `recovery_mode: true`.

### Recovery mode

libxml2's `XML_PARSE_RECOVER` drops the surrounding text on a bare `&` in
PCDATA. SupXML's recovery mode preserves the `&` as a literal and surfaces
the error via `recovered_errors()`. The bare-`&` case is test-backed in
`crates/api/tests/recovery.rs`.

[Recovery mode details →](/guides/recovery/)

### Entity expansion limits

libxml2 caps entity expansion at ~10 MB unless you pass `XML_PARSE_HUGE`.
SupXML caps at **1 MB** by default (`max_entity_expansion_bytes:
1_000_000`). The default is on for both libraries because billion-laughs
attacks are a real threat — be deliberate if you raise it.

## Things that are the same

- Document model — `Node`, `Attribute`, `Namespace` map closely.
- XPath 1.0 semantics — context node, axis specifiers, function library,
  EXSLT extensions (always on, no registration needed).
- XSLT 1.0 — match patterns, modes, `key()`, `document()`, `format-number()`.
- Canonical XML — Inclusive (1.0 / 1.1), Exclusive (Exc-C14N), with or without comments.
- DTD validation — internal subset, external subset, entity declarations.

## Things that are deliberately different

- No global init. No `xmlInitParser`, no `xmlCleanupParser`.
- No `xmlFree*` — Rust drops things automatically.
- No print-on-error. Errors are `Result` and never touch stderr.
- No exception-style `xmlGetLastError`. Errors are returned, not stashed.

## Gaps

If you find a libxml2 feature you depend on that isn't covered yet, please
[file an issue](https://github.com/SupsoOrg/sup-xml/issues). Migration is the
project's #1 use case and we close gaps fast.
