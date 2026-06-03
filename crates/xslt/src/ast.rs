//! Compiled XSLT AST — the output of [`crate::compiler`] and the
//! input to the [`crate::eval`] instruction evaluator.
//!
//! XPath expressions in the stylesheet (`select="…"`, `match="…"`,
//! `test="…"`, `use="…"`) are pre-parsed at compile time to
//! [`sup_xml_core::xpath::Expr`], saving the re-parse on every
//! transformation.  Attribute Value Templates (`attr="x{expr}y"`)
//! decompose into a [`Avt`] of literal + expression parts.

use sup_xml_core::xpath::Expr;

/// A namespace-qualified name, captured at compile time so we don't
/// re-resolve prefix→URI per use.  XSLT spec speaks of "expanded
/// names" — `local` + `uri`; we keep the lexical `prefix` too so
/// emitted result-tree elements can preserve the stylesheet's
/// chosen prefixes.
#[derive(Clone, Debug)]
pub struct QName {
    pub prefix: Option<String>,
    pub local:  String,
    pub uri:    String,
}

impl QName {
    /// Build a `prefix:local` lexical form, or just `local` if no prefix.
    pub fn to_qname_string(&self) -> String {
        match &self.prefix {
            Some(p) => format!("{p}:{}", self.local),
            None    => self.local.clone(),
        }
    }
}

// ── attribute value templates ─────────────────────────────────────

/// One part of an Attribute Value Template (AVT).  Mixed literal +
/// expression sequence.
#[derive(Clone, Debug)]
pub enum AvtPart {
    Literal(String),
    Expr(Expr),
}

/// A complete AVT — concatenate part by part at evaluation time.
#[derive(Clone, Debug, Default)]
pub struct Avt {
    pub parts: Vec<AvtPart>,
}

impl Avt {
    /// `true` when the AVT has no `{expr}` substitutions.  Lets
    /// the evaluator short-circuit and emit the literal string
    /// directly without an XPath round-trip.
    pub fn is_literal(&self) -> bool {
        self.parts.iter().all(|p| matches!(p, AvtPart::Literal(_)))
    }

    /// Build a single-literal AVT — convenient when the compiler
    /// needs a constant default (e.g. XSLT 2.0 `xsl:value-of`'s
    /// implicit `separator=" "`).
    pub fn literal(s: &str) -> Self {
        Avt { parts: vec![AvtPart::Literal(s.to_string())] }
    }

    /// Concatenate the AVT's literal parts when the whole AVT has
    /// no `{expr}` substitutions.  Returns `None` when the AVT
    /// contains any expression part — callers that need a runtime
    /// value should drive the regular AVT renderer.
    pub fn as_literal(&self) -> Option<String> {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                AvtPart::Literal(s) => out.push_str(s),
                _ => return None,
            }
        }
        Some(out)
    }
}

// ── templates ────────────────────────────────────────────────────

/// A compiled `xsl:template`.  Templates with `match=` participate
/// in pattern-matching; templates with `name=` are call-targets.
/// The XSLT spec allows both on one template (rare but legal).
#[derive(Clone, Debug)]
pub struct Template {
    /// `match=`'s compiled XPath, if present.  The XSLT pattern
    /// grammar is a subset of XPath but we compile the whole
    /// expression — [`crate::pattern`] decides match-ness by
    /// evaluating the pattern as an XPath against the candidate
    /// node's ancestor-or-self chain.
    pub match_pattern: Option<Expr>,
    /// `name=`'s expanded name, if present.
    pub name:          Option<QName>,
    /// `mode=`'s expanded name, if present.  `None` means the
    /// default (no-mode) match set.  This is the *primary* mode
    /// used by callers that don't care about XSLT 2.0 multi-mode
    /// expansion (XSLT 1.0 only ever had one).
    pub mode:          Option<QName>,
    /// XSLT 2.0 §6 multi-mode list.  The same template participates
    /// in every mode in this list.  An empty vec means "default mode
    /// only" (legacy XSLT 1.0 behaviour); `modes_match_all == true`
    /// means the template matches every mode (`mode="#all"`).
    /// `#default` resolves to the empty-QName entry, kept here so
    /// the matcher can compare without special-casing.
    pub modes:           Vec<QName>,
    /// True iff `mode=` contained the `#all` token.
    pub modes_match_all: bool,
    /// Explicit `priority=` from the source, if any.  When `None`
    /// the matcher computes a default priority per XSLT 1.0 §5.5.
    pub priority:      Option<f64>,
    /// XSLT 1.0 §2.6.2 import precedence.  Outermost stylesheet =
    /// 0; each imported stylesheet's templates get a more-negative
    /// precedence (or one stamped during recursive import
    /// resolution).  The pattern matcher uses this as the
    /// highest-priority tiebreaker.
    pub import_precedence: i32,
    /// Position in the *effective* source order across the whole
    /// stylesheet, with `xsl:include` flattened in at the include
    /// directive's position.  Compared lexicographically so that an
    /// included template (path `[2, 0]`) correctly sorts between the
    /// surrounding main-file templates at `[1]` and `[3]`.  XSLT 1.0
    /// §5.5 uses this as the last conflict-resolution tiebreaker.
    pub source_path: Vec<u32>,
    /// Local declarations parsed from the template's body header:
    /// `xsl:param` elements that have to precede instructions.
    pub params:        Vec<Param>,
    /// Body instructions, in document order.
    pub body:          Vec<Instr>,
    /// XSLT 2.0 `as="xs:T"` on xsl:template — the declared type of
    /// the value produced by the body.  XTTE0505 fires when the
    /// produced value doesn't match.  `None` means no constraint.
    pub as_type:       Option<String>,
}

