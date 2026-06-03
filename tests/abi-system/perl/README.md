# Perl XML::LibXML smoke test against sup-xml-compat

Drives Perl's `XML::LibXML` (the standard libxml2 binding in the Perl
ecosystem) against `libsup_xml_compat.dylib` — the Perl analog of the
[`lxml/`](../lxml/) and [`nokogiri/`](../nokogiri/) harnesses.

## Running

```
./run.sh
```

Idempotent and reentrant. All artifacts under `<repo>/target/`,
wiped by `cargo clean`.

## How it differs from the lxml and nokogiri runners

**Way simpler.** macOS's system Perl ships `XML::LibXML` already
dynamically linked to `/usr/lib/libxml2.2.dylib` — no install-from-source
step, no `--use-system-libraries` flag, no compiler flag tuning. We just:

1. Build the shim.
2. Stage a swap dir with our shim symlinked as `libxml2.2.dylib`.
3. Copy `XML/LibXML.pm` + `XML/LibXML/*.pm` + `auto/XML/LibXML/LibXML.bundle`
   into a `target/perl-redir-inc/` tree.
4. `install_name_tool -change` the libxml2 reference in the copied bundle.
5. Run `smoke.pl` with `PERL5LIB=target/perl-redir-inc` so the redirected
   tree takes priority.

The whole thing runs in well under a second after the cdylib is built.

## Status

As of the last run: **control 17 / 17 PASS, shim 17 / 17 PASS**.

`use XML::LibXML` loads against the shim and every behavioural case
passes — parse, text/attribute access, child iteration, mixed content,
default and prefixed namespaces, string round-trip, malformed-input
rejection, XPath, attribute/subtree mutation, XSD valid/invalid, and the
HTML lifecycle.

### History: the earlier `Symbol not found` failure

This harness previously failed at module load with `Symbol not found`.
The cause was **not** missing implementations: XML::LibXML's
`LibXML.bundle` references ~95 libxml2 symbols (the `xmlTextReader*`
pull-parser family, schema error callbacks, namespace reconciliation,
save/output-buffer, pattern, regex, tree mutation, HTML output) that
were **already implemented in `crates/compat/`** but absent from the
cdylib's export allowlist (`crates/compat/src/symbols.txt` /
`symbols.ld`), so the linker hid them. Adding the implemented symbols
to the allowlist resolved the load failure; the behavioural cases then
passed without further code changes.

## Why XML::LibXML is heavier than lxml/nokogiri

The Python and Ruby bindings each pick a relatively focused subset of
the libxml2 API and wrap it in idiomatic objects. XML::LibXML exposes a
much closer 1:1 mapping of libxml2's C API to Perl — including the
xmlTextReader pull parser, several save-formatter variants, and the
low-level pattern/regex compilation APIs that the higher-level bindings
abstract away or don't expose.

## Build prerequisites

- macOS system Perl 5.34+ (ships pre-installed).
- `XML::LibXML` (also pre-installed via the system's Perl Extras tree).
- Xcode Command Line Tools (for `install_name_tool`, `codesign`).
