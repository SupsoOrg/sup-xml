//! Incremental (non-grounding) streamed execution — XSLT 3.0 §19.
//!
//! The burst engine in [`super::engine`] materializes one record subtree
//! at a time and hands it to the tree evaluator: memory is bounded by the
//! largest *record*, not the document.  That is enough for record-oriented
//! data, but a single record larger than memory still cannot be processed.
//!
//! This module is the first execution path that never grounds: it drives
//! the byte-level [`XmlByteStreamReader`] and echoes matched elements'
//! subtrees straight to a [`Write`] sink as they stream past, holding only
//! the ancestor-name stack.  Memory is bounded by document *depth* — a
//! single multi-gigabyte record streams through in constant space, and the
//! output is streamed too (it is never accumulated into a result tree).
//!
//! What it implements is the streamed **identity/extract** transform: copy
//! through every element selected by a [`RecordSelector`], skipping the
//! rest.  This is the guaranteed-streamable `xsl:copy-of` of the current
//! node, the simplest of the streamable bodies the analyzer in
//! [`super::analysis`] accepts.  Template-driven incremental transformation
//! (motionless projection, downward `apply-templates` recursion with a
//! streamed result) builds on this same event loop and is the remaining
//! frontier.

use std::io::{Read, Write};

use sup_xml_core::xml_bytes_reader::BytesEvent;
use sup_xml_core::streaming_reader::{XmlByteStreamReader, DEFAULT_BUFFER_SIZE};

use super::engine::RecordSelector;
use crate::error::XsltError;

/// Stream `source`, writing the serialized subtree of every element that
/// matches `records` to `out`, and skipping everything else.  Returns the
/// number of records copied.
///
/// Memory is bounded by document depth: a record of any size streams
/// through without being materialized, and the output is written as it is
/// produced rather than accumulated.  This is the non-grounding execution
/// of a streamed identity copy (`xsl:copy-of select="."` per record).
pub fn stream_copy<R: Read, W: Write>(
    source:  R,
    out:     &mut W,
    records: RecordSelector<'_>,
) -> Result<usize, XsltError> {
    let mut reader = XmlByteStreamReader::new(source, DEFAULT_BUFFER_SIZE)?;
    let mut names: Vec<String> = Vec::new();
    // `Some(len)` while copying: the ancestor-stack length at which the
    // current record started, so we know which end tag closes it.
    let mut copying_from: Option<usize> = None;
    let mut count = 0usize;

    loop {
        match reader.next_event()? {
            BytesEvent::StartElement(tag) => {
                let name = bytes_to_string(tag.name())?;
                // Collect attributes before pushing the name so a fresh
                // record's start tag is serialized with them.
                let mut attrs: Vec<(String, String)> = Vec::new();
                for a in tag.attrs() {
                    let a = a?;
                    attrs.push((bytes_to_string(a.name)?, bytes_to_string(&a.value)?));
                }
                names.push(name);

                if copying_from.is_none() && record_matches(&records, &names) {
                    copying_from = Some(names.len());
                    count += 1;
                }
                if copying_from.is_some() {
                    write_start_tag(out, names.last().unwrap(), &attrs)?;
                }
            }
            BytesEvent::EndElement(_) => {
                if copying_from.is_some() {
                    write_end_tag(out, names.last().unwrap())?;
                    if copying_from == Some(names.len()) {
                        copying_from = None;
                    }
                }
                names.pop();
            }
            BytesEvent::Text(t) => {
                if copying_from.is_some() {
                    write_escaped_text(out, &bytes_to_string(t.as_bytes())?)?;
                }
            }
            BytesEvent::CData(t) => {
                if copying_from.is_some() {
                    write_all(out, "<![CDATA[")?;
                    write_all(out, &bytes_to_string(t.as_bytes())?)?;
                    write_all(out, "]]>")?;
                }
            }
            BytesEvent::Comment(t) => {
                if copying_from.is_some() {
                    write_all(out, "<!--")?;
                    write_all(out, &bytes_to_string(t.as_bytes())?)?;
                    write_all(out, "-->")?;
                }
            }
            BytesEvent::Pi(p) => {
                if copying_from.is_some() {
                    write_all(out, "<?")?;
                    write_all(out, &bytes_to_string(p.target())?)?;
                    let content = bytes_to_string(p.content())?;
                    if !content.is_empty() {
                        write_all(out, " ")?;
                        write_all(out, &content)?;
                    }
                    write_all(out, "?>")?;
                }
            }
            BytesEvent::EntityRef(e) => {
                if copying_from.is_some() {
                    write_all(out, "&")?;
                    write_all(out, &bytes_to_string(e.name())?)?;
                    write_all(out, ";")?;
                }
            }
            BytesEvent::Eof => break,
        }
    }
    Ok(count)
}

/// Does the current ancestor-name stack identify a record to copy?
fn record_matches(records: &RecordSelector<'_>, names: &[String]) -> bool {
    match records {
        RecordSelector::Depth(d) => names.len() as u32 == d + 1,
        RecordSelector::Path(p) => {
            p.len() == names.len() && names.iter().zip(p.iter()).all(|(a, b)| a == b)
        }
    }
}