/// `xsl:param` — typed enough to carry name + default value.  Top-
/// level params share the same struct.
#[derive(Clone, Debug)]
pub struct Param {
    pub name:   QName,
    /// `select=` form: pre-parsed XPath.  Mutually exclusive with
    /// `body` per the XSLT spec; we encode "either / or" by
    /// storing the active form and leaving the other empty.
    pub select: Option<Expr>,
    pub body:   Vec<Instr>,
    /// XSLT 2.0 `tunnel="yes"` — read from the tunnel-param pool
    /// rather than the caller's regular `xsl:with-param` args.
    pub tunnel: bool,
    /// XSLT 2.0 `as="xs:T"` — when set, the bound value is cast to
    /// the requested type at bind time so `instance of` / typed
    /// arithmetic see the declared type rather than whatever the
    /// `select=` expression produced.  Stored as the raw lexical
    /// (e.g. `"xs:integer"` / `"xs:string*"`) so the parser at
    /// bind time can extract the local name + occurrence indicator.
    pub as_type: Option<String>,
    /// XSLT 2.0 `required="yes"` — the caller MUST supply this
    /// parameter; absence at apply time is the XTSE0010 / XTDE0700
    /// error.  Required params can't have a default (`select=` or
    /// body); the compiler doesn't enforce this — the runtime
    /// surfaces the missing-arg error.
    pub required: bool,
}

/// `xsl:variable` — same shape as `Param`.  Local variables live
/// inside `Body`; global variables live on the [`Stylesheet`].
#[derive(Clone, Debug)]
pub struct Variable {
    pub name:   QName,
    pub select: Option<Expr>,
    pub body:   Vec<Instr>,
    /// XSLT 2.0 `as="xs:T"` — see [`Param::as_type`].
    pub as_type: Option<String>,
    /// Effective xml:base for this variable's body-form RTF.
    /// XPath 2.0 §3.1.5 — the temporary tree the body builds has
    /// its document node's base-uri set to this value; reachable
    /// at runtime through fn:base-uri($var).
    pub base_uri: Option<String>,
    /// XSLT 3.0 `visibility=` (§3.5.2).  `None` = the package default
    /// (private).  A using package can only reference this global if
    /// it is `public`/`final`/`abstract`.
    pub visibility: Option<String>,
}

// ── instructions ────────────────────────────────────────────────

/// One XSLT instruction or piece of result-tree-emitting content.
/// Covers every XSLT 1.0 instruction.  The structure is captured
/// at compile time; [`crate::eval`] consumes this AST.
#[derive(Clone, Debug)]
pub enum Instr {
    // ── result tree emission ────────────────────────────────────
    /// Non-XSLT element appearing in a template body.  Its
    /// attributes are AVT-compiled; its body recurses into more
    /// instructions / literals.
    LiteralElement {
        name:       QName,
        /// Attribute name (qname) + AVT value.
        attributes: Vec<(QName, Avt)>,
        /// In-scope namespaces inherited from the stylesheet source,
        /// minus the XSLT and extension-element prefixes and anything
        /// the author listed in `[xsl:]exclude-result-prefixes`.  XSLT
        /// 1.0 §7.1.1 requires these to appear as namespace nodes on
        /// the result element so xmlns declarations propagate.
        ///
        /// `prefix=None` means the default xmlns; `uri=""` means the
        /// explicit `xmlns=""` undeclare.
        namespaces: Vec<(Option<String>, String)>,
        /// `xsl:use-attribute-sets` on the LRE — the named attribute
        /// sets to apply before any LRE-declared attributes.
        use_attribute_sets: Vec<QName>,
        body:       Vec<Instr>,
    },
    /// Literal text node — verbatim character content with no
    /// XSLT interpretation.  Carries the `disable-output-escaping`
    /// flag from any wrapping `xsl:text`.
    LiteralText { text: String, dose: bool },

