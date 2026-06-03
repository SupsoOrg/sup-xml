# tests/abi-system/comparison/

Side-by-side correctness + performance harness for the libxml2 ABI
shim.  Loads BOTH the system `libxml2.2.dylib` AND our
`libsup_xml_compat.dylib` into a single Python process via `ctypes`
(different handles, different load addresses), calls each function
of interest on both, asserts results match, and times them.

## Why Python (and not a Rust integration test)?

The point of `sup-xml-compat` is that **non-Rust consumers** can drop
our dylib in where they expect libxml2 and have things keep working.
That promise is only meaningful if it's exercised from outside Rust:

- **Exercises the C ABI, not the Rust crate.**  A Rust integration
  test would link the `sup-xml-compat` crate and call its Rust API —
  which bypasses the entire `extern "C"` surface, struct layouts,
  null-pointer conventions, and symbol-visibility story.  `ctypes`
  is a foreign caller: it sees only what a C program would see.  If
  an export is mis-named, has the wrong signature, or returns a
  garbage pointer at the FFI boundary, ctypes finds out immediately.

- **Loads two libxml2s in one process, cleanly.**  `ctypes.CDLL(...,
  mode=RTLD_LOCAL)` gives each dylib its own symbol namespace, so
  `xmlStrlen` from the system and `xmlStrlen` from ours coexist with
  no `DYLD_*` tricks and no link-time collision.  A Rust harness
  would have to pick one libxml2 at link time, or fork a subprocess
  per library — both make side-by-side timing far less convenient.

- **Mirrors how real downstream consumers call us.**  `lxml`, GNOME
  tooling, language bindings, OS package managers — almost everyone
  who consumes libxml2 does so through a C-ABI loader, not by
  linking Rust.  Testing through `ctypes` reproduces that path
  byte-for-byte.

- **Aligns with the sibling `lxml/` harness.**  Both directories
  speak Python so they can share build steps, fixtures, and CI
  glue; `lxml/` validates a full downstream consumer end-to-end,
  `comparison/` validates the ABI function-by-function.

## What it produces

One table per category (parse / tree-walk / XPath / serialization),
each with these columns:

| Column | Meaning |
|---|---|
| `match` | `✓` if outputs identical; an inline comment if a known-harmless divergence exists; a red mark if unexplained |
| `sys`   | ns/call against system libxml2 |
| `ours`  | ns/call against sup-xml-compat |
| `ratio` | `sys / ours` — higher = we're faster |

A red row means **results differ AND the difference is not annotated
as known-harmless** — that's a real ABI bug to investigate.

## How to run

Requires Python 3 and a system libxml2 (`/usr/lib/libxml2.2.dylib`
on macOS; `libxml2.so.2` on Linux distros where it's installed).

```sh
# Build the shim first (the script doesn't do this for you):
cargo build -p sup-xml-compat --features cdylib-exports

# Then run:
python3 tests/abi-system/comparison/compare.py
```

No `DYLD_*` env tricks needed — both dylibs coexist in-process
via independent `ctypes.CDLL` handles.

## Contrast with the `lxml/` harness

- `lxml/` swaps our dylib in for the system libxml2 entirely and
  drives the real Python `lxml.etree` against it.  Tests "does a
  real downstream consumer survive?".
- `comparison/` keeps both dylibs loaded and calls each function
  directly with controlled inputs.  Tests "does each function
  return byte-identical results to the reference?".

Both are useful; they catch different bug classes.
