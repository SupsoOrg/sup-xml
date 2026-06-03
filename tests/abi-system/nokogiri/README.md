# nokogiri smoke test against sup-xml-compat

Drives Ruby `nokogiri` against `libsup_xml_compat.dylib` — the Ruby
analog of the [`lxml/`](../lxml/) smoke test. nokogiri is the most
demanding libxml2 consumer in the Ruby ecosystem.

## Running

```
./run.sh
```

Idempotent and reentrant. All artifacts under `<repo>/target/`,
wiped by `cargo clean`.

## How it differs from lxml/

| | lxml | nokogiri |
|---|---|---|
| Precompiled wheel/gem | `pip install lxml` with `STATIC_DEPS=false` dynamically links libxml2 | precompiled `arm64-darwin` gem **statically bundles** libxml2 inside `nokogiri.bundle` |
| Workaround | (none needed) | install nokogiri from source with `--use-system-libraries` so `nokogiri.bundle` dynamically links libxml2 |
| Build time | seconds | ~1-2 min first time |

The install-from-source step is what makes the smoke test *possible* —
without it, nokogiri's static libxml2 inside the bundle has no
external symbol references to redirect.

## What `run.sh` does

1. **Builds the shim** via `cargo build -p sup-xml-compat --features cdylib-exports`.
2. **Installs nokogiri from source** into `target/nokogiri-gem-home/` with
   `--use-system-libraries` so the resulting `nokogiri.bundle` references
   Homebrew's libxml2 as a dynamic load command.
3. **Detects the linked dylib name** from `otool -L nokogiri.bundle`.
   Homebrew's libxml2 used to ship as `libxml2.2.dylib` (v2 ABI); since
   libxml2 3.x the major-version bump makes it `libxml2.16.dylib`. The
   runner reads the actual name off the bundle rather than hard-coding.
4. **Stages `target/nokogiri-swap/`** with a `<linked-name>.dylib` symlink
   pointing at our cdylib. Also copies Homebrew's `libxslt.1.dylib` and
   `libexslt.0.dylib`, rewriting their embedded libxml2 reference to
   point at our shim (so libxslt → libxml2 transitively goes through us).
5. **Copies nokogiri.bundle** to `target/nokogiri-redir/` and rewrites
   its `LC_LOAD_DYLIB` commands via `install_name_tool`. Ad-hoc resigns
   so dyld accepts the modified bundle.
6. **Runs `smoke.rb` twice**:
   - Control: against Homebrew libxml2 directly.
   - Shim: against `libsup_xml_compat`.

## Reading the output

- **Control run all PASS** = nokogiri built correctly, baseline works.
- **Shim run** = the real test:
  - `LoadError: cannot load nokogiri` at first `require "nokogiri"` =
    a symbol we don't export. Run `nm -gU target/debug/libsup_xml_compat.dylib`
    and diff against what nokogiri.bundle needs.
  - `PASS` rows = nokogiri operations that worked on our shim.
  - `FAIL` rows = exceptions raised mid-test (process survives).

## Status

As of the last run: **17 passed, 0 failed of 17** on both control
(Homebrew libxml2 v3, `libxml2.16.dylib`) and shim (`sup-xml-compat`).
nokogiri does not detect any behavioural difference between the two
libraries across `smoke.rb`'s fixtures.

Of note: Homebrew's libxml2 was bumped to v3 (`libxml2.16.dylib`)
recently, while our shim targets the v2 ABI surface. The smoke test
proves that for the surface nokogiri exercises, our v2-shaped shim
is a working drop-in even against a binding compiled against v3
headers — the symbol set and struct layouts nokogiri uses are
unchanged across the version bump.

## Build prerequisites

The install-from-source step needs:

- Homebrew `libxml2` and `libxslt` (`brew install libxml2 libxslt`).
- Xcode Command Line Tools.
- Permissive CFLAGS to defeat clang-17's strict-mode escalations that
  otherwise make mkmf's `try_cppflags` self-tests fail and skip the
  libgumbo include path; the runner sets these automatically:

  ```
  -Wno-error
  -Wno-unused-command-line-argument
  -Wno-default-const-init-field-unsafe
  -Wno-incompatible-function-pointer-types
  -Wno-implicit-function-declaration
  ```
