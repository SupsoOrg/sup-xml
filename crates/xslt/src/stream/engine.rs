//! Streamed execution — burst mode (XSLT 3.0 §18.1).
//!
//! This is the first of the streamed-evaluation strategies.  It drives a
//! [`StreamParser`] over the source, which materializes one *record*
//! subtree at a time — an element selected by a root-anchored path — and
//! frees it before reading the next.  Each record is handed to the
//! ordinary tree evaluator in a chosen mode, so the full XSLT/XPath
//! engine processes it, and the per-record results are concatenated.
//!
//! Memory is bounded by the largest single record rather than the whole
//! document: a multi-gigabyte document of millions of small records is
//! transformed without ever holding more than one record's tree.  This
//! is the dominant streaming workload ("split a huge record-oriented
//! document, process each record").
//!
//! What this mode does *not* yet do: process an individual record
//! incrementally (a single record larger than memory), or stream the
//! *output*.  Those are later refinements; the static streamability
//! analysis in [`super::analysis`] already gates the constructs that a
//! fully-incremental evaluator will need.  The source here is an
//! in-memory `&str`/`&[u8]`; wiring the byte-level
//! [`sup_xml_core::streaming_reader::XmlByteStreamReader`] in for a
//! bounded-memory *source* is the next step.

use sup_xml_core::StreamParser;

use crate::error::XsltError;
use crate::result_tree::ResultTree;
use crate::Stylesheet;

/// Selects which elements of the streamed source become records.
///
/// All three forms are *ancestor-bounded*: the decision uses only the
/// just-opened element's name and its ancestor chain, so elements above
/// the record boundary cost only a small ancestor stack regardless of
/// document size.
pub enum RecordSelector<'a> {
    /// Records are the elements at a fixed depth below the root
    /// (`depth = 1` is the document element's children).
    Depth(u32),
    /// Records are the elements whose root-anchored ancestor chain
    /// matches this path exactly, e.g. `["data", "item"]`.
    Path(&'a [&'a str]),
}

impl Stylesheet {
    /// Transform `source` in burst-streaming mode: pull each record
    /// selected by `records` from the source one at a time, apply this
    /// stylesheet's templates to it in `mode`, and concatenate the
    /// results.  `mode` names an `xsl:mode` (typically a
    /// `streamable="yes"` one whose rules the compiler has already
    /// verified streamable); `None` is the unnamed default mode.
    ///
    /// Memory is bounded by the largest single record, not the whole
    /// document.  See the [module docs](self) for the current limits of
    /// this strategy.
    pub fn stream_apply(
        &self,
        source:  &str,
        records: RecordSelector<'_>,
        mode:    Option<&str>,
    ) -> Result<ResultTree, XsltError> {
        let mut sp = match records {
            RecordSelector::Depth(d) => StreamParser::from_str(source).emit_at_depth(d),
            RecordSelector::Path(p)  => StreamParser::from_str(source).emit_at_path(p),
        };

        let mut children = Vec::new();
        let mut secondary = Vec::new();
        let mut output = None;
        let mut character_map = Vec::new();

        while let Some(record) = sp.next().map_err(XsltError::from)? {
            let mut rt = self.apply_with_params_initial_and_mode(
                &record,
                &crate::loader::NullLoader,
                None,
                &[],
                None,
                mode,
            )?;
            if output.is_none() {
                output = Some(rt.output.clone());
                character_map = std::mem::take(&mut rt.character_map);
            }
            children.append(&mut rt.children);
            secondary.append(&mut rt.secondary);
        }

        // With no records the loop never observed the stylesheet's
        // serialization settings; recover them with a probe apply over
        // an empty document so an empty stream serializes the same way a
        // populated one would (e.g. honoring omit-xml-declaration).
        let output = match output {
            Some(o) => o,
            None => {
                let probe = sup_xml_core::parse_str(
                    "<_/>",
                    &sup_xml_core::ParseOptions::default(),
                )
                .map_err(XsltError::from)?;
                let mut rt = self.apply_with_params_initial_and_mode(
                    &probe,
                    &crate::loader::NullLoader,
                    None,
                    &[],
                    None,
                    mode,
                )?;
                character_map = std::mem::take(&mut rt.character_map);
                rt.output
            }
        };

        Ok(ResultTree { children, output, character_map, secondary })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STYLE: &str = r#"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:mode name="s" streamable="yes"/>
        <xsl:template match="item" mode="s">
            <got id="{@id}"><xsl:value-of select="name"/></got>
        </xsl:template>
    </xsl:stylesheet>"#;

    fn doc(n: usize) -> String {
        let mut s = String::from("<data>");
        for i in 0..n {
            s.push_str(&format!("<item id=\"{i}\"><name>n{i}</name></item>"));
        }
        s.push_str("</data>");
        s
    }

    #[test]
    fn burst_applies_mode_per_record() {
        let style = Stylesheet::compile_str(STYLE).unwrap();
        let out = style
            .stream_apply(&doc(3), RecordSelector::Path(&["data", "item"]), Some("s"))
            .unwrap()
            .to_string()
            .unwrap();
        assert_eq!(
            out,
            r#"<got id="0">n0</got><got id="1">n1</got><got id="2">n2</got>"#
        );
    }

    #[test]
    fn depth_selector_matches_root_children() {
        let style = Stylesheet::compile_str(STYLE).unwrap();
        let out = style
            .stream_apply(&doc(2), RecordSelector::Depth(1), Some("s"))
            .unwrap()
            .to_string()
            .unwrap();
        assert_eq!(out, r#"<got id="0">n0</got><got id="1">n1</got>"#);
    }

    #[test]
    fn empty_stream_yields_empty_result() {
        let style = Stylesheet::compile_str(STYLE).unwrap();
        let out = style
            .stream_apply("<data/>", RecordSelector::Path(&["data", "item"]), Some("s"))
            .unwrap()
            .to_string()
            .unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn handles_many_records() {
        // A document far larger than any single record — burst mode
        // processes it one record at a time.
        let style = Stylesheet::compile_str(STYLE).unwrap();
        let out = style
            .stream_apply(&doc(5000), RecordSelector::Path(&["data", "item"]), Some("s"))
            .unwrap()
            .to_string()
            .unwrap();
        assert!(out.starts_with(r#"<got id="0">n0</got>"#));
        assert!(out.ends_with(r#"<got id="4999">n4999</got>"#));
        assert_eq!(out.matches("<got ").count(), 5000);
    }

    #[test]
    fn streamed_matches_non_streamed_equivalent() {
        // The burst result must equal applying the same mode to the
        // whole document with an ordinary apply-templates driver.
        let style = Stylesheet::compile_str(STYLE).unwrap();
        let streamed = style
            .stream_apply(&doc(4), RecordSelector::Path(&["data", "item"]), Some("s"))
            .unwrap()
            .to_string()
            .unwrap();

        let reference_style = r#"<xsl:stylesheet version="3.0"
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
            <xsl:output method="xml" omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <xsl:apply-templates select="data/item" mode="s"/>
            </xsl:template>
            <xsl:template match="item" mode="s">
                <got id="{@id}"><xsl:value-of select="name"/></got>
            </xsl:template>
        </xsl:stylesheet>"#;
        let rstyle = Stylesheet::compile_str(reference_style).unwrap();
        let src = sup_xml_core::parse_str(&doc(4), &sup_xml_core::ParseOptions::default()).unwrap();
        let reference = rstyle.apply(&src).unwrap().to_string().unwrap();

        assert_eq!(streamed, reference);
    }
}
