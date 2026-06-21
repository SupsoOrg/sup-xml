//! General template-driven incremental evaluation (XSLT 3.0 §19).
//!
//! Where [`super::engine`]'s burst mode grounds one record at a time and
//! hands it to the tree evaluator, this module never grounds: it drives
//! the byte-level [`XmlByteStreamReader`] and executes a streamable mode's
//! template rules directly against the event stream, so an individual
//! record larger than memory is transformed in O(depth) working space.
//!
//! # How it reuses the existing engine
//!
//! The only data a *motionless* expression or a match pattern may touch
//! is the streaming window: the current node, its attributes/namespaces,
//! and its ancestors (XSLT 3.0 §19.4 — the climbing posture).  This module
//! materializes exactly that window — the ancestor-or-self spine, with
//! attributes — as a tiny [`DocIndex`] and evaluates patterns and
//! motionless expressions with the unmodified core XPath engine
//! ([`pattern::matches`] / [`eval_expr`]).  So motionless XPath is fully
//! general; only the *consuming* operations are special-cased to read from
//! the event stream:
//!
//! * `xsl:apply-templates select="<downward>"` recurses into matching
//!   children as their events arrive,
//! * `xsl:copy-of select="."` deep-copies the current subtree from events,
//! * `xsl:value-of select="."` / `string(.)` accumulates descendant text.
//!
//! # Scope and fallback
//!
//! [`push_eligible`] decides whether a streamable mode's rules fit the
//! instruction subset implemented here (literal result elements,
//! `xsl:value-of`, `xsl:copy-of` of the current node, downward
//! `xsl:apply-templates`, `xsl:if`, `xsl:choose`, with motionless
//! selects/tests and in-mode recursion).  When they do not, the caller
//! falls back to burst, which handles arbitrary streamable templates
//! (bounded by record rather than depth).  Nothing is ever half-applied.

use std::collections::HashMap;
use std::io::Read;

use sup_xml_core::streaming_reader::{XmlByteStreamReader, DEFAULT_BUFFER_SIZE};
use sup_xml_core::xml_bytes_reader::BytesEvent;
use sup_xml_core::xpath::ast::{Axis, Expr, LocationPath, NodeTest};
use sup_xml_core::xpath::eval::{eval_expr, value_to_bool, value_to_string, EvalCtx, StaticContext, XPathBindings};
use sup_xml_core::xpath::{DocIndex, DocIndexLike, NodeId};
use sup_xml_tree::dom::{DocumentBuilder, Node};

use super::analysis::{analyze_expr, Posture, Sweep};
use crate::ast::{Avt, AvtPart, Body, Instr, QName, StylesheetAst, Template};
use crate::error::XsltError;
use crate::pattern;
use crate::result_tree::{ResultBuilder, ResultNode};

/// Run a streamable mode incrementally over `reader`, applying templates
/// to every element whose root-anchored ancestor path equals
/// `record_path`, and return the produced result nodes.  Memory is
/// bounded by document depth, not by record or document size.
pub fn stream_apply_push<R: Read>(
    style:       &StylesheetAst,
    reader:      R,
    record_path: &[String],
    mode:        Option<&QName>,
) -> Result<Vec<ResultNode>, XsltError> {
    let mut ev = PushEval {
        reader:      XmlByteStreamReader::new(reader, DEFAULT_BUFFER_SIZE)?,
        builder:     ResultBuilder::new(),
        frames:      Vec::new(),
        style,
        mode:        mode.cloned(),
        prefixes:    &style.namespaces,
        record_path,
        scratch:     Vec::new(),
    };
    ev.run()?;
    Ok(ev.builder.finish())
}

// ── eligibility ─────────────────────────────────────────────────────────────

/// Is every template rule of `mode` within the instruction subset this
/// evaluator can stream?  When `false`, the caller must use burst.
pub fn push_eligible(style: &StylesheetAst, mode: Option<&QName>) -> bool {
    let mut any = false;
    for t in &style.templates {
        if t.match_pattern.is_none() || !template_in_mode(t, mode) {
            continue;
        }
        any = true;
        if !body_eligible(&t.body, mode) {
            return false;
        }
    }
    any
}

