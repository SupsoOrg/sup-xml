# tests/abi-system/

End-to-end tests of the libxml2 ABI shim (`sup-xml-compat`).  These
are the highest-fidelity validations of the C-ABI surface: they
exercise it the way a real downstream consumer would, not via
in-process Rust tests.

## Subfolders

| Path | What it tests |
|---|---|
| [`lxml/`](lxml/) | Runs Python `lxml.etree` against `libsup_xml_compat.dylib` via `install_name_tool` redirection.  Catches any layout/ABI mismatch that the C-side `_Static_assert` tests (`crates/compat/c-tests/`) miss — lxml is the most demanding real-world libxml2 consumer. |
| [`comparison/`](comparison/) | Loads `libsup_xml_compat.dylib` AND the system `libxml2.2.dylib` into the same Python process via `ctypes`, calls each exported function on both, asserts results match, and times each.  Produces a side-by-side correctness + perf table. |

## How to run

Each subfolder has its own runner; see their READMEs for details.
At a glance:

```sh
tests/abi-system/lxml/run.sh         # lxml + sup-xml-compat smoke
tests/abi-system/comparison/compare.py   # ctypes side-by-side
```

Both build `sup-xml-compat` first if needed.  Output goes to stdout;
artifacts (built dylibs, redirected `etree.so`) land in
`<repo>/target/` and are wiped by `cargo clean`.

## When to update

- Adding a new exported libxml2 function to `sup-xml-compat`: lxml
  may exercise it indirectly (no change needed), or you may want to
  add an explicit comparison-harness call to lock down behaviour.
- ABI struct layout change: re-run both suites; layout mismatches
  here surface as field-read garbage rather than the cleaner
  `_Static_assert` errors from the C tests.