    // ── XSLT instructions ──────────────────────────────────────
    ApplyTemplates {
        select:      Option<Expr>,
        mode:        Option<QName>,
        sort:        Vec<Sort>,
        with_params: Vec<WithParam>,
        /// XSLT 2.0 `mode="#current"` — at apply time the call
        /// inherits the calling template's mode rather than using
        /// either `mode` (named) or the unnamed default.  When this
        /// is true, the compiled `mode` field is ignored.
        mode_current: bool,
    },
    /// XSLT 2.0 §9.4 extended `xsl:apply-imports` to accept
    /// `xsl:with-param` children whose values are passed to the
    /// imported template.  XSLT 1.0's form is an empty content
    /// model — older stylesheets carry an empty Vec here.
    ApplyImports { with_params: Vec<WithParam> },
    /// XSLT 2.0 §6.7 `xsl:next-match` — re-runs template selection
    /// against the same `(node, mode)` but limited to templates with
    /// strictly lower (import-precedence, priority, source-position)
    /// than the currently-running one.  Our implementation reuses the
    /// `apply-imports` plumbing with the next-lower-precedence
    /// shortcut — handles the common case (different precedence
    /// levels) cleanly and degrades to the same template as
    /// `apply-imports` when the spec would actually re-select the
    /// same level at a lower priority (rare in practice).
    NextMatch {
        with_params: Vec<WithParam>,
    },
    CallTemplate {
        name:        QName,
        with_params: Vec<WithParam>,
    },
    Choose {
        whens:     Vec<(Expr, Vec<Instr>)>,
        otherwise: Option<Vec<Instr>>,
    },
    If {
        test: Expr,
        body: Vec<Instr>,
    },
    ForEach {
        select: Expr,
        sort:   Vec<Sort>,
        body:   Vec<Instr>,
    },
    /// XSLT 3.0 §8.3 `xsl:iterate` — sequential iteration with
    /// loop-carried parameters.  `params` are the `xsl:param`
    /// declarations (initial values); the body may end an iteration
    /// with [`Instr::NextIteration`] (supplying the next parameter
    /// values) or [`Instr::Break`] (early exit).  `on_completion`
    /// runs once after the last item if no `xsl:break` fired.
    Iterate {
        select:        Expr,
        params:        Vec<Param>,
        on_completion: Vec<Instr>,
        body:          Vec<Instr>,
    },
    /// XSLT 3.0 §8.3 `xsl:next-iteration` — ends the current iteration
    /// of the enclosing `xsl:iterate`, supplying parameter values for
    /// the next one (unmentioned params keep their current value).
    NextIteration {
        with_params: Vec<WithParam>,
    },
    /// XSLT 3.0 §8.3 `xsl:break` — terminates the enclosing
    /// `xsl:iterate`; the optional `select`/body is the break's output.
    Break {
        select: Option<Expr>,
        body:   Vec<Instr>,
    },
    ValueOf {
        select: Expr,
        dose:   bool,
        /// XSLT 2.0 `separator=` AVT.  When set, the evaluator
        /// atomises the select result into a sequence and joins each
        /// item with the rendered separator string.  In XSLT 1.0
        /// mode this stays `None` (1.0 always takes the first node's
        /// string-value).  XSLT 2.0 with no separator defaults to a
        /// single space — represented here by `Some(<space AVT>)`
        /// so the runtime path stays uniform.
        separator: Option<Avt>,
    },
    /// XSLT 2.0 §11.6 — body-form `<xsl:value-of>` without a
    /// `select=` attribute.  The body is a sequence-constructor;
    /// the evaluator runs it into a fresh result tree, takes the
    /// string-value, and emits it as a text node (applying
    /// `disable-output-escaping` and `separator=` the same way the
    /// select-form does).
    ValueOfBody {
        body:      Vec<Instr>,
        dose:      bool,
        separator: Option<Avt>,
    },
    Copy {
        use_attribute_sets: Vec<QName>,
        body:               Vec<Instr>,
        /// XSLT 2.0 §11.9.1 `copy-namespaces` — `false` (`="no"`) keeps
        /// only the namespaces the copied element's own name needs.
        /// Default `true`.
        copy_namespaces:    bool,
    },
    CopyOf {
        select: Expr,
        /// XSLT 2.0 §11.9.1 `copy-namespaces` — `false` (`="no"`) copies
        /// only the namespaces required by copied nodes' own names,
        /// dropping inherited in-scope declarations.  Default `true`.
        copy_namespaces: bool,
    },
    Element {
        name:               Avt,                 // AVT — name may be dynamic
        namespace:          Option<Avt>,
        use_attribute_sets: Vec<QName>,
        body:               Vec<Instr>,
        /// In-scope namespaces at the `xsl:element` source location,
        /// captured at compile time.  Used to expand the runtime
        /// `name` AVT into a QName under the stylesheet author's
        /// local namespace context (XSLT 1.0 §7.1.2 — "the namespace
        /// declarations in effect for the xsl:element element").
        in_scope_namespaces: Vec<(Option<String>, String)>,
    },
    Attribute {
        name:      Avt,
        namespace: Option<Avt>,
        /// XSLT 2.0 `select=` shortcut — when present, the attribute
        /// value is the string-value of this XPath expression and
        /// `body` is empty.  When absent the value comes from the
        /// instruction body (XSLT 1.0 form).
        select:    Option<Expr>,
        /// XSLT 2.0 §11.3 `separator=` AVT — inserted between
        /// adjacent items of a sequence-valued `select=` /
        /// constructed body.  `None` falls back to the default
        /// (`" "` for `select=`, empty for body-form).
        separator: Option<Avt>,
        body:      Vec<Instr>,
        /// In-scope namespaces at the `xsl:attribute` source
        /// location, for the same reason `xsl:element` carries them.
        in_scope_namespaces: Vec<(Option<String>, String)>,
    },
    Comment {
        /// XSLT 2.0 §11.8 `select=` shortcut — comment text is the
        /// string-value of the expression.  When absent, the value
        /// comes from the body.
        select: Option<Expr>,
        body:   Vec<Instr>,
    },
    ProcessingInstruction {
        name:   Avt,
        /// XSLT 2.0 §11.5 `select=` shortcut for the PI data.
        select: Option<Expr>,
        body:   Vec<Instr>,
    },
    Number {
        // XSLT §7.7.  When `value=` is set we just format that
        // integer; otherwise we walk the source tree per `level`
        // and the optional `count` / `from` XPath patterns.
        value:  Option<Expr>,
        /// XSLT 2.0 §12.4 — `select=` replaces the context node
        /// used by the level/count/from walk; defaults to the
        /// surrounding template's context node when absent.
        select: Option<Expr>,
        level:  NumberLevel,
        count:  Option<Expr>,
        from:   Option<Expr>,
        format: Avt,
        /// `grouping-separator` AVT — defaults to none if unset.
        /// When both `grouping-size` and this are present, the
        /// formatter inserts the separator every `grouping-size`
        /// digits from the right.
        grouping_separator: Option<Avt>,
        /// `grouping-size` AVT — defaults to none if unset.
        grouping_size:      Option<Avt>,
        /// `ordinal` AVT — XSLT 2.0 §12.5.  A non-empty value selects
        /// ordinal numbering ("first", "second", …) for word-form
        /// formats; otherwise cardinal ("one", "two", …).
        ordinal:            Option<Avt>,
        /// `lang` AVT — XSLT 2.0 §12.5 selects the natural-language
        /// numbering used by the word-form formats (`W` / `w` / `Ww`).
        /// Only `en` is implemented; other languages fall back to it.
        lang:               Option<Avt>,
        /// `letter-value` AVT — `traditional` vs `alphabetic`
        /// (XSLT 2.0 §12.5).  Currently unused (no language requires
        /// distinct sequences) but parsed so AVT-driven tests don't
        /// reject the attribute.
        letter_value:       Option<Avt>,
        /// `start-at` AVT — XSLT 3.0 §12.3.  A whitespace-separated
        /// list of integers giving the first number at each level;
        /// level `i` is offset by `start_at[i] - 1` (levels past the
        /// list, and the default, use 1 → no offset).
        start_at:           Option<Avt>,
    },
    Variable(Variable),
    Message {
        /// XSLT 2.0 §17.1 — `terminate=` is an AVT that evaluates to
        /// `"yes"` / `"no"` at runtime.  `None` means absent, equivalent
        /// to `"no"`.
        terminate: Option<Avt>,
        body:      Vec<Instr>,
    },
    /// `xsl:fallback` — only fires when the surrounding instruction
    /// is unrecognised.  In XSLT 1.0 this is rare; we capture it so
    /// forward-compat documents that target XSLT 2.0+ still parse.
    Fallback { body: Vec<Instr> },