fn body_eligible(body: &Body, mode: Option<&QName>) -> bool {
    body.instrs().iter().all(|i| instr_eligible(i, mode))
}

fn instr_eligible(instr: &Instr, mode: Option<&QName>) -> bool {
    match instr {
        Instr::LiteralText { .. } => true,
        Instr::LiteralElement { attributes, body, use_attribute_sets, schema_type, .. } => {
            use_attribute_sets.is_empty()
                && schema_type.is_none()
                && attributes.iter().all(|(_, avt)| avt_motionless(avt))
                && body_eligible(body, mode)
        }
        // `separator=` is accepted but not applied: streamed value-of is
        // used on single-item selects (`.`, `@x`), where it is a no-op.
        Instr::ValueOf { select, .. } => {
            matches!(select_kind(select), SelectKind::Motionless | SelectKind::SelfNode)
        }
        Instr::CopyOf { select, .. } => is_self_select(select),
        Instr::ApplyTemplates { select, mode: am, with_params, sort, mode_current } => {
            with_params.is_empty()
                && sort.is_empty()
                && child_select(select.as_ref()).is_some()
                && apply_mode_in_mode(*mode_current, am.as_ref(), mode)
        }
        Instr::If { test, body } => {
            select_kind(test) == SelectKind::Motionless && body_eligible(body, mode)
        }
        Instr::Choose { whens, otherwise } => {
            whens.iter().all(|(t, b)| select_kind(t) == SelectKind::Motionless && body_eligible(b, mode))
                && otherwise.as_ref().is_none_or(|b| body_eligible(b, mode))
        }
        _ => false,
    }
}

/// `xsl:apply-templates` keeps recursion inside the streamable mode only
/// when it inherits the current mode or names the same one; otherwise the
/// switch is out of this evaluator's scope.
fn apply_mode_in_mode(mode_current: bool, am: Option<&QName>, mode: Option<&QName>) -> bool {
    if mode_current {
        return true;
    }
    match (am, mode) {
        (None, None) => true,
        (Some(a), Some(m)) => same_mode(a, m),
        _ => false,
    }
}

fn avt_motionless(avt: &Avt) -> bool {
    avt.parts.iter().all(|p| match p {
        AvtPart::Literal(_) => true,
        AvtPart::Expr(e) => select_kind(e) == SelectKind::Motionless,
    })
}

#[derive(PartialEq, Eq)]
enum SelectKind {
    Motionless,
    SelfNode,
    Unsupported,
}

/// Classify a `select`/test expression by how it touches the stream.
fn select_kind(expr: &Expr) -> SelectKind {
    if is_self_select(expr) {
        return SelectKind::SelfNode;
    }
    let ps = analyze_expr(expr, Posture::Striding);
    // Function calls are excluded: this evaluator runs motionless
    // expressions through the core XPath engine with bindings that don't
    // provide the XSLT function library (current(), key(),
    // accumulator-before(), document(), …).  Anything using a function
    // routes to burst, which has the full evaluator.
    if ps.posture != Posture::Roaming && ps.sweep == Sweep::Motionless && !has_function_call(expr) {
        SelectKind::Motionless
    } else {
        SelectKind::Unsupported
    }
}

/// Conservatively detect any function call in an expression.  Unknown /
/// exotic expression shapes report `true` so they route to burst rather
/// than being mis-evaluated by the function-less window bindings.
fn has_function_call(expr: &Expr) -> bool {
    use Expr::*;
    match expr {
        Literal(_) | Integer(_) | Decimal(_) | Double(_) | Variable(_) | ContextItem => false,
        Path(LocationPath::Absolute(steps)) | Path(LocationPath::Relative(steps)) => {
            steps.iter().any(|s| {
                s.filter.as_deref().is_some_and(has_function_call)
                    || s.predicates.iter().any(has_function_call)
            })
        }
        Or(a, b) | And(a, b)
        | Eq(a, b) | Ne(a, b) | Lt(a, b) | Gt(a, b) | Le(a, b) | Ge(a, b)
        | ValueEq(a, b) | ValueNe(a, b) | ValueLt(a, b) | ValueGt(a, b) | ValueLe(a, b) | ValueGe(a, b)
        | Add(a, b) | Sub(a, b) | Mul(a, b) | Div(a, b) | Mod(a, b)
        | Union(a, b) | Intersect(a, b) | Except(a, b) => has_function_call(a) || has_function_call(b),
        Neg(a) => has_function_call(a),
        WithDefaultCollation(_, inner) | BackwardsCompat(inner) => has_function_call(inner),
        _ => true,
    }
}

