# PHP smoke test against sup-xml-compat

Drives PHP's libxml2-based XML extensions (DOMDocument, SimpleXML,
DOMXPath, schema validation, HTML parsing) against
`libsup_xml_compat.dylib`.

PHP is special among our smoke tests because **PHP's entire XML stack
IS libxml2** — `ext/libxml`, `ext/dom`, `ext/simplexml`, `ext/xmlreader`,
`ext/xmlwriter`, `ext/xsl` are all thin wrappers over libxml2/libxslt
functions.  If our shim works for PHP, it works for the single largest
libxml2 consumer ecosystem by user count.

## Running

```
./run.sh
```

Requires PHP (`brew install php` on macOS).  All other artifacts live
under `<repo>/target/`, wiped by `cargo clean`.

## How it differs from other runners

| | lxml | nokogiri | Perl | **PHP** |
|---|---|---|---|---|
| Install required | source build with `STATIC_DEPS=false` | source build with `--use-system-libraries` | none (system Perl) | none (Homebrew PHP) |
| Redirect target | etree.so | nokogiri.bundle | LibXML.bundle | **the PHP binary itself** |
| Reason | precompiled bundle stat-links libxml2 | precompiled gem stat-links libxml2 | system XML::LibXML dyn-links system libxml2 | Homebrew PHP dyn-links Homebrew libxml2 |

PHP's XML extensions are built INTO the `php` binary as compiled-in
extensions (not loadable `.so` files), so we redirect the `php` binary's
libxml2 load command directly — copy `php` to `target/php-redir/php`,
`install_name_tool -change` the libxml2 reference, re-sign, and invoke
the redirected copy.

## What `run.sh` does

1. **Builds the shim** via `cargo build -p sup-xml-compat --features cdylib-exports`.
2. **Probes the PHP binary's linkage** with `otool -L` to discover the
   libxml2 dylib name it references (Homebrew currently ships
   `libxml2.16.dylib` — v3 ABI).
3. **Stages `target/php-swap/`** with a `<linked-name>.dylib` symlink
   pointing at our cdylib.
4. **Copies `php` to `target/php-redir/php`** and rewrites its
   `LC_LOAD_DYLIB` libxml2 entry via `install_name_tool`.  Ad-hoc
   re-signs so dyld accepts the modified binary.
5. **Runs `smoke.php` twice**: control (system php) + shim (redirected php).

## Reading the output

- **Control run all PASS** = PHP installed correctly, XML extensions work.
- **Shim run** = the real test:
  - `Symbol not found` at startup or a SIGSEGV before any output = a
    libxml2 entry point we don't export or whose layout we got wrong.
  - `PASS` rows are PHP XML ops that worked on our shim.
  - `FAIL` rows are exceptions/false returns; PHP process survives.

## Why PHP is worth testing

Even if you write zero PHP yourself, **most XML on the web that gets
processed at all is processed by PHP via libxml2**.  Wordpress,
Drupal, Magento, every PHP CMS plugin that touches XML feeds — all
flow through `DOMDocument`/`SimpleXML`.  If the shim handles PHP's
XML surface, the long tail of "I have a PHP server and want to swap
libxml2 for sup-xml" works without per-app intervention.