    /// `xsl:sequence` (XSLT 2.0) — evaluates `select` and contributes
    /// its value to the current sequence constructor.  Inside an
    /// `xsl:function` body, the last `xsl:sequence` value is what the
    /// function returns.  In other contexts it behaves like a
    /// dynamic `xsl:value-of` / `xsl:copy-of` blend (atomic values
    /// become text, node-set values are deep-copied).
    Sequence { select: Expr },
    /// `xsl:for-each-group` (XSLT 2.0 §14) — partitions the `select`
    /// node-set into groups by one of the four grouping criteria
    /// (`group-by`, `group-adjacent`, `group-starting-with`,
    /// `group-ending-with`) and runs `body` once per group with the
    /// per-group context exposed via `current-group()` /
    /// `current-grouping-key()`.
    ForEachGroup {
        select:   Expr,
        kind:     GroupingKind,
        key:      Expr,
        sort:     Vec<Sort>,
        body:     Vec<Instr>,
        /// Optional `collation=` URI (defaults to the codepoint
        /// collation if absent or empty).  Only `group-by` /
        /// `group-adjacent` consult this — the positional grouping
        /// forms compare nodes by identity.
        collation: Option<String>,
    },
    /// `xsl:source-document` (XSLT 3.0 §18.1; also the older
    /// `xsl:stream`) — process an external document.  We implement it
    /// non-streamed: the referenced document is loaded into a tree and
    /// the body is evaluated with the document node as context.  The
    /// `streamable` attribute is accepted and ignored.
    SourceDocument {
        href: Avt,
        body: Vec<Instr>,
    },
    /// `xsl:on-empty` (XSLT 3.0 §16.4.1) — its content is emitted only
    /// if the rest of the containing sequence constructor produces no
    /// significant output.
    OnEmpty {
        body: Vec<Instr>,
    },
    /// `xsl:on-non-empty` (XSLT 3.0 §16.4.2) — its content is emitted
    /// only if the rest of the containing sequence constructor produces
    /// significant output.  Both appear at their own position in the
    /// result.
    OnNonEmpty {
        body: Vec<Instr>,
    },
    /// `xsl:where-populated` (XSLT 3.0 §16.4.3) — evaluate the body but
    /// emit it only if the result is "populated" (contains at least one
    /// node that isn't an empty element/document or a zero-length text
    /// node).  Suppresses empty wrapper elements.
    WherePopulated {
        body: Vec<Instr>,
    },
    /// `xsl:fork` (XSLT 3.0 §19) — in streamed processing the prongs
    /// share one pass over the input; for a tree-based engine it is
    /// equivalent to evaluating the prongs (its `xsl:sequence` /
    /// `xsl:for-each-group` children) in order and concatenating the
    /// results, so we model it as a plain body.
    Fork {
        body: Vec<Instr>,
    },
    /// `xsl:evaluate` (XSLT 3.0 §10.4) — evaluate a dynamically
    /// constructed XPath expression.  `xpath` yields the expression
    /// string; `context_item` supplies its context item (default: the
    /// current node); `with_params` bind variables visible to it.
    Evaluate {
        xpath:        Expr,
        context_item: Option<Expr>,
        with_params:  Vec<WithParam>,
    },
    /// `xsl:merge` (XSLT 3.0 §15) — merges several pre-sorted input
    /// sequences into one stream ordered by the merge keys, invoking
    /// the action once per distinct merge-key value with
    /// `current-merge-group()` / `current-merge-key()` in scope.
    Merge {
        sources: Vec<MergeSource>,
        action:  Vec<Instr>,
    },
    /// `xsl:analyze-string` (XSLT 2.0 §15.1) — partitions the
    /// `select` string into matching / non-matching substrings against
    /// the `regex` AVT (with optional `flags` AVT) and runs the
    /// corresponding body for each piece.  Inside `matching`,
    /// `regex-group(n)` exposes the nth capture group.
    AnalyzeString {
        select:        Expr,
        regex:         Avt,
        flags:         Avt,
        matching:      Vec<Instr>,
        non_matching:  Vec<Instr>,
    },
    /// `xsl:perform-sort` (XSLT 2.0 §13.3) — sorts a sequence and
    /// emits the sorted items.  Behaves like `xsl:for-each` with
    /// sort but yields items directly (no per-item body).
    ///
    /// The sequence comes from `select=` when present; otherwise from
    /// evaluating `body` as a sequence constructor.  `body` is empty
    /// in the `select=` form.  `xsl:sort` element children are
    /// directives — they're collected into `sort` and stripped from
    /// `body`.
    PerformSort {
        select: Option<Expr>,
        sort:   Vec<Sort>,
        body:   Vec<Instr>,
    },
    /// `xsl:document` (XSLT 2.0 §14.4) — wraps the body's sequence
    /// constructor in a document node (an RTF in our value model).
    /// The optional `type=`/`validation=` attributes are ignored.
    Document {
        body: Vec<Instr>,
    },
    /// `xsl:result-document` (XSLT 2.0 §19.1) — evaluates its body into
    /// a result tree written to the resolved `href`, separate from the
    /// principal output.  An empty/absent `href` targets the principal
    /// output URI instead, which is valid only when it is the sole
    /// writer of that destination.
    ResultDocument {
        href: Avt,
        /// `format=` AVT — XSLT 2.0 §19.1 names the output definition
        /// to use for serialisation.  Lexically a QName / EQName that
        /// must (after AVT expansion) match an `xsl:output` with a
        /// matching `name=`.  XTDE1460 fires at runtime when the
        /// expansion either has a prefix not in scope on the
        /// xsl:result-document element or names no declared output.
        format: Option<Avt>,
        /// In-scope namespace bindings on the `xsl:result-document`
        /// element, captured at compile time so `format=` AVT
        /// expansions can be QName-validated at runtime without
        /// re-walking the source.  `None` prefix is the default
        /// namespace.
        format_namespaces: Vec<(Option<String>, String)>,
        body: Vec<Instr>,
    },
    /// `xsl:namespace name="x" [select="uri"]` (XSLT 2.0 §11.7) —
    /// emits a namespace node on the surrounding result-tree element.
    Namespace {
        name:   Avt,
        select: Option<Expr>,
        body:   Vec<Instr>,
    },