/// See through the synthetic wrappers the XSLT compiler adds around
/// top-level expressions (default-collation / 1.0-compat scopes).
fn unwrap_synthetic(expr: &Expr) -> &Expr {
    match expr {
        Expr::WithDefaultCollation(_, inner) | Expr::BackwardsCompat(inner) => unwrap_synthetic(inner),
        other => other,
    }
}

/// True for the context-item selections `.`, `self::node()`, and
/// `string(.)` — the consuming "current node / its string value" forms.
fn is_self_select(expr: &Expr) -> bool {
    match unwrap_synthetic(expr) {
        Expr::ContextItem => true,
        Expr::Path(LocationPath::Relative(steps)) => {
            steps.len() == 1
                && steps[0].axis == Axis::Self_
                && steps[0].predicates.is_empty()
                && steps[0].filter.is_none()
                && matches!(steps[0].node_test, NodeTest::AnyNode)
        }
        Expr::FunctionCall(name, args) => {
            let local = name.rsplit(':').next().unwrap_or(name);
            local == "string" && args.len() == 1 && is_self_select(&args[0])
        }
        _ => false,
    }
}

/// A supported downward `xsl:apply-templates` selection.
#[derive(Clone)]
enum ChildSelect {
    AllElements,
    Named(String),
    AllNodes,
}

/// Interpret a downward `select` (`*`, `name`, `node()`, or absent) as a
/// [`ChildSelect`].  `None` (an absent `select`) defaults to `node()`.
fn child_select(select: Option<&Expr>) -> Option<ChildSelect> {
    let Some(expr) = select else { return Some(ChildSelect::AllNodes) };
    let Expr::Path(LocationPath::Relative(steps)) = unwrap_synthetic(expr) else { return None };
    if steps.len() != 1 {
        return None;
    }
    let step = &steps[0];
    if step.axis != Axis::Child || !step.predicates.is_empty() || step.filter.is_some() {
        return None;
    }
    match &step.node_test {
        NodeTest::Wildcard      => Some(ChildSelect::AllElements),
        NodeTest::LocalName(n)  => Some(ChildSelect::Named(n.clone())),
        NodeTest::AnyNode       => Some(ChildSelect::AllNodes),
        _ => None,
    }
}

impl ChildSelect {
    fn matches_element(&self, name: &str) -> bool {
        match self {
            ChildSelect::AllElements | ChildSelect::AllNodes => true,
            ChildSelect::Named(n) => n == name,
        }
    }
    fn admits_text(&self) -> bool {
        matches!(self, ChildSelect::AllNodes)
    }
}

// ── mode helpers ────────────────────────────────────────────────────────────

fn same_mode(a: &QName, b: &QName) -> bool {
    a.uri == b.uri && a.local == b.local
}

fn is_default_mode_qname(q: &QName) -> bool {
    q.uri.is_empty() && q.local.is_empty()
}

fn template_in_mode(t: &Template, mode: Option<&QName>) -> bool {
    if t.modes_match_all {
        return true;
    }
    match mode {
        None => t.modes.is_empty() || t.modes.iter().any(is_default_mode_qname),
        Some(m) => t.modes.iter().any(|tm| same_mode(tm, m)),
    }
}

// ── window bindings (reuse the core XPath engine) ────────────────────────────

struct WindowBindings<'a> {
    prefixes: &'a HashMap<String, String>,
}

impl XPathBindings for WindowBindings<'_> {
    fn resolve_prefix(&self, prefix: &str) -> Option<String> {
        self.prefixes.get(prefix).cloned()
    }
}

// ── normalized events ───────────────────────────────────────────────────────

enum Ev {
    Start(String, Vec<(String, String)>),
    End,
    Text(String),
    CData(String),
    Comment(String),
    Pi(String, String),
    EntityRef(String),
    Eof,
}

