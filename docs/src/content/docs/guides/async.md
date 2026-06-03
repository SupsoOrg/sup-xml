---
title: Async I/O
description: Parse XML from any tokio AsyncRead — network sockets, async file handles, multipart bodies.
---

The `tokio` feature exposes `parse_async` and friends, which accept any
`tokio::io::AsyncRead + Unpin` source and parse without blocking the
runtime thread.

## Enabling

```toml
[dependencies]
sup-xml = { version = "*", features = ["tokio"] }
tokio = { version = "1", features = ["rt", "io-util"] }
```

## Parse from any AsyncRead

```rust
use sup_xml::async_io::parse_async;

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let bytes: &[u8] = b"<r><b/></r>";   // `&[u8]` implements `AsyncRead`
let doc = parse_async(bytes).await?;
# Ok(())
# }
```

Any `AsyncRead` works — a `tokio::net::TcpStream`, a `tokio::fs::File`
(with the `fs` tokio feature), the body of an `axum` / `hyper` request,
an `S3` byte-stream, a chunked HTTP response, etc.

## Custom options

```rust
use sup_xml::{async_io::parse_async_with, ParseOptions};

# async fn run(file: tokio::fs::File) -> Result<(), Box<dyn std::error::Error>> {
let opts = ParseOptions {
    recovery_mode: true,
    skip_inter_element_whitespace: true,
    ..Default::default()
};
let doc = sup_xml::async_io::parse_async_with(file, &opts).await?;
# Ok(())
# }
```

## Streaming and untrusted input

For arbitrarily large payloads, cap the reader before handing it to
`parse_async` so a malicious upstream can't OOM your process:

```rust
use tokio::io::AsyncReadExt;

# async fn handler(body: impl tokio::io::AsyncRead + Unpin) -> Result<sup_xml::Document, Box<dyn std::error::Error>> {
const MAX_BODY: u64 = 10 * 1024 * 1024;
let capped = body.take(MAX_BODY);
let doc = sup_xml::async_io::parse_async(capped).await?;
# Ok(doc)
# }
```

`parse_async` reads the entire stream into a buffer before invoking the
sync parser — XML is not a streamable grammar in the general case (a
late `<!DOCTYPE>` can change the meaning of earlier entities). For
genuinely streaming use cases where the document IS line-delimited or
NDJSON-style, drive `XmlBytesReader` directly from the
[parsing guide](/guides/parsing/#streaming).

## Threading model

`parse_async` is `Send`-safe and works under any tokio runtime
(multi-thread or current-thread). The underlying parser is synchronous
once the bytes are buffered — async only governs *how* the bytes arrive.
If your input is local memory, prefer the sync `parse_str` /
`parse_bytes`; the async wrapper is for cases where the bytes are
*remote* and reading them blocks your thread.
