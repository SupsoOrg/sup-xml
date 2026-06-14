# sup-xml-core

The implementation heart of [SupXML](https://supso.org/projects/sup-xml) —
XML parsing, serialization, namespace resolution, and XPath 1.0 evaluation,
plus optional XSD, HTML5, and network-resolver support behind feature flags.

**Most users should depend on [`sup-xml`](https://crates.io/crates/sup-xml)
instead.** That crate re-exports everything here through a stable, idiomatic
public surface. This crate is the internal engine and its API may move faster
than the top-level one.

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
- [API reference (docs.rs)](https://docs.rs/sup-xml-core)
- [Source on GitHub](https://github.com/SupsoOrg/sup-xml)