fn bytes_to_string(b: &[u8]) -> Result<String, XsltError> {
    std::str::from_utf8(b)
        .map(str::to_owned)
        .map_err(|_| XsltError::InvalidStylesheet("streamed source is not valid UTF-8".into()))
}

fn pull_event<R: Read>(reader: &mut XmlByteStreamReader<R>) -> Result<Ev, XsltError> {
    Ok(match reader.next_event()? {
        BytesEvent::StartElement(tag) => {
            let name = bytes_to_string(tag.name())?;
            let mut attrs = Vec::new();
            for a in tag.attrs() {
                let a = a?;
                attrs.push((bytes_to_string(a.name)?, bytes_to_string(&a.value)?));
            }
            Ev::Start(name, attrs)
        }
        BytesEvent::EndElement(_) => Ev::End,
        BytesEvent::Text(t)       => Ev::Text(bytes_to_string(t.as_bytes())?),
        BytesEvent::CData(t)      => Ev::CData(bytes_to_string(t.as_bytes())?),
        BytesEvent::Comment(t)    => Ev::Comment(bytes_to_string(t.as_bytes())?),
        BytesEvent::Pi(p)         => Ev::Pi(bytes_to_string(p.target())?, bytes_to_string(p.content())?),
        BytesEvent::EntityRef(e)  => Ev::EntityRef(bytes_to_string(e.name())?),
        BytesEvent::Eof           => Ev::Eof,
    })
}

// ── the evaluator ───────────────────────────────────────────────────────────

struct Frame {
    name:  String,
    attrs: Vec<(String, String)>,
}

struct PushEval<'a, R: Read> {
    reader:      XmlByteStreamReader<R>,
    builder:     ResultBuilder,
    /// Ancestor-or-self spine of currently-open elements (the window).
    frames:      Vec<Frame>,
    style:       &'a StylesheetAst,
    mode:        Option<QName>,
    prefixes:    &'a HashMap<String, String>,
    record_path: &'a [String],
    /// Reused element stack for building `xsl:copy-of` subtrees, so a
    /// copy doesn't reallocate its work stack on every record.
    scratch:     Vec<ResultNode>,
}