    /// XSLT 3.0 §15 `xsl:try` / `xsl:catch` — evaluate the body;
    /// if it raises a dynamic error, dispatch to the first catch
    /// whose `errors=` test matches the error's QName.  Bare
    /// `errors="*"` (or no `errors=` attribute) catches anything.
    /// Inside the catch body, the implicit `$err:code` /
    /// `$err:description` / `$err:value` / `$err:module` /
    /// `$err:line-number` / `$err:column-number` variables
    /// describe the caught error.
    Try {
        body:    Vec<Instr>,
        catches: Vec<TryCatch>,
    },

    /// Compile-time placeholder for instructions whose structure
    /// we haven't fully wired yet.  The evaluator first runs the
    /// captured `fallback` children (XSLT 1.0 §15 forwards-compat
    /// mode) and, if there are none, raises an error referencing
    /// `name`.  Keeps compilation total — a stylesheet with one
    /// unsupported instruction inside an `xsl:if` still compiles
    /// cleanly; the failure surfaces only if execution actually
    /// reaches it.
    Unsupported {
        name:     String,
        fallback: Vec<Instr>,
    },
}

/// One handler clause of an [`Instr::Try`] / `<xsl:catch>`.
#[derive(Clone, Debug)]
pub struct TryCatch {
    /// QName matchers from the `errors=` attribute.  Each entry
    /// is either an exact `{uri}local`, a `{uri}*` namespace
    /// wildcard, `*` (catch-all), or `*:local` (any namespace,
    /// matching local name).  An empty list also means "catch
    /// anything" — XSLT 3.0 lets `errors=` be omitted.
    pub errors: Vec<CatchMatcher>,
    /// Sequence-constructor body of the catch.  When the
    /// handler matches, its body runs with the `err:*` variables
    /// in scope.
    pub body:   Vec<Instr>,
}

/// One name-test inside an `xsl:catch` `errors=` list.
#[derive(Clone, Debug)]
pub enum CatchMatcher {
    /// `*` — any error matches.
    Any,
    /// `*:NCName` — matches errors with this local name in any namespace.
    LocalNameOnly(String),
    /// `prefix:*` — namespace wildcard, prefix resolved to URI.
    PrefixWildcard(String),
    /// `prefix:local` / `local` — fully-qualified error code.
    QName(QName),
}

/// Distinguishes the four `xsl:for-each-group` grouping criteria.
/// We currently implement `group-by` (the workhorse); the other
/// three parse but error at apply time so stylesheets that compile
/// today still flag the unimplemented branches when reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupingKind {
    By,
    Adjacent,
    StartingWith,
    EndingWith,
}

/// `level=` attribute on `xsl:number` (XSLT 1.0 §7.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NumberLevel {
    /// Default.  Count preceding siblings matching `count` + self.
    #[default]
    Single,
    /// Count preceding (in document order) nodes matching `count`,
    /// from the `from` ancestor (or root) onwards.  One integer.
    Any,
    /// Count for *each* ancestor-or-self that matches `count` and
    /// lies within the `from` ancestor's subtree.  List of integers.
    Multiple,
}

/// `xsl:sort` — used by `xsl:apply-templates` and `xsl:for-each`.
/// Sorting itself lives in [`crate::sort`]; this struct only
/// captures the compiled per-key knobs.
#[derive(Clone, Debug)]
pub struct Sort {
    pub select:     Option<Expr>,    // defaults to the context node's string-value
    pub lang:       Option<Avt>,
    pub data_type:  Option<Avt>,     // "text" | "number" | qname-ext
    pub order:      Option<Avt>,     // "ascending" | "descending"
    pub case_order: Option<Avt>,     // "upper-first" | "lower-first"
    /// XSLT 2.0 §13.1.3 — `collation=` URI.  Only the codepoint URI
    /// is recognised today; absent or any other value falls back to
    /// the Unicode-aware default text_compare (which is itself
    /// codepoint-based but case-folds before comparison).
    pub collation:  Option<Avt>,
}

