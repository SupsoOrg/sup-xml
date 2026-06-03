//! Async I/O entry points — feature `tokio`.
//!
//! The parser itself is synchronous and CPU-bound; async support
//! here is the standard "slurp bytes via async I/O, then hand to
//! the existing parser" pattern.  It doesn't parse incrementally
//! across `.await` points — for that, use the streaming reader
//! ([`crate::Iterparse`] / [`crate::XmlReader`]) on bytes you've
//! already collected.
//!
//! # Example
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use sup_xml::async_io::parse_async;
//!
//! // Any `tokio::io::AsyncRead` works — `tokio::fs::File` (under
//! // the `fs` tokio feature), a TCP stream, a `&[u8]` cursor, etc.
//! let bytes: &[u8] = b"<r><a>hi</a></r>";
//! let doc = parse_async(bytes).await?;
//! println!("root: {}", doc.root().name());
//! # Ok(())
//! # }
//! ```
//!
//! Bounded-memory usage: callers that don't trust their input
//! should wrap the reader in `tokio::io::AsyncReadExt::take(MAX)`
//! before passing.

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::Result;
use crate::{parse_bytes, ParseOptions};
use sup_xml_tree::dom::Document;

/// Read `reader` to end asynchronously, then parse the bytes.
/// Uses default [`ParseOptions`].
pub async fn parse_async<R: AsyncRead + Unpin>(mut reader: R) -> Result<Document> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await.map_err(|e| crate::XmlError::new(
        crate::ErrorDomain::Io,
        crate::ErrorLevel::Error,
        format!("io error reading async source: {e}"),
    ))?;
    parse_bytes(&bytes, &ParseOptions::default())
}

/// Like [`parse_async`] but consults the supplied [`ParseOptions`]
/// (e.g. `recovery_mode: true`, `external_resolver: Some(...)`).
pub async fn parse_async_with<R: AsyncRead + Unpin>(
    mut reader: R, opts: &ParseOptions,
) -> Result<Document> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await.map_err(|e| crate::XmlError::new(
        crate::ErrorDomain::Io,
        crate::ErrorLevel::Error,
        format!("io error reading async source: {e}"),
    ))?;
    parse_bytes(&bytes, opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn parse_from_cursor() {
        let bytes = b"<r><a>1</a></r>".to_vec();
        let doc = parse_async(&bytes[..]).await.expect("parse");
        assert_eq!(doc.root().name(), "r");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn malformed_input_errors() {
        let bytes = b"<r><unclosed>".to_vec();
        let r = parse_async(&bytes[..]).await;
        assert!(r.is_err());
    }
}
