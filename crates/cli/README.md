# sup-xml-cli

A command-line Swiss-army knife for XML, built on
[SupXML](https://supso.org/projects/sup-xml) — a memory-safe, fast,
spec-compliant XML toolkit written in Rust.

## Install

```bash
cargo install sup-xml-cli
```

This installs the `sup-xml` binary.

## Usage

```bash
sup-xml lint myfile.xml
sup-xml xpath '/catalog/book/@id' input.xml
sup-xml validate --schema schema.xsd instance.xml
sup-xml xslt --stylesheet style.xsl input.xml -o output.xml
sup-xml format --pretty input.xml             # re-emit pretty-printed
sup-xml stats input.xml                       # sizes, depths, counts
sup-xml c14n input.xml                        # Canonical XML / Exc-C14N
```

Run `sup-xml --help` or `sup-xml <command> --help` for the full flag
surface — including `--allow-fs` / `--allow-host` for DTD and entity
fetches, `--xinclude` for `<xi:include>` resolution, and `--html` for
HTML5 input.

## License

SupXML is **source-available** software released through
[Supported Source](https://supso.org/). A valid license certificate is
required to use it; document parsing returns a fatal error without one (a
grace period applies after an existing certificate expires). Get a
certificate — free for individuals and non-monetized projects — at
[supso.org/projects/sup-xml](https://supso.org/projects/sup-xml) and place it
where SupXML looks — `~/.supso/license_certificates/` or a project-local
`./.supso/license_certificates/`. Full terms are in the repository `LICENSE`.

## Documentation

- [Project docs](https://supso.org/projects/sup-xml/docs)
- [Source on GitHub](https://github.com/SupsoOrg/sup-xml)