/// `<xsl:use-package>` declaration (XSLT 3.0 §3.5.1).
#[derive(Clone, Debug)]
pub struct UsePackage {
    /// The `name=` URI of the package to use.
    pub name:      String,
    /// Optional `package-version=` constraint (currently informational).
    pub version:   Option<String>,
    /// Declarations inside `<xsl:override>` children, compiled as
    /// top-level declarations of the using package so they take
    /// precedence over the used package's originals.
    pub overrides: Box<StylesheetAst>,
}

/// `<xsl:accumulator>` declaration (XSLT 3.0 §18).  Computes a running
/// value over a document-order traversal; `accumulator-before(name)` /
/// `accumulator-after(name)` read the value at a node.
#[derive(Clone, Debug)]
pub struct AccumulatorDecl {
    pub name:          QName,
    pub initial_value: Expr,
    pub rules:         Vec<AccumulatorRule>,
}

/// One `<xsl:accumulator-rule>` (XSLT 3.0 §18.2).
#[derive(Clone, Debug)]
pub struct AccumulatorRule {
    pub match_pattern: Expr,
    pub phase:         AccumulatorPhase,
    /// `select=` for the new value; `None` when the new value comes
    /// from the rule's sequence-constructor body.
    pub select:        Option<Expr>,
    pub body:          Vec<Instr>,
}

/// Whether an accumulator rule fires on the node's pre-order (`start`)
/// or post-order (`end`) event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccumulatorPhase { Start, End }

/// One `<xsl:merge-source>` of an [`Instr::Merge`] (XSLT 3.0 §15).
#[derive(Clone, Debug)]
pub struct MergeSource {
    /// `name=` — referenced by `current-merge-group('name')`.
    pub name:            Option<String>,
    /// `select=` — the sequence contributed by this source (evaluated
    /// once, or once per `for-each-source` item).
    pub select:          Expr,
    /// Optional `for-each-source=` — evaluate `select` with each of
    /// these items as the context, concatenating the results.
    pub for_each_source: Option<Expr>,
    /// `<xsl:merge-key>` children — share `xsl:sort`'s comparison
    /// knobs and must be positionally consistent across sources.
    pub keys:            Vec<Sort>,
}

/// `xsl:with-param` — argument passed to apply-templates /
/// call-template.
#[derive(Clone, Debug)]
pub struct WithParam {
    pub name:   QName,
    pub select: Option<Expr>,
    pub body:   Vec<Instr>,
    /// XSLT 2.0 `tunnel="yes"` — the value enters the tunnel-param
    /// pool that propagates through every downstream apply / call
    /// until a tunnel-typed `xsl:param` consumes it.
    pub tunnel: bool,
    /// XSLT 2.0 `as="xs:T"` — declared type of the supplied value.
    /// When the body-form contributes a sequence-typed value, the
    /// declared type drives the §9.3 non-document-wrap binding.
    pub as_type: Option<String>,
}

// ── top-level stylesheet ────────────────────────────────────────

/// `xsl:output` settings.  XSLT 1.0 allows multiple `xsl:output`
/// elements; the *effective* output is the merge of all of them
/// with later-precedence wins.  Raw entries land here and the
/// final merge happens in [`crate::eval::apply_stylesheet`] before
/// handing to the output serialiser.
#[derive(Clone, Debug, Default)]
pub struct OutputSpec {
    pub method:                 Option<String>,
    pub encoding:               Option<String>,
    pub indent:                 Option<bool>,
    pub omit_xml_declaration:   Option<bool>,
    pub standalone:             Option<bool>,
    pub cdata_section_elements: Vec<QName>,
    pub media_type:             Option<String>,
    pub doctype_public:         Option<String>,
    pub doctype_system:         Option<String>,
    pub version:                Option<String>,
    /// XSLT 2.0 §20: `use-character-maps="qname …"`.  Each name
    /// resolves to an `xsl:character-map` declaration in the
    /// stylesheet; their `output-character` substitutions are
    /// applied to character data during serialization.
    pub use_character_maps:     Vec<QName>,
}

/// `xsl:key name= match= use=` — indexes nodes by computed key
/// values for fast lookup via the `key()` XPath function.  Built
/// lazily at evaluation time; structure captured here.
#[derive(Clone, Debug)]
pub struct Key {
    pub name:  QName,
    pub matcher: Expr,
    /// The `use=` expression.  Mutually exclusive with `body` (XSLT
    /// 2.0 §16.3 / XTSE1205); when the key declares a sequence
    /// constructor instead, this is `Expr::Sequence(vec![])` and
    /// `body` holds the constructor.
    pub use_:  Expr,
    /// Sequence-constructor form of the key value (XSLT 2.0 §16.3).
    /// Empty when the key uses the `use=` attribute; otherwise the
    /// `use=` expression is absent and the key value is computed by
    /// evaluating this constructor at each matched node.
    pub body:  Vec<Instr>,
    /// Effective collation URI for value comparison.  `None` means
    /// the codepoint collation (the XSLT default); a recognised
    /// non-codepoint URI changes how key('name', 'value') matches
    /// against the indexed `use=` results.
    pub collation: Option<String>,
}

