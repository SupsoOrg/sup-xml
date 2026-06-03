# Security Policy

SupXML is a memory-safe, libxml2-compatible XML library. Parsing
untrusted input safely is a primary design goal, so we take security
reports seriously.

## Reporting a Vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Email **help@supso.org** with the details. A useful report includes:

- the affected version (or commit) and crate (`sup-xml`, `sup-xml-core`,
  `sup-xml-compat`, etc.);
- a minimal XML/XSD/XSLT/DTD input or code snippet that reproduces the
  issue;
- what you observed (panic, crash, hang, out-of-bounds read, incorrect
  output, resource exhaustion, …) and what you expected;
- the build configuration if relevant (enabled features such as `xsd`,
  `xslt`, `html`, `c-abi`, `network-resolver`).

We aim to acknowledge reports within a few business days and to keep you
updated as we investigate and prepare a fix. Please give us a reasonable
window to release a fix before any public disclosure; we are happy to
credit reporters who would like acknowledgement.

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 1.0.x   | :white_check_mark: |

Security fixes land on the latest release line.

## Security Model

SupXML is built to process **untrusted input**, and the default
[`ParseOptions`] are the hardened stance — protections are opt-*out*,
never opt-in. The defaults defend against the standard XML attack
classes:

- **XXE / SSRF.** External DTD loading is off by default
  (`load_external_dtd = false`), and no external entity or DTD is
  fetched from the network or filesystem unless the caller explicitly
  installs an external resolver. An unresolved external reference is
  rejected, not silently expanded. No network client is wired into the
  default parse, XInclude, or entity-resolution paths.
- **Entity-expansion (billion laughs / quadratic blowup).** Total
  expanded entity text is capped (`max_entity_expansion_bytes`,
  default 1 MB).
- **Stack-exhaustion (deeply nested input).** Element nesting is bounded
  (`max_element_depth`, default 256); DTD content models and regular
  expressions (XSD `pattern` facets, XPath `matches`/`replace`/
  `tokenize`) are independently depth-bounded so malicious schemas and
  patterns cannot overflow the parser stack.
- **Memory safety.** The high-level Rust API crate (`sup-xml`) is
  `#![forbid(unsafe_code)]`; the parser cannot produce a buffer overflow
  or use-after-free regardless of input.

See the [security reference](docs/src/content/docs/reference/security.md)
for the full threat model and tuning guidance.

## Scope Notes

- **Opting into external/network loading** (custom resolvers, the
  `network-resolver` feature, `xmlRegisterInputCallbacks` handlers, or
  enabling `XML_PARSE_DTDLOAD` / `XML_PARSE_NOENT`) intentionally relaxes
  the default posture. When you opt in, validating and bounding what gets
  loaded (e.g. host allowlists to prevent SSRF) is the integrator's
  responsibility.
- **The `sup-xml-compat` C-ABI shim** is an `unsafe`, libxml2-shaped
  layer (raw pointers, `extern "C"`). Memory-safety issues there are
  in scope; please report them with the reproducing C/Python consumer
  where possible.

[`ParseOptions`]: https://docs.rs/sup-xml/latest/sup_xml/struct.ParseOptions.html
