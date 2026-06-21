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

use std::io::Read;

use sup_xml_core::xpath::ast::{Axis, Expr, LocationPath, NodeTest};
use sup_xml_core::{ByteStreamParser, StreamParser};
use sup_xml_tree::dom::Document;

use crate::ast::{Body, Instr};
use crate::error::XsltError;
use crate::loader::Loader;
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
        self.run_records(mode, || sp.next().map_err(XsltError::from))
    }

    /// Run a stylesheet whose transform is driven by an
    /// `xsl:source-document streamable="yes"` declaration: locate that
    /// declaration, open its (static) `href=` through `loader` as a byte
    /// stream, and apply its templates record-by-record in bounded memory.
    ///
    /// This is the "the transform IS a streamed source-document" entry
    /// point (the W3C streaming convention, where an initial template opens
    /// a streamed source and applies templates).  The streaming parameters
    /// — which elements are records, and in which mode — are derived from
    /// the source-document's body, so the caller need only supply the
    /// `loader` that resolves the `href`.
    ///
    /// Returns an error if the stylesheet has no streamable source-document
    /// with a static `href=` and a direct downward `xsl:apply-templates`
    /// body (the shape this engine streams); such stylesheets should use
    /// the ordinary [`apply`](Self::apply) path or the explicit
    /// [`stream_apply_reader`](Self::stream_apply_reader).
    pub fn apply_streaming(
        &self,
        loader: &dyn Loader,
        base:   Option<&str>,
    ) -> Result<ResultTree, XsltError> {
        let plan = find_stream_plan(&self.ast).ok_or_else(|| {
            XsltError::InvalidStylesheet(
                "apply_streaming: found no xsl:source-document streamable=\"yes\" with a \
                 static href and a direct downward xsl:apply-templates body"
                    .to_string(),
            )
        })?;
        let read = loader.open(&plan.href, base)?;
        let path: Vec<&str> = plan.path.iter().map(String::as_str).collect();
        self.stream_apply_reader(read, RecordSelector::Path(&path), plan.mode.as_deref())
    }

    /// Like [`stream_apply`](Self::stream_apply) but reads from any
    /// [`io::Read`](std::io::Read) source through a bounded rolling buffer,
    /// so neither the source nor the working set is ever fully resident —
    /// total memory is bounded by `max(buffer, largest record)` regardless
    /// of document size.  This is the form for multi-gigabyte inputs from a
    /// file or pipe.
    pub fn stream_apply_reader<R: Read>(
        &self,
        source:  R,
        records: RecordSelector<'_>,
        mode:    Option<&str>,
    ) -> Result<ResultTree, XsltError> {
        let mut sp = match records {
            RecordSelector::Depth(d) => ByteStreamParser::new(source)?.emit_at_depth(d),
            RecordSelector::Path(p)  => ByteStreamParser::new(source)?.emit_at_path(p),
        };
        self.run_records(mode, || sp.next().map_err(XsltError::from))
    }

    /// Drive a record source — `next_record` yields one materialized record
    /// subtree at a time — applying this stylesheet's `mode` templates to
    /// each and concatenating the results.  Shared by the `&str` and
    /// `io::Read` entry points; the only difference between them is the
    /// record source.
    fn run_records<F>(
        &self,
        mode:        Option<&str>,
        mut next_record: F,
    ) -> Result<ResultTree, XsltError>
    where
        F: FnMut() -> Result<Option<Document>, XsltError>,
    {
        // Carry streamable accumulators' running values across records for
        // the duration of this streamed run (XSLT 3.0 §18.2), then clear.
        crate::eval::begin_streamed_accumulators();
        let result = self.run_records_inner(mode, &mut next_record);
        crate::eval::end_streamed_accumulators();
        result
    }

    fn run_records_inner<F>(
        &self,
        mode:            Option<&str>,
        mut next_record: F,
    ) -> Result<ResultTree, XsltError>
    where
        F: FnMut() -> Result<Option<Document>, XsltError>,
    {
        let mut children = Vec::new();
        let mut secondary = Vec::new();
        let mut output = None;
        let mut character_map = Vec::new();

        while let Some(record) = next_record()? {
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
        // serialization settings; recover them with a probe apply over an
        // empty document so an empty stream serializes the same way a
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

/// The streaming parameters recovered from a stylesheet's
/// `xsl:source-document streamable="yes"` declaration.
struct StreamPlan {
    /// The static `href=` of the source-document.
    href: String,
    /// Root-anchored element-name path of the records to stream.
    path: Vec<String>,
    /// Mode (lexical QName) the records are processed in, if named.
    mode: Option<String>,
}

/// Find the first streamable source-document in any template whose body
/// has the shape this engine can stream (a static `href=` and a direct
/// downward `xsl:apply-templates`), and recover its streaming plan.
fn find_stream_plan(ast: &crate::ast::StylesheetAst) -> Option<StreamPlan> {
    ast.templates.iter().find_map(|t| find_in_body(&t.body))
}

fn find_in_body(body: &Body) -> Option<StreamPlan> {
    for instr in body.instrs() {
        if let Instr::SourceDocument { streamable: true, href, body: sd_body } = instr {
            if let (Some(href), Some((path, mode))) =
                (href.as_literal(), derive_records(sd_body))
            {
                return Some(StreamPlan { href, path, mode });
            }
        }
        for child in child_bodies(instr) {
            if let Some(plan) = find_in_body(child) {
                return Some(plan);
            }
        }
    }
    None
}

/// Recover `(record-path, mode)` from a source-document body that is a
/// direct downward `xsl:apply-templates`.  Returns `None` for any shape
/// outside this first-cut streamable form.
fn derive_records(body: &Body) -> Option<(Vec<String>, Option<String>)> {
    body.instrs().iter().find_map(|instr| match instr {
        Instr::ApplyTemplates { select: Some(sel), mode, mode_current: false, .. } => {
            let path = derive_path(sel)?;
            Some((path, mode.as_ref().map(|q| q.to_qname_string())))
        }
        _ => None,
    })
}

/// Convert a relative downward name-only path (`a/b/c`) into the
/// root-anchored element-name list a [`StreamParser`] path matches.
/// Returns `None` for anything with predicates, non-child axes,
/// wildcards, or namespace-prefixed tests (unsupported in this first
/// cut).
fn derive_path(select: &Expr) -> Option<Vec<String>> {
    let steps = match select {
        Expr::Path(LocationPath::Relative(steps)) => steps,
        _ => return None,
    };
    let mut names = Vec::with_capacity(steps.len());
    for step in steps {
        if step.filter.is_some() || !step.predicates.is_empty() || step.axis != Axis::Child {
            return None;
        }
        match &step.node_test {
            NodeTest::LocalName(n) => names.push(n.clone()),
            _ => return None,
        }
    }
    (!names.is_empty()).then_some(names)
}

/// The nested sequence-constructor bodies of an instruction, for the
/// recursive source-document search.
fn child_bodies(instr: &Instr) -> Vec<&Body> {
    use Instr::*;
    match instr {
        SourceDocument { body, .. } | LiteralElement { body, .. } | If { body, .. }
        | ForEach { body, .. } | Copy { body, .. } | Element { body, .. }
        | Attribute { body, .. } | Comment { body, .. } | ProcessingInstruction { body, .. }
        | Message { body, .. } | Assert { body, .. } | Fallback { body } | Map { body }
        | MapEntry { body, .. } | ForEachGroup { body, .. } | OnEmpty { body }
        | OnNonEmpty { body } | WherePopulated { body } | Fork { body }
        | PerformSort { body, .. } | Document { body } | ResultDocument { body, .. }
        | Namespace { body, .. } | ValueOfBody { body, .. } | Break { body, .. } => vec![body],
        Variable(v) => vec![&v.body],
        Choose { whens, otherwise } => {
            let mut v: Vec<&Body> = whens.iter().map(|(_, b)| b).collect();
            v.extend(otherwise.as_ref());
            v
        }
        Iterate { params, on_completion, body, .. } => {
            let mut v: Vec<&Body> = params.iter().map(|p| &p.body).collect();
            v.push(on_completion);
            v.push(body);
            v
        }
        AnalyzeString { matching, non_matching, .. } => vec![matching, non_matching],
        Merge { action, .. } => vec![action],
        Try { body, catches } => {
            let mut v = vec![body];
            v.extend(catches.iter().map(|c| &c.body));
            v
        }
        ApplyTemplates { with_params, .. } | CallTemplate { with_params, .. }
        | ApplyImports { with_params } | NextMatch { with_params }
        | NextIteration { with_params } | Evaluate { with_params, .. } => {
            with_params.iter().map(|w| &w.body).collect()
        }
        _ => vec![],
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
    fn streaming_accumulator_carries_across_records() {
        // A streamable accumulator's running value must continue from one
        // record to the next, not restart from initial-value per record.
        let style = Stylesheet::compile_str(
            r#"<xsl:stylesheet version="3.0"
                xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
                <xsl:output method="xml" omit-xml-declaration="yes"/>
                <xsl:accumulator name="n" initial-value="0" streamable="yes">
                    <xsl:accumulator-rule match="item" select="$value + 1"/>
                </xsl:accumulator>
                <xsl:mode name="s" streamable="yes"/>
                <xsl:template match="item" mode="s">
                    <c><xsl:value-of select="accumulator-before('n')"/></c>
                </xsl:template>
            </xsl:stylesheet>"#,
        )
        .unwrap();
        let out = style
            .stream_apply(&doc(4), RecordSelector::Path(&["data", "item"]), Some("s"))
            .unwrap()
            .to_string()
            .unwrap();
        // Distinct, increasing per record — the accumulator did not reset.
        assert_eq!(out, "<c>1</c><c>2</c><c>3</c><c>4</c>");
    }

    #[test]
    fn streaming_accumulator_running_sum() {
        let style = Stylesheet::compile_str(
            r#"<xsl:stylesheet version="3.0"
                xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
                <xsl:output method="xml" omit-xml-declaration="yes"/>
                <xsl:accumulator name="total" initial-value="0" streamable="yes">
                    <xsl:accumulator-rule match="item" select="$value + xs:integer(@v)"/>
                </xsl:accumulator>
                <xsl:mode name="s" streamable="yes"/>
                <xsl:template match="item" mode="s">
                    <t><xsl:value-of select="accumulator-after('total')"/></t>
                </xsl:template>
            </xsl:stylesheet>"#,
        )
        .unwrap();
        let src = "<data><item v=\"10\"/><item v=\"5\"/><item v=\"7\"/></data>";
        let out = style
            .stream_apply(src, RecordSelector::Path(&["data", "item"]), Some("s"))
            .unwrap()
            .to_string()
            .unwrap();
        assert_eq!(out, "<t>10</t><t>15</t><t>22</t>");
    }

    #[test]
    fn apply_streaming_derives_plan_from_source_document() {
        // The stylesheet drives streaming itself via xsl:source-document;
        // apply_streaming finds it, opens the href via the loader, and
        // streams records — no explicit RecordSelector from the caller.
        let style = Stylesheet::compile_str(
            r#"<xsl:stylesheet version="3.0"
                xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
                <xsl:output method="xml" omit-xml-declaration="yes"/>
                <xsl:mode name="s" streamable="yes"/>
                <xsl:template name="main">
                    <xsl:source-document href="big.xml" streamable="yes">
                        <xsl:apply-templates select="data/item" mode="s"/>
                    </xsl:source-document>
                </xsl:template>
                <xsl:template match="item" mode="s">
                    <got id="{@id}"/>
                </xsl:template>
            </xsl:stylesheet>"#,
        )
        .unwrap();

        let loader = crate::loader::InMemoryLoader::new().with("big.xml", doc(3));
        let out = style
            .apply_streaming(&loader, None)
            .unwrap()
            .to_string()
            .unwrap();
        assert_eq!(out, r#"<got id="0"/><got id="1"/><got id="2"/>"#);
    }

    #[test]
    fn apply_streaming_rejects_unstreamable_stylesheet() {
        // No streamable source-document → a clear error, not a panic.
        let style = Stylesheet::compile_str(STYLE).unwrap();
        let loader = crate::loader::InMemoryLoader::new();
        assert!(style.apply_streaming(&loader, None).is_err());
    }

    #[test]
    fn reader_source_matches_str_source() {
        // The io::Read entry must produce the same result as the &str
        // entry — and works on a document far larger than its buffer.
        let style = Stylesheet::compile_str(STYLE).unwrap();
        let d = doc(1000);
        let from_str = style
            .stream_apply(&d, RecordSelector::Path(&["data", "item"]), Some("s"))
            .unwrap()
            .to_string()
            .unwrap();
        let from_reader = style
            .stream_apply_reader(
                std::io::Cursor::new(d.into_bytes()),
                RecordSelector::Path(&["data", "item"]),
                Some("s"),
            )
            .unwrap()
            .to_string()
            .unwrap();
        assert_eq!(from_str, from_reader);
        assert_eq!(from_reader.matches("<got ").count(), 1000);
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