/// `xsl:attribute-set` — a named bundle of attribute templates a
/// LiteralElement / xsl:element / xsl:copy can pull in via
/// `use-attribute-sets="…"`.
#[derive(Clone, Debug)]
pub struct AttributeSet {
    pub name:                 QName,
    pub use_attribute_sets:   Vec<QName>,
    /// Each `<xsl:attribute>` declared inside the set — stored as the
    /// full `Instr::Attribute` so `select=`, `separator=`,
    /// `namespace=`, and AVT names are preserved.  At apply time
    /// each entry runs through the standard attribute instruction
    /// path, which emits the resulting attribute onto the current
    /// element via the open ResultBuilder.
    pub attributes:           Vec<Instr>,
    /// XSLT 1.0 §2.6.2 import precedence — same scheme as Template.
    /// `apply_attribute_set_one` applies same-named sets in
    /// precedence order so a higher-precedence import overrides a
    /// lower one's attribute when both bind the same name.
    pub import_precedence:    i32,
}

/// `xsl:strip-space` / `xsl:preserve-space` directive set.  Order
/// of declaration matters per XSLT 1.0 §3.4; we preserve it.  Each
/// rule also carries the import precedence of the stylesheet
/// module that declared it: higher precedence wins under §3.4's
/// conflict-resolution rules regardless of specificity.
#[derive(Clone, Debug)]
pub enum WhitespaceRule {
    Strip(QName, i32),
    Preserve(QName, i32),
}

/// A fully compiled XSLT 1.0 stylesheet.  Holds enough state to
/// service repeated [`Stylesheet::apply`](crate::Stylesheet) calls
/// without re-parsing.
#[derive(Clone, Debug, Default)]
pub struct StylesheetAst {
    /// `version=` attribute on `xsl:stylesheet`.  Captured purely
    /// for compatibility (and forward-compat warnings later); the
    /// engine only implements 1.0 semantics.
    pub version:            String,
    /// Every `xmlns:p="…"` declaration in scope on the stylesheet
    /// root, plus the default namespace.  Used by the runtime to
    /// resolve prefixes in compiled XPath expressions (template
    /// match patterns, `select=`/`test=`/etc.).  Without this,
    /// stylesheets that use prefixed names inside XPath
    /// (e.g. `match="iso:pattern"`) can't resolve the prefix at
    /// evaluation time.
    pub namespaces:         std::collections::HashMap<String, String>,
    /// `xsl:namespace-alias` mappings: stylesheet-side URI →
    /// result-side URI.  When a literal result element (or
    /// attribute) has a namespace matching the stylesheet-side
    /// URI, the runtime rewrites it to the result-side URI on
    /// emit.  XSLT 1.0 §7.1.1.
    /// `(style_uri, result_uri, result_prefix)` triples gathered from
    /// `<xsl:namespace-alias>` declarations.  Per XSLT 1.0 §7.1.1 the
    /// result-prefix supplies the qualifier on the emitted name, not
    /// just the URI — `xmlns="a"` → `result-prefix="A"` means
    /// `<a:* xmlns:A="http://A.com/">` even though the source had no
    /// prefix.
    pub namespace_aliases:  Vec<(String, String, Option<String>)>,
    pub templates:          Vec<Template>,
    pub global_variables:   Vec<Variable>,
    pub global_params:      Vec<Param>,
    pub keys:               Vec<Key>,
    pub attribute_sets:     Vec<AttributeSet>,
    /// Named `<xsl:decimal-format>` declarations.  Keyed by name's
    /// expanded-name form (`{uri}local` for prefixed; `local` for
    /// unprefixed); the unnamed default decimal-format lives at the
    /// empty key `""`.  `format-number()`'s 3rd arg looks up here.
    pub decimal_formats:    std::collections::HashMap<String, crate::format_number::DecimalFormat>,
    /// Bitmask of attributes that were explicitly set on each named
    /// decimal-format declaration — used to merge multiple non-
    /// conflicting declarations with the same name and to flag XTSE1290
    /// conflicts only on attributes actually authored.
    /// Bits: 0 decimal-separator, 1 grouping-separator, 2 infinity,
    /// 3 minus-sign, 4 NaN, 5 percent, 6 per-mille, 7 zero-digit,
    /// 8 digit, 9 pattern-separator.
    pub decimal_format_explicit: std::collections::HashMap<String, u16>,
    /// Bitmask (same bit layout as `decimal_format_explicit`) of
    /// attributes on which two declarations of equal import precedence
    /// disagreed.  Such a clash is XTSE1290 only if it survives import
    /// merging — a higher-precedence declaration of the same attribute
    /// resolves it (XSLT 2.0 §16.4.2), so the check is deferred until
    /// every module has merged.
    pub decimal_format_conflicts: std::collections::HashMap<String, u16>,
    /// Per-attribute import precedence (same bit order, indexed 0..10) at
    /// which each decimal-format attribute's effective value was last
    /// set.  Merging visits modules highest-precedence-first; an
    /// attribute set at a higher precedence is never overwritten by a
    /// lower one (XSLT 2.0 §16.4.2).  `i32::MIN` marks "unset".
    pub decimal_format_attr_prec: std::collections::HashMap<String, [i32; 10]>,
    pub whitespace_rules:   Vec<WhitespaceRule>,
    pub outputs:            Vec<OutputSpec>,
    /// `xsl:include` href values — resolved during compilation if
    /// a loader is supplied; currently captured for diagnostics.
    pub includes:           Vec<String>,
    /// Includes paired with their source position in the
    /// stylesheet's top-level child list, for source-order-aware
    /// conflict resolution.  Same length and order as `includes`.
    pub include_positions:  Vec<u32>,
    /// `xsl:import` href values — same.
    pub imports:            Vec<String>,
    /// Every URI passed as a string-literal first argument to the
    /// XPath `document()` function anywhere in the stylesheet.
    /// Collected by [`crate::walk::collect_static_document_uris`]
    /// after compilation and pre-loaded at apply time so the
    /// runtime never has to resolve URIs while an XPath evaluation
    /// is in progress.  Empty when the stylesheet doesn't call
    /// `document()`, or only calls it with non-literal arguments
    /// (those raise a runtime error).
    pub documents_to_load:  Vec<String>,
    /// User-defined functions declared via `<xsl:function>` (XSLT 2.0).
    /// Empty for pure XSLT 1.0 stylesheets.  Keyed lookups at apply
    /// time use the function's expanded-name (`{uri}local`).
    pub functions:          Vec<UserFunction>,
    /// `<xsl:character-map>` declarations (XSLT 2.0 §20).  Resolved
    /// at serialization time when an `xsl:output` references them
    /// via `use-character-maps=`.  Map composition (one cmap can
    /// reference others) is flattened in
    /// [`StylesheetAst::resolve_character_maps`].
    pub character_maps:     Vec<CharacterMap>,
    /// `xml:base` attribute on the `xsl:stylesheet` root element,
    /// when present.  Forms the static base URI for XPath expressions
    /// in the stylesheet's default static context (XPath 2.0 §C.1).
    /// `fn:resolve-uri($rel)` and `fn:static-base-uri()` consult this.
    /// `None` falls back to the apply-time `base` argument.
    pub xml_base:           Option<String>,
    /// `<xsl:accumulator>` declarations (XSLT 3.0 §18).
    pub accumulators:       Vec<AccumulatorDecl>,
    /// `<xsl:use-package>` declarations (XSLT 3.0 §3.5.1) — resolved
    /// after the local stylesheet by name against the supplied package
    /// library; the used package's components merge in at a lower
    /// import precedence (like xsl:import), with xsl:override children
    /// taking precedence over the originals.
    pub use_packages:       Vec<UsePackage>,
    /// `<xsl:mode>` declarations (XSLT 3.0 §6.6).  A mode named here
    /// overrides the default built-in-template behaviour for that mode
    /// via its `on-no-match` action.  The unnamed (default) mode has
    /// `name: None`; modes used by templates but never declared keep
    /// the XSLT 1.0 default (`text-only-copy`).
    pub modes:              Vec<ModeDecl>,
    /// `input-type-annotations=` value(s) declared on this stylesheet
    /// module's root.  XSLT 2.0 §3.6 / XTSE0265 — across all modules
    /// of a package the value must be consistent: if any module asks
    /// `strip` while another asks `preserve`, the stylesheet is
    /// ill-formed.  We collect one entry per module (only when the
    /// attribute is present) and the post-merge check raises XTSE0265
    /// on conflict.
    pub input_type_annotations: Vec<String>,
}