impl<R: Read> PushEval<'_, R> {
    /// Top-level loop: descend through the stream and dispatch each
    /// element whose path matches `record_path` to its template.
    fn run(&mut self) -> Result<(), XsltError> {
        loop {
            match pull_event(&mut self.reader)? {
                Ev::Start(name, attrs) => {
                    self.frames.push(Frame { name, attrs });
                    if self.frames_match_record() {
                        self.process_node()?;
                    }
                }
                Ev::End => {
                    self.frames.pop();
                }
                Ev::Text(_) | Ev::CData(_) | Ev::Comment(_) | Ev::Pi(_, _) | Ev::EntityRef(_) => {}
                Ev::Eof => return Ok(()),
            }
        }
    }

    fn frames_match_record(&self) -> bool {
        self.frames.len() == self.record_path.len()
            && self.frames.iter().zip(self.record_path).all(|(f, p)| &f.name == p)
    }

    /// Apply the matching template (or the built-in) to the current
    /// element, consuming through its end tag, then pop its frame.
    fn process_node(&mut self) -> Result<(), XsltError> {
        let mut consumed = false;
        match self.match_template()? {
            Some(i) => {
                let style = self.style;
                let body = &style.templates[i].body;
                self.exec_body(body, &mut consumed)?;
            }
            // Built-in template: apply templates to children, copy text.
            None => {
                self.apply_to_children(&ChildSelect::AllNodes)?;
                consumed = true;
            }
        }
        if !consumed {
            self.skip_children_to_end()?;
        }
        self.frames.pop();
        Ok(())
    }

    /// Execute a sequence constructor.  `consumed` becomes `true` once a
    /// consuming instruction has read the current element's children.
    fn exec_body(&mut self, body: &Body, consumed: &mut bool) -> Result<(), XsltError> {
        for instr in body.instrs() {
            match instr {
                Instr::LiteralText { text, dose } => {
                    self.builder.push_text(text.clone(), *dose);
                }
                Instr::LiteralElement { name, attributes, namespaces, body, .. } => {
                    self.builder.open_element(name.clone());
                    for (prefix, uri) in namespaces {
                        self.builder.push_namespace_decl(prefix.clone(), uri.clone());
                    }
                    for (an, avt) in attributes {
                        let v = self.window_avt(avt)?;
                        self.builder.push_attribute(an.clone(), v);
                    }
                    self.exec_body(body, consumed)?;
                    self.builder.close_element();
                }
                Instr::ValueOf { select, dose, .. } => {
                    if is_self_select(select) {
                        self.emit_self_text(*dose)?;
                        *consumed = true;
                    } else {
                        let s = self.window_string(select)?;
                        self.builder.push_text(s, *dose);
                    }
                }
                Instr::CopyOf { .. } => {
                    self.copy_self_subtree()?;
                    *consumed = true;
                }
                Instr::ApplyTemplates { select, .. } => {
                    let sel = child_select(select.as_ref())
                        .expect("eligibility guarantees a supported select");
                    self.apply_to_children(&sel)?;
                    *consumed = true;
                }
                Instr::If { test, body } => {
                    if self.window_bool(test)? {
                        self.exec_body(body, consumed)?;
                    }
                }
                Instr::Choose { whens, otherwise } => {
                    let mut hit = false;
                    for (test, b) in whens {
                        if self.window_bool(test)? {
                            self.exec_body(b, consumed)?;
                            hit = true;
                            break;
                        }
                    }
                    if !hit {
                        if let Some(b) = otherwise {
                            self.exec_body(b, consumed)?;
                        }
                    }
                }
                other => {
                    return Err(XsltError::InvalidStylesheet(format!(
                        "push evaluator reached an unsupported instruction ({}); \
                         this is a bug — eligibility should have routed to burst",
                        instr_name(other)
                    )));
                }
            }
        }
        Ok(())
    }

    /// Process the children of the current element via `select`,
    /// consuming up to and including the current element's end tag.
    fn apply_to_children(&mut self, select: &ChildSelect) -> Result<(), XsltError> {
        loop {
            match pull_event(&mut self.reader)? {
                Ev::Start(name, attrs) => {
                    let selected = select.matches_element(&name);
                    self.frames.push(Frame { name, attrs });
                    if selected {
                        self.process_node()?;
                    } else {
                        self.skip_subtree()?;
                    }
                }
                Ev::End => return Ok(()),
                Ev::Text(t) | Ev::CData(t) => {
                    if select.admits_text() {
                        self.builder.push_text(t, false);
                    }
                }
                Ev::Comment(_) | Ev::Pi(_, _) | Ev::EntityRef(_) => {}
                Ev::Eof => return Err(unexpected_eof()),
            }
        }
    }

    /// Skip the current element's subtree (its start already consumed and
    /// its frame pushed): consume through its end tag, then pop the frame.
    fn skip_subtree(&mut self) -> Result<(), XsltError> {
        self.skip_children_to_end()?;
        self.frames.pop();
        Ok(())
    }

    /// Consume events up to and including the current element's matching
    /// end tag, discarding them.  Does not pop the frame.
    fn skip_children_to_end(&mut self) -> Result<(), XsltError> {
        let mut depth = 0u32;
        loop {
            match pull_event(&mut self.reader)? {
                Ev::Start(_, _) => depth += 1,
                Ev::End => {
                    if depth == 0 {
                        return Ok(());
                    }
                    depth -= 1;
                }
                Ev::Eof => return Err(unexpected_eof()),
                _ => {}
            }
        }
    }

    /// Accumulate the string-value (all descendant text) of the current
    /// element from events, up to and including its end tag, and emit it.
    fn emit_self_text(&mut self, dose: bool) -> Result<(), XsltError> {
        let mut depth = 0u32;
        let mut text = String::new();
        loop {
            match pull_event(&mut self.reader)? {
                Ev::Start(_, _) => depth += 1,
                Ev::End => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                Ev::Text(t) | Ev::CData(t) => text.push_str(&t),
                Ev::Comment(_) | Ev::Pi(_, _) | Ev::EntityRef(_) => {}
                Ev::Eof => return Err(unexpected_eof()),
            }
        }
        self.builder.push_text(text, dose);
        Ok(())
    }

    /// Deep-copy the current element's subtree from events into the
    /// result (consuming through its end tag).  Bounded by subtree size —
    /// inherent to `copy-of`; everything else stays O(depth).
    fn copy_self_subtree(&mut self) -> Result<(), XsltError> {
        let cur = self.frames.last().expect("current element");
        self.scratch.clear();
        self.scratch.push(make_element(&cur.name, &cur.attrs));
        loop {
            match pull_event(&mut self.reader)? {
                Ev::Start(name, attrs) => self.scratch.push(make_element(&name, &attrs)),
                Ev::End => {
                    let done = self.scratch.pop().expect("balanced");
                    match self.scratch.last_mut() {
                        Some(parent) => push_child(parent, done),
                        None => {
                            self.builder.push_built_node(done);
                            return Ok(());
                        }
                    }
                }
                Ev::Text(t) | Ev::CData(t) => {
                    push_child(self.scratch.last_mut().unwrap(), ResultNode::Text { content: t, dose: false });
                }
                Ev::Comment(c) => push_child(self.scratch.last_mut().unwrap(), ResultNode::Comment(c)),
                Ev::Pi(target, data) => {
                    push_child(self.scratch.last_mut().unwrap(), ResultNode::ProcessingInstruction { target, data });
                }
                Ev::EntityRef(n) => {
                    push_child(self.scratch.last_mut().unwrap(), ResultNode::Text { content: format!("&{n};"), dose: true });
                }
                Ev::Eof => return Err(unexpected_eof()),
            }
        }
    }

    // ── template selection + window evaluation ───────────────────────────

    fn match_template(&self) -> Result<Option<usize>, XsltError> {
        let mut best: Option<(usize, i32, f64)> = None;
        for (i, t) in self.style.templates.iter().enumerate() {
            let Some(pat) = &t.match_pattern else { continue };
            if !template_in_mode(t, self.mode.as_ref()) {
                continue;
            }
            if self.window_matches(pat)? {
                let prio = t.priority.unwrap_or(0.0);
                let take = match best {
                    None => true,
                    Some((_, bp, bpr)) => (t.import_precedence, prio) > (bp, bpr),
                };
                if take {
                    best = Some((i, t.import_precedence, prio));
                }
            }
        }
        Ok(best.map(|b| b.0))
    }

    fn window_matches(&self, pattern: &Expr) -> Result<bool, XsltError> {
        self.with_window(|idx, cur, b, _sc| {
            pattern::matches(pattern, cur, idx, b).map_err(XsltError::from)
        })
    }

    fn window_string(&self, expr: &Expr) -> Result<String, XsltError> {
        self.with_window(|idx, cur, b, sc| {
            let ctx = EvalCtx { context_node: cur, pos: 1, size: 1, bindings: b, static_ctx: sc };
            let v = eval_expr(expr, &ctx, idx).map_err(XsltError::from)?;
            Ok(value_to_string(&v, idx))
        })
    }

    fn window_bool(&self, expr: &Expr) -> Result<bool, XsltError> {
        self.with_window(|idx, cur, b, sc| {
            let ctx = EvalCtx { context_node: cur, pos: 1, size: 1, bindings: b, static_ctx: sc };
            let v = eval_expr(expr, &ctx, idx).map_err(XsltError::from)?;
            Ok(value_to_bool(&v, idx))
        })
    }

    fn window_avt(&self, avt: &Avt) -> Result<String, XsltError> {
        let mut out = String::new();
        for part in &avt.parts {
            match part {
                AvtPart::Literal(s) => out.push_str(s),
                AvtPart::Expr(e) => out.push_str(&self.window_string(e)?),
            }
        }
        Ok(out)
    }

    /// Materialize the current window (ancestor-or-self spine with
    /// attributes) as a transient [`DocIndex`] and run `f` with the
    /// current element as the context node.
    fn with_window<T>(
        &self,
        f: impl FnOnce(&DocIndex, NodeId, &WindowBindings, &StaticContext) -> Result<T, XsltError>,
    ) -> Result<T, XsltError> {
        let doc = build_spine(&self.frames);
        let idx = DocIndex::build(&doc);
        let current = leaf_node(&idx);
        let bindings = WindowBindings { prefixes: self.prefixes };
        let sc = StaticContext::default();
        f(&idx, current, &bindings, &sc)
    }
}

