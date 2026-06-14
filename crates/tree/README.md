# sup-xml-tree

The in-memory document model for [SupXML](https://supso.org/projects/sup-xml)
— a libxml2-shaped arena DOM, plus the string dictionary used for name
interning.

**You rarely need this crate directly.** Everything is re-exported from the
top-level [`sup-xml`](https://crates.io/crates/sup-xml) crate, which is what
most users should depend on.

The DOM module contains a small, contained `unsafe` core for the
self-referential `Document` (it owns its arena and the root pointer into that
arena); the safety argument lives in the module docs, and the whole crate is
exercised under Miri in CI.

## License

SupXML is **source-available** software released through
[Supported Source](https://supso.org/). A valid license certificate is
required to use it; document parsing returns a fatal error without one (a
grace period applies after an existing certificate expires). Get a
certificate — free for individuals and non-monetized projects — at
[supso.org/projects/sup-xml](https://supso.org/projects/sup-xml). Full terms
are in the repository `LICENSE`.

## Documentation

- [Project docs](https://supso.org/projects/sup-xml/docs)
- [API reference (docs.rs)](https://docs.rs/sup-xml-tree)
- [Source on GitHub](https://github.com/SupsoOrg/sup-xml)