/// One `<xsl:mode>` declaration (XSLT 3.0 §6.6).
#[derive(Clone, Debug)]
pub struct ModeDecl {
    /// Mode name; `None` for the unnamed default mode (`name="#default"`
    /// or no `name`).
    pub name:        Option<QName>,
    /// Action taken by the built-in template when no user template
    /// matches a node in this mode.
    pub on_no_match: OnNoMatch,
}

/// `on-no-match` action for a mode's built-in template (XSLT 3.0 §6.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnNoMatch {
    /// XSLT 1.0 default: recurse into document/element children;
    /// copy text nodes; everything else produces nothing.
    #[default]
    TextOnlyCopy,
    /// Produce nothing at all for the unmatched node.
    DeepSkip,
    /// Produce nothing for the node, but apply templates to its
    /// attributes and children.
    ShallowSkip,
    /// Shallow-copy the node, then apply templates to its attributes
    /// and children (identity-style).
    ShallowCopy,
    /// Copy the node and its entire subtree, with no further template
    /// application.
    DeepCopy,
    /// Raise a dynamic error (XTDE0555) when no template matches.
    Fail,
}

/// `<xsl:character-map>` declaration (XSLT 2.0 §20).  Maps single
/// characters to replacement strings; references to other named
/// maps compose.
#[derive(Clone, Debug)]
pub struct CharacterMap {
    pub name:                 QName,
    /// Other character maps referenced via
    /// `use-character-maps="…"`.  Applied first, then this map's
    /// own `<xsl:output-character>` entries (later wins).
    pub use_character_maps:   Vec<QName>,
    /// `(char, replacement)` pairs.  Single-char keys; replacement
    /// is emitted verbatim (no XML escaping) when the char appears
    /// in serialized text or attribute content.
    pub mappings:             Vec<(char, String)>,
}

/// A user-defined function declared by `<xsl:function name="…">`
/// (XSLT 2.0 § 10.3).  The body is a sequence constructor; the
/// function's value is whatever value the body produces.
#[derive(Clone, Debug)]
pub struct UserFunction {
    /// Expanded function name.  Per XSLT 2.0, the name MUST be
    /// prefixed (so it lives in some user namespace, never the
    /// default XPath function library).
    pub name:   QName,
    /// Parameters declared by `<xsl:param>` children, in source order.
    /// Callers must supply exactly this many arguments.
    pub params: Vec<Param>,
    /// Sequence constructor — runs to produce the function's value.
    pub body:   Vec<Instr>,
    /// XSLT 2.0 `as="xs:T"` on xsl:function — the declared return
    /// type.  Mismatches at call time raise XTTE0780.  `None` means
    /// `item()*` (any sequence is accepted).
    pub as_type: Option<String>,
    /// XSLT 3.0 `visibility=` (§3.5.2).  `None` = the package default
    /// (private).  Only `public`/`final`/`abstract` components are
    /// visible to a using package; `abstract` has no body and must be
    /// overridden before it can be called (XTDE3052).
    pub visibility: Option<String>,
}
