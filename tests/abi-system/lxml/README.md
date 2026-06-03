# lxml smoke test against sup-xml-compat

Drives Python `lxml.etree` against the `libsup_xml_compat.dylib`
shim — the most realistic possible test of our libxml2 ABI: a
mainstream consumer that doesn't know it's running on us.

## Running

```
./run.sh
```

This is idempotent and reentrant. All artifacts live in
`<repo>/target/` (gitignored, wiped by `cargo clean`) — nothing
outside the repo is touched.

## What `run.sh` does

1. **Builds the shim** via `cargo build -p sup-xml-compat --features cdylib-exports`.
   The `cdylib-exports` feature is what flips `#[no_mangle]` on every
   libxml2-named entry point; without it the symbols come out
   Rust-mangled and lxml's `etree.so` can't link against them.
2. **Creates `target/py-venv/`** (one-time) and `pip install`s lxml
   with `STATIC_DEPS=false` so it links dynamically against the system
   `libxml2.2.dylib` (PyPI's binary wheel bundles libxml2 statically;
   useless for our test).
3. **Stages `target/lxml-swap/`** with a `libxml2.2.dylib` symlink
   pointing at our cdylib.
4. **Creates `target/lxml-redir/`** with a modified copy of lxml's
   `etree.so` whose `LC_LOAD_DYLIB` is rewritten to point at the swap
   dir via `install_name_tool`. Ad-hoc re-signs the modified .so so
   dyld accepts it.
5. **Runs the smoke test twice**:
   - Control: against the system `/usr/lib/libxml2.2.dylib`.
   - Shim: with `PYTHONPATH` aimed at the redirected lxml.

## Why the redirection dance is necessary

macOS uses two-level namespaces: lxml's `etree.so` records every
external symbol along with the absolute install_name of the dylib
that defines it (`/usr/lib/libxml2.2.dylib`). `DYLD_INSERT_LIBRARIES`
alone doesn't override that — both dylibs end up loaded, but
lxml's calls still route to the system one. `install_name_tool
-change` rewrites the load command so dyld looks at our shim
instead.

## Reading the output

- **Control run all PASS** = lxml installed correctly, fixtures load,
  baseline works.
- **Shim run** = the real test:
  - `ImportError: Symbol not found` at the first `from lxml import
    etree` means lxml needs a symbol we don't export. See
    `target_symbols.txt`.
  - `PASS` rows are lxml operations that succeeded on top of our shim.
  - `FAIL` rows surfaced exceptions (e.g. `XMLSyntaxError`); the
    Python process survives.
  - `CRASH` (process exit code != 0 with no output) means we segfaulted
    on a layout-mismatch or NULL-deref — strongest signal to
    investigate immediately.

## Symbol inventory

`target_symbols.txt` lists every `xml*`/`html*` symbol that lxml's
`etree.so` references. The compat shim currently exports the full
list — diffing the file against the cdylib's exported symbol set
(`nm -gU target/debug/libsup_xml_compat.dylib`) shows no gap.

Having symbols resolve at link time is necessary but not sufficient:
the value the smoke test produces is in catching behavioural and
struct-layout mismatches that pure symbol presence can't reveal.
PASS / FAIL / CRASH output below is the load-bearing signal.

## Current status

As of the last run: **47 passed, 0 failed of 47** on both control
(system libxml2) and shim (sup-xml-compat). lxml does not detect any
behavioural difference between the two libraries across the fixtures
in `smoke.py`.