fn write_start_tag<W: Write>(out: &mut W, name: &str, attrs: &[(String, String)]) -> Result<(), XsltError> {
    write_all(out, "<")?;
    write_all(out, name)?;
    for (an, av) in attrs {
        write_all(out, " ")?;
        write_all(out, an)?;
        write_all(out, "=\"")?;
        write_escaped_attr(out, av)?;
        write_all(out, "\"")?;
    }
    write_all(out, ">")
}

fn write_end_tag<W: Write>(out: &mut W, name: &str) -> Result<(), XsltError> {
    write_all(out, "</")?;
    write_all(out, name)?;
    write_all(out, ">")
}

fn write_escaped_text<W: Write>(out: &mut W, s: &str) -> Result<(), XsltError> {
    for ch in s.chars() {
        match ch {
            '&' => write_all(out, "&amp;")?,
            '<' => write_all(out, "&lt;")?,
            '>' => write_all(out, "&gt;")?,
            _   => write_char(out, ch)?,
        }
    }
    Ok(())
}

fn write_escaped_attr<W: Write>(out: &mut W, s: &str) -> Result<(), XsltError> {
    for ch in s.chars() {
        match ch {
            '&'  => write_all(out, "&amp;")?,
            '<'  => write_all(out, "&lt;")?,
            '"'  => write_all(out, "&quot;")?,
            '\t' => write_all(out, "&#9;")?,
            '\n' => write_all(out, "&#10;")?,
            '\r' => write_all(out, "&#13;")?,
            _    => write_char(out, ch)?,
        }
    }
    Ok(())
}

fn write_all<W: Write>(out: &mut W, s: &str) -> Result<(), XsltError> {
    out.write_all(s.as_bytes()).map_err(io_err)
}

fn write_char<W: Write>(out: &mut W, ch: char) -> Result<(), XsltError> {
    let mut buf = [0u8; 4];
    out.write_all(ch.encode_utf8(&mut buf).as_bytes()).map_err(io_err)
}

fn io_err(e: std::io::Error) -> XsltError {
    XsltError::InvalidStylesheet(format!("stream_copy: write error: {e}"))
}

fn bytes_to_string(b: &[u8]) -> Result<String, XsltError> {
    std::str::from_utf8(b)
        .map(str::to_owned)
        .map_err(|_| XsltError::InvalidStylesheet("stream_copy: invalid UTF-8 in source".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn copy(src: &str, records: RecordSelector) -> (String, usize) {
        let mut out = Vec::new();
        let n = stream_copy(Cursor::new(src.as_bytes().to_vec()), &mut out, records).unwrap();
        (String::from_utf8(out).unwrap(), n)
    }

    #[test]
    fn copies_matching_records_skips_rest() {
        let src = "<data><meta>x</meta><item id=\"1\"><v>a</v></item>\
                   <item id=\"2\"><v>b</v></item></data>";
        let (out, n) = copy(src, RecordSelector::Path(&["data", "item"]));
        assert_eq!(n, 2);
        assert_eq!(out, "<item id=\"1\"><v>a</v></item><item id=\"2\"><v>b</v></item>");
    }

    #[test]
    fn escapes_text_and_attributes() {
        let src = "<data><item k=\"a&amp;b&lt;c\">x &lt; y &amp; z</item></data>";
        let (out, _) = copy(src, RecordSelector::Path(&["data", "item"]));
        assert_eq!(out, "<item k=\"a&amp;b&lt;c\">x &lt; y &amp; z</item>");
    }

    #[test]
    fn preserves_nested_structure_and_namespaces() {
        let src = r#"<data><item xmlns:p="urn:x"><p:c a="1">t</p:c></item></data>"#;
        let (out, n) = copy(src, RecordSelector::Path(&["data", "item"]));
        assert_eq!(n, 1);
        assert_eq!(out, r#"<item xmlns:p="urn:x"><p:c a="1">t</p:c></item>"#);
    }

    #[test]
    fn depth_selector() {
        let src = "<r><a/><b><c/></b></r>";
        let (out, n) = copy(src, RecordSelector::Depth(1));
        assert_eq!(n, 2);
        assert_eq!(out, "<a></a><b><c></c></b>");
    }

    #[test]
    fn bounded_memory_over_huge_record() {
        // A single record far larger than any reasonable window streams
        // through; the only growth is the (shallow) ancestor stack.
        let mut src = String::from("<data><item>");
        for i in 0..20_000 {
            src.push_str(&format!("<row>{i}</row>"));
        }
        src.push_str("</item></data>");
        let (out, n) = copy(&src, RecordSelector::Path(&["data", "item"]));
        assert_eq!(n, 1);
        assert!(out.starts_with("<item><row>0</row>"));
        assert!(out.ends_with("<row>19999</row></item>"));
        assert_eq!(out.matches("<row>").count(), 20_000);
    }

    #[test]
    fn no_match_yields_empty() {
        let (out, n) = copy("<data><other/></data>", RecordSelector::Path(&["data", "item"]));
        assert_eq!(n, 0);
        assert_eq!(out, "");
    }
}