// ── spine construction ──────────────────────────────────────────────────────

/// Build a document consisting solely of the ancestor-or-self spine in
/// `frames`, each element carrying its attributes — the streaming window.
fn build_spine(frames: &[Frame]) -> sup_xml_tree::dom::Document {
    let b = DocumentBuilder::new();
    let mut parent: Option<&Node<'_>> = None;
    let mut root: Option<&Node<'_>> = None;
    for frame in frames {
        let name = b.alloc_str(&frame.name);
        let el: &Node<'_> = b.new_element(name);
        for (an, av) in &frame.attrs {
            // xmlns declarations are namespace nodes, not attributes; the
            // window is namespace-naive (push eligibility forbids prefixed
            // name tests), so they're left off without affecting matching.
            if an == "xmlns" || an.starts_with("xmlns:") {
                continue;
            }
            let attr = b.new_attribute(b.alloc_str(an), b.alloc_str(av));
            b.append_attribute(el, attr);
        }
        match parent {
            Some(p) => b.append_child(p, el),
            None => root = Some(el),
        }
        parent = Some(el);
    }
    if let Some(r) = root {
        b.set_root(r);
    }
    b.build()
}

/// The deepest element of a single-path spine document — the current node.
fn leaf_node(idx: &DocIndex) -> NodeId {
    let mut cur = 0;
    while let Some(child) = idx.children(cur).iter().copied().find(|&c| idx.is_element(c)) {
        cur = child;
    }
    cur
}

// ── result-node helpers ─────────────────────────────────────────────────────

fn make_element(name: &str, attrs: &[(String, String)]) -> ResultNode {
    let mut namespaces = Vec::new();
    let mut attributes = Vec::new();
    for (an, av) in attrs {
        if an == "xmlns" {
            namespaces.push((None, av.clone()));
        } else if let Some(p) = an.strip_prefix("xmlns:") {
            namespaces.push((Some(p.to_string()), av.clone()));
        } else {
            attributes.push((qname_of(an), av.clone()));
        }
    }
    ResultNode::Element {
        name: qname_of(name),
        namespaces,
        attributes,
        children: Vec::new(),
        schema_type: None,
        attr_types: Vec::new(),
    }
}

fn qname_of(raw: &str) -> QName {
    match raw.split_once(':') {
        Some((p, l)) => QName { prefix: Some(p.to_string()), local: l.to_string(), uri: String::new() },
        None => QName { prefix: None, local: raw.to_string(), uri: String::new() },
    }
}

fn push_child(parent: &mut ResultNode, child: ResultNode) {
    if let ResultNode::Element { children, .. } = parent {
        children.push(child);
    }
}

fn unexpected_eof() -> XsltError {
    XsltError::InvalidStylesheet("unexpected end of stream while transforming".into())
}

fn instr_name(i: &Instr) -> &'static str {
    match i {
        Instr::ForEach { .. } => "xsl:for-each",
        Instr::Copy { .. } => "xsl:copy",
        Instr::CallTemplate { .. } => "xsl:call-template",
        Instr::Variable(_) => "xsl:variable",
        _ => "an instruction",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stylesheet;

    fn run(xsl: &str, src: &str, path: &[&str], mode: Option<&str>) -> String {
        let style = Stylesheet::compile_str(xsl).unwrap();
        let mode_q = mode.map(|m| QName { prefix: None, local: m.to_string(), uri: String::new() });
        let path_owned: Vec<String> = path.iter().map(|s| s.to_string()).collect();
        let nodes = stream_apply_push(
            &style.ast,
            std::io::Cursor::new(src.as_bytes().to_vec()),
            &path_owned,
            mode_q.as_ref(),
        )
        .unwrap();
        let mut output = crate::ast::OutputSpec::default();
        output.omit_xml_declaration = Some(true);
        let rt = crate::result_tree::ResultTree {
            children: nodes,
            output,
            character_map: Vec::new(),
            secondary: Vec::new(),
        };
        rt.to_string().unwrap()
    }

    const PROJECT: &str = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:mode name="s" streamable="yes"/>
        <xsl:template match="book" mode="s">
            <entry isbn="{@isbn}"><xsl:apply-templates select="title" mode="#current"/></entry>
        </xsl:template>
        <xsl:template match="title" mode="s">
            <t><xsl:value-of select="."/></t>
        </xsl:template>
    </xsl:stylesheet>"##;

    #[test]
    fn transforming_projection_streams_without_grounding() {
        let src = "<lib><book isbn=\"9\"><title>A</title><pages>3</pages></book>\
                   <book isbn=\"8\"><title>B</title></book></lib>";
        let out = run(PROJECT, src, &["lib", "book"], Some("s"));
        assert_eq!(out, r#"<entry isbn="9"><t>A</t></entry><entry isbn="8"><t>B</t></entry>"#);
    }

    #[test]
    fn eligibility_accepts_projection_rejects_grouping() {
        let style = Stylesheet::compile_str(PROJECT).unwrap();
        let s = QName { prefix: None, local: "s".into(), uri: String::new() };
        assert!(push_eligible(&style.ast, Some(&s)));

        // A mode using xsl:for-each is streamable (the analyzer accepts
        // it) but outside this evaluator's instruction subset, so it is
        // not push-eligible → the caller falls back to burst.
        let fe = Stylesheet::compile_str(
            r#"<xsl:stylesheet version="3.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
                <xsl:mode name="s" streamable="yes"/>
                <xsl:template match="x" mode="s">
                    <xsl:for-each select="y"><g/></xsl:for-each>
                </xsl:template>
            </xsl:stylesheet>"#,
        )
        .unwrap();
        assert!(!push_eligible(&fe.ast, Some(&s)));
    }

    #[test]
    fn copy_of_streams_current_subtree() {
        let xsl = r#"<xsl:stylesheet version="3.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
            <xsl:output method="xml" omit-xml-declaration="yes"/>
            <xsl:mode name="s" streamable="yes"/>
            <xsl:template match="item" mode="s"><xsl:copy-of select="."/></xsl:template>
        </xsl:stylesheet>"#;
        let src = "<data><item id=\"1\"><v>a</v></item><item id=\"2\"/></data>";
        let out = run(xsl, src, &["data", "item"], Some("s"));
        assert_eq!(out, r#"<item id="1"><v>a</v></item><item id="2"/>"#);
    }

    #[test]
    fn motionless_predicate_and_if_filter() {
        let xsl = r#"<xsl:stylesheet version="3.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
            <xsl:output method="xml" omit-xml-declaration="yes"/>
            <xsl:mode name="s" streamable="yes"/>
            <xsl:template match="item" mode="s">
                <xsl:if test="@keep='yes'"><kept><xsl:value-of select="@id"/></kept></xsl:if>
            </xsl:template>
        </xsl:stylesheet>"#;
        let src = "<d><item id=\"1\" keep=\"yes\"/><item id=\"2\" keep=\"no\"/><item id=\"3\" keep=\"yes\"/></d>";
        let out = run(xsl, src, &["d", "item"], Some("s"));
        assert_eq!(out, "<kept>1</kept><kept>3</kept>");
    }

    #[test]
    fn deep_recursion_stays_in_mode() {
        // Nested apply-templates recursion through several levels.
        let xsl = r##"<xsl:stylesheet version="3.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
            <xsl:output method="xml" omit-xml-declaration="yes"/>
            <xsl:mode name="s" streamable="yes"/>
            <xsl:template match="section" mode="s">
                <sec><xsl:apply-templates select="*" mode="#current"/></sec>
            </xsl:template>
            <xsl:template match="p" mode="s"><para><xsl:value-of select="."/></para></xsl:template>
        </xsl:stylesheet>"##;
        let src = "<doc><section><p>a</p><section><p>b</p></section></section></doc>";
        let out = run(xsl, src, &["doc", "section"], Some("s"));
        assert_eq!(out, "<sec><para>a</para><sec><para>b</para></sec></sec>");
    }
}
