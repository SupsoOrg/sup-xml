//! Static streamability analysis — posture and sweep (XSLT 3.0 §19).
//!
//! Streaming processes a source document as a forward-only stream of
//! parse events, holding only a bounded *window* in memory: the current
//! node, its attributes/namespaces, and its ancestors.  Whether a given
//! expression can be evaluated against that window without buying back
//! content that has already streamed past (or hasn't arrived yet) is
//! decided *statically*, before any byte is read, by classifying every
//! construct with two properties computed bottom-up over the AST:
//!
//! * **Posture** — what the result of an expression refers to, relative
//!   to the streamed context item:
//!   - [`Posture::Grounded`]  — values / nodes not in the streamed tree
//!     (atomics, constructed or copied trees).  Freely usable.
//!   - [`Posture::Striding`]  — a horizontal slice of downward-selected
//!     nodes with no ancestor/descendant relationship among them; each
//!     can be processed as it streams by.  The canonical streamable
//!     posture.
//!   - [`Posture::Crawling`]  — like striding but the selected nodes may
//!     be in ancestor/descendant relationships (`descendant::`).
//!   - [`Posture::Climbing`]  — the current node's ancestors, attributes,
//!     or namespaces (the upward part of the window).  Always available;
//!     but you may not turn around and descend from a climbed node.
//!   - [`Posture::Roaming`]   — requires unrestricted navigation
//!     (`preceding::`, `following::`, sibling re-access, descending from
//!     an ancestor).  Not streamable.
//!
//! * **Sweep** — how evaluating the construct moves through the stream:
//!   - [`Sweep::Motionless`]  — does not advance the stream (reads an
//!     attribute, a name, an ancestor).
//!   - [`Sweep::Consuming`]   — reads the descendants of the context
//!     node, advancing the stream past them.
//!   - [`Sweep::FreeRanging`] — would need content outside the window.
//!     Not streamable.
//!
//! The two central rules this module enforces:
//!
//! 1. A downward axis step (`child`, `descendant`) is streamable only
//!    from a striding/crawling context; from a climbing or roaming
//!    context it becomes roaming.  An absolute path is rooted at the
//!    streamed document node, so `/a/b` and `//b` are downward
//!    selections from the root — striding/crawling, streamable — the
//!    same as the relative `a/b` evaluated at the document node.
//! 2. A construct may have **at most one consuming operand**, because
//!    the stream can be walked only once.  Two operands that each read
//!    the descendants of the same context node make the construct
//!    free-ranging.  (`xsl:fork` is the escape hatch — see
//!    [`crate::stream`].)
//!
//! This is a conservative analyzer: it implements the guaranteed-
//! streamable rules for the constructs that carry real streaming
//! workloads (path/axis composition, comparisons, the common function
//! library, and the sequence-constructor instructions handled in
//! [`crate::stream`]).  Constructs whose streamability it cannot prove
//! are classified free-ranging and rejected rather than mis-streamed —
//! soundness (never wrongly accept) takes priority over completeness.

use sup_xml_core::xpath::ast::{Axis, Expr, LocationPath, Step};

/// The posture of an expression's result relative to the streamed
/// context item.  See the module docs for the meaning of each value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Posture {
    Grounded,
    Striding,
    Crawling,
    Climbing,
    Roaming,
}

/// How evaluating a construct moves through the input stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sweep {
    Motionless,
    Consuming,
    FreeRanging,
}

/// The combined posture-and-sweep classification of a construct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ps {
    pub posture: Posture,
    pub sweep:   Sweep,
}

impl Ps {
    pub const fn new(posture: Posture, sweep: Sweep) -> Self {
        Ps { posture, sweep }
    }

    /// A grounded, motionless result — an atomic constant, or any value
    /// that neither touches the streamed tree nor advances the stream.
    pub const fn grounded() -> Self {
        Ps::new(Posture::Grounded, Sweep::Motionless)
    }

    /// The not-guaranteed-streamable result: roaming posture and a
    /// free-ranging sweep.  Any construct the analyzer cannot prove
    /// streamable collapses to this.
    pub const fn rejected() -> Self {
        Ps::new(Posture::Roaming, Sweep::FreeRanging)
    }

    /// True iff this classification is guaranteed-streamable: neither a
    /// roaming posture nor a free-ranging sweep.
    pub fn is_streamable(self) -> bool {
        self.posture != Posture::Roaming && self.sweep != Sweep::FreeRanging
    }
}

/// Apply one axis step to a context posture, yielding the step's result
/// posture and the sweep its evaluation contributes (XSLT 3.0 §19.8.7.7,
/// the axis-vs-context-posture table).
pub(crate) fn apply_axis(ctx: Posture, axis: Axis) -> Ps {
    use Axis::*;
    use Posture::*;
    use Sweep::*;

    match ctx {
        // Navigating a grounded (constructed/copied) tree is always
        // free: it is not part of the stream, so no axis consumes it.
        Grounded => Ps::new(Grounded, Motionless),
        // Once roaming, everything downstream is roaming.
        Roaming => Ps::rejected(),
        _ => match axis {
            // `self::` keeps the context posture and reads nothing.
            Self_ => Ps::new(ctx, Motionless),
            // Attributes and namespaces of the current node are in the
            // window: motionless, and a striding context stays striding.
            Attribute | Namespace => Ps::new(ctx, Motionless),
            // Upward axes reach the retained ancestor stack: climbing,
            // motionless, from any in-window context.
            Parent | Ancestor | AncestorOrSelf => Ps::new(Climbing, Motionless),
            // Downward axes consume the stream.  Valid only from a
            // striding/crawling context; from a climbed node the
            // descendants are out of the window, so it becomes roaming.
            Child => match ctx {
                Striding => Ps::new(Striding, Consuming),
                Crawling => Ps::new(Crawling, Consuming),
                _ => Ps::rejected(),
            },
            Descendant | DescendantOrSelf => match ctx {
                Striding | Crawling => Ps::new(Crawling, Consuming),
                _ => Ps::rejected(),
            },
            // Sibling / following / preceding navigation leaves the
            // window entirely.
            FollowingSibling | PrecedingSibling | Following | Preceding => Ps::rejected(),
        },
    }
}

/// The starting posture of a location path's first step.  Relative paths
/// start from the context posture; absolute paths start from the
/// document root.  In a streamed context the root is the streamed
/// document node, so an absolute downward path (`/a/b`, `//x`) is a
/// downward selection from it — *striding* — exactly as the W3C
/// streaming tests use `xsl:source-document` bodies that aggregate over
/// `/BOOKLIST/...`.  (A grounded context keeps its grounded root.)
fn path_root_posture(path: &LocationPath, ctx: Posture) -> Posture {
    match path {
        LocationPath::Relative(_) => ctx,
        LocationPath::Absolute(_) => match ctx {
            Posture::Grounded => Posture::Grounded,
            Posture::Roaming => Posture::Roaming,
            _ => Posture::Striding,
        },
    }
}

/// Analyze a location path: thread the posture through each step and
/// fold the per-step sweeps together (a path consumes if any step does;
/// it is free-ranging if any step roams).  Step predicates are analyzed
/// against the posture reached at that step and folded in under the
/// at-most-one-consuming rule.
fn analyze_path(path: &LocationPath, ctx: Posture) -> Ps {
    let steps: &[Step] = match path {
        LocationPath::Absolute(s) | LocationPath::Relative(s) => s,
    };

    let mut posture = path_root_posture(path, ctx);
    // An absolute path with no steps is the root node itself: climbing,
    // motionless.  A relative path with no steps is `.` (context).
    let mut sweep = Sweep::Motionless;

    for step in steps {
        if let Some(filter) = &step.filter {
            // XPath 2.0 FilterExpr step (`path/key(...)`, `path/(expr)`):
            // the primary produces its own sequence per input node.
            let f = analyze_expr(filter, posture);
            posture = f.posture;
            sweep = combine_strict(sweep, f.sweep);
        } else {
            let s = apply_axis(posture, step.axis);
            posture = s.posture;
            sweep = combine_max(sweep, s.sweep);
        }

        for pred in &step.predicates {
            // A predicate is evaluated with the step's nodes as context.
            // It inspects them; a consuming predicate is an additional
            // walk over the same nodes, governed by the one-consuming
            // rule.
            let p = analyze_expr(pred, posture);
            if p.posture == Posture::Roaming {
                return Ps::rejected();
            }
            sweep = combine_strict(sweep, p.sweep);
        }

        if posture == Posture::Roaming || sweep == Sweep::FreeRanging {
            return Ps::rejected();
        }
    }

    Ps::new(posture, sweep)
}

/// Combine two operand sweeps under the at-most-one-consuming rule
/// (XSLT 3.0 §19.8.5): two consuming operands sharing a context make the
/// construct free-ranging, because the stream can be walked only once.
pub(crate) fn combine_strict(a: Sweep, b: Sweep) -> Sweep {
    use Sweep::*;
    match (a, b) {
        (FreeRanging, _) | (_, FreeRanging) => FreeRanging,
        (Consuming, Consuming) => FreeRanging,
        (Consuming, _) | (_, Consuming) => Consuming,
        (Motionless, Motionless) => Motionless,
    }
}

/// Combine sweeps where the operands do not each independently re-walk
/// the context:
/// the steps of one path (which chain), the branches of a conditional
/// (only one runs), or the members of a set operation (one downward
/// pass).  The at-most-one-consuming rule does not apply.
pub(crate) fn combine_max(a: Sweep, b: Sweep) -> Sweep {
    use Sweep::*;
    match (a, b) {
        (FreeRanging, _) | (_, FreeRanging) => FreeRanging,
        (Consuming, _) | (_, Consuming) => Consuming,
        (Motionless, Motionless) => Motionless,
    }
}

/// Merge the postures of two node-set-producing branches (union, the two
/// arms of `if`).  Equal postures merge to themselves; a grounded branch
/// defers to the other; anything else is conservatively roaming.
fn merge_posture(a: Posture, b: Posture) -> Posture {
    use Posture::*;
    match (a, b) {
        _ if a == b => a,
        (Grounded, x) | (x, Grounded) => x,
        (Striding, Crawling) | (Crawling, Striding) => Crawling,
        _ => Roaming,
    }
}

/// Classify a function call.  Returns the call's posture/sweep given the
/// already-analyzed argument classifications and the function's local
/// name (the `fn:` / no-namespace built-ins; user/EXSLT functions take
/// the conservative default).
fn analyze_function(local: &str, args: &[Ps]) -> Ps {
    use Posture::*;
    use Sweep::*;

    // The combined sweep of all arguments under the one-consuming rule.
    let args_sweep = args.iter().fold(Motionless, |acc, a| combine_strict(acc, a.sweep));
    if args.iter().any(|a| a.posture == Roaming) || args_sweep == FreeRanging {
        return Ps::rejected();
    }

    // An "absorbing" function reads the *content* (string value, typed
    // value, or whole subtree) of a node argument, so it consumes the
    // stream whenever an argument denotes streamed nodes that may carry
    // descendants — i.e. a striding/crawling posture — even when
    // evaluating that argument is itself motionless (`copy-of(.)`).
    let absorbs = args.iter().any(|a| matches!(a.posture, Striding | Crawling));
    let absorb_sweep = if absorbs { Consuming } else { Motionless };

    match local {
        // Context-independent constants and the positional functions:
        // grounded, motionless, no operands of interest.
        "true" | "false" | "position" | "last" | "current-dateTime"
        | "current-date" | "current-time" | "default-collation"
        | "static-base-uri" | "implicit-timezone" => Ps::grounded(),

        // `root()` / the document root — climbing (reached upward),
        // motionless.
        "root" => Ps::new(Climbing, Motionless),

        // Reflective accessors read a node's own properties (name, base
        // URI, existence) without descending into its content: their
        // sweep is just the arguments' own sweep.
        "name" | "local-name" | "namespace-uri" | "node-name" | "base-uri"
        | "document-uri" | "generate-id" | "nilled" | "lang" | "has-children"
        | "exists" | "empty" | "count" => Ps::new(Grounded, args_sweep),

        // Atomizing / aggregating / copying a node argument reads its
        // content and yields a grounded value.  `snapshot` additionally
        // captures the ancestor spine; `copy-of` the whole subtree.
        "string" | "data" | "number" | "string-length" | "normalize-space"
        | "sum" | "avg" | "min" | "max" | "string-join"
        | "copy-of" | "snapshot" | "deep-equal"
        | "boolean" | "not" | "concat" | "subsequence" | "head" | "tail" => {
            // Reading the argument's content is the same forward pass as
            // selecting it, so absorption folds with `combine_max`, not
            // the one-consuming rule.
            Ps::new(Grounded, combine_max(args_sweep, absorb_sweep))
        }

        // Unknown (user-defined / EXSLT) function: result grounded.  A
        // node argument it might re-navigate would have to be grounded to
        // be passed soundly, so the arguments' combined sweep suffices.
        _ => Ps::new(Grounded, args_sweep),
    }
}

/// Classify an XPath expression in a streamable context whose context
/// item has posture `ctx`.  The result's [`Ps::is_streamable`] reports
/// whether the expression is guaranteed-streamable there.
pub fn analyze_expr(expr: &Expr, ctx: Posture) -> Ps {
    use Posture::*;

    match expr {
        // ── grounded leaves ─────────────────────────────────────────
        Expr::Literal(_) | Expr::Integer(_) | Expr::Decimal(_) | Expr::Double(_) => {
            Ps::grounded()
        }
        // A referenced variable holds an already-bound value; in a
        // streamable context that value must itself be grounded, so a
        // reference reads nothing.
        Expr::Variable(_) => Ps::grounded(),
        // `.` as a primary yields the context item unchanged.
        Expr::ContextItem => Ps::new(ctx, Sweep::Motionless),

        // ── paths ───────────────────────────────────────────────────
        Expr::Path(path) => analyze_path(path, ctx),
        Expr::FilterPath { primary, predicates, steps } => {
            let mut cur = analyze_expr(primary, ctx);
            for pred in predicates {
                let p = analyze_expr(pred, cur.posture);
                if p.posture == Roaming {
                    return Ps::rejected();
                }
                cur.sweep = combine_strict(cur.sweep, p.sweep);
            }
            for step in steps {
                let s = apply_axis(cur.posture, step.axis);
                cur.posture = s.posture;
                cur.sweep = combine_max(cur.sweep, s.sweep);
                for pred in &step.predicates {
                    let p = analyze_expr(pred, cur.posture);
                    if p.posture == Roaming {
                        return Ps::rejected();
                    }
                    cur.sweep = combine_strict(cur.sweep, p.sweep);
                }
                if cur.posture == Roaming || cur.sweep == Sweep::FreeRanging {
                    return Ps::rejected();
                }
            }
            cur
        }

        // ── set operations: combine postures, one downward pass ──────
        Expr::Union(a, b) | Expr::Intersect(a, b) | Expr::Except(a, b) => {
            let pa = analyze_expr(a, ctx);
            let pb = analyze_expr(b, ctx);
            Ps::new(merge_posture(pa.posture, pb.posture), combine_max(pa.sweep, pb.sweep))
        }

        // ── boolean / comparison / arithmetic: grounded result, the
        //    two operands each walk the context (one-consuming rule) ──
        Expr::Or(a, b) | Expr::And(a, b)
        | Expr::Eq(a, b) | Expr::Ne(a, b)
        | Expr::Lt(a, b) | Expr::Gt(a, b) | Expr::Le(a, b) | Expr::Ge(a, b)
        | Expr::ValueEq(a, b) | Expr::ValueNe(a, b)
        | Expr::ValueLt(a, b) | Expr::ValueGt(a, b)
        | Expr::ValueLe(a, b) | Expr::ValueGe(a, b)
        | Expr::Add(a, b) | Expr::Sub(a, b)
        | Expr::Mul(a, b) | Expr::Div(a, b) | Expr::Mod(a, b)
        | Expr::IDiv(a, b)
        | Expr::Range(a, b)
        | Expr::NodeBefore(a, b) | Expr::NodeAfter(a, b) | Expr::NodeIs(a, b) => {
            let pa = analyze_expr(a, ctx);
            let pb = analyze_expr(b, ctx);
            if pa.posture == Roaming || pb.posture == Roaming {
                return Ps::rejected();
            }
            Ps::new(Grounded, combine_strict(pa.sweep, pb.sweep))
        }
        Expr::Neg(a) => {
            let p = analyze_expr(a, ctx);
            Ps::new(Grounded, p.sweep)
        }

        // ── function calls ──────────────────────────────────────────
        Expr::FunctionCall(name, args) => {
            let arg_ps: Vec<Ps> = args.iter().map(|a| analyze_expr(a, ctx)).collect();
            let local = name.rsplit(':').next().unwrap_or(name);
            analyze_function(local, &arg_ps)
        }

        // ── conditional: cond consumes; one branch runs ─────────────
        Expr::IfThenElse { cond, then_branch, else_branch } => {
            let c = analyze_expr(cond, ctx);
            let t = analyze_expr(then_branch, ctx);
            let e = analyze_expr(else_branch, ctx);
            if [c.posture, t.posture, e.posture].contains(&Roaming) {
                return Ps::rejected();
            }
            let branch_sweep = combine_max(t.sweep, e.sweep);
            Ps::new(merge_posture(t.posture, e.posture), combine_strict(c.sweep, branch_sweep))
        }

        // ── binding constructs: the in-sequence is walked, the body is
        //    evaluated per item with the item as context ─────────────
        Expr::For { bindings, body } | Expr::Let { bindings, body } => {
            let mut sweep = Sweep::Motionless;
            let mut item_posture = ctx;
            for (_, seq) in bindings {
                let s = analyze_expr(seq, item_posture);
                if s.posture == Roaming {
                    return Ps::rejected();
                }
                sweep = combine_strict(sweep, s.sweep);
                // The bound variable iterates over the sequence's items;
                // for a `for`, the body's context item stays the outer
                // context, but each bound item carries the sequence's
                // posture for use inside the body via the variable
                // (treated grounded on reference).
                item_posture = ctx;
            }
            let b = analyze_expr(body, item_posture);
            if b.posture == Roaming {
                return Ps::rejected();
            }
            Ps::new(b.posture, combine_strict(sweep, b.sweep))
        }
        Expr::Quantified { bindings, test, .. } => {
            let mut sweep = Sweep::Motionless;
            for (_, seq) in bindings {
                let s = analyze_expr(seq, ctx);
                if s.posture == Roaming {
                    return Ps::rejected();
                }
                sweep = combine_strict(sweep, s.sweep);
            }
            let t = analyze_expr(test, ctx);
            if t.posture == Roaming {
                return Ps::rejected();
            }
            Ps::new(Grounded, combine_strict(sweep, t.sweep))
        }
        Expr::SimpleMap(a, b) => {
            let pa = analyze_expr(a, ctx);
            if pa.posture == Roaming {
                return Ps::rejected();
            }
            let pb = analyze_expr(b, pa.posture);
            if pb.posture == Roaming {
                return Ps::rejected();
            }
            Ps::new(pb.posture, combine_strict(pa.sweep, pb.sweep))
        }

        // ── parenthesised sequence: concatenation of independent
        //    operands; more than one consuming makes it free-ranging ──
        Expr::Sequence(items) => {
            let mut sweep = Sweep::Motionless;
            let mut posture = Grounded;
            for item in items {
                let p = analyze_expr(item, ctx);
                if p.posture == Roaming {
                    return Ps::rejected();
                }
                sweep = combine_strict(sweep, p.sweep);
                posture = merge_posture(posture, p.posture);
            }
            Ps::new(posture, sweep)
        }

        // ── type operations: inspect the operand, grounded result ────
        Expr::InstanceOf(a, _) | Expr::CastAs(a, _)
        | Expr::CastableAs(a, _) | Expr::TreatAs(a, _) => {
            let p = analyze_expr(a, ctx);
            if p.posture == Roaming {
                return Ps::rejected();
            }
            Ps::new(Grounded, p.sweep)
        }

        // ── synthetic wrappers added by the XSLT compiler ───────────
        Expr::WithDefaultCollation(_, inner) | Expr::BackwardsCompat(inner) => {
            analyze_expr(inner, ctx)
        }

        // Constructs whose streamability this analyzer does not (yet)
        // prove — maps, arrays, inline/dynamic functions, try/catch,
        // lookups, named-function references, placeholders.  Classified
        // not-guaranteed-streamable so they are rejected rather than
        // mis-streamed.
        _ => Ps::rejected(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sup_xml_core::xpath::parse_xpath;

    fn ps(src: &str, ctx: Posture) -> Ps {
        let expr = parse_xpath(src).expect("xpath parses");
        analyze_expr(&expr, ctx)
    }

    fn streamable(src: &str) -> bool {
        ps(src, Posture::Striding).is_streamable()
    }

    #[test]
    fn relative_downward_path_is_striding() {
        let r = ps("items/item", Posture::Striding);
        assert_eq!(r.posture, Posture::Striding);
        assert_eq!(r.sweep, Sweep::Consuming);
        assert!(r.is_streamable());
    }

    #[test]
    fn absolute_downward_path_is_striding() {
        // Rooted at the streamed document node, `/items/item` is a
        // downward selection — streamable, like the W3C source-document
        // tests that aggregate over `/BOOKLIST/...`.
        let r = ps("/items/item", Posture::Striding);
        assert_eq!(r.posture, Posture::Striding);
        assert!(r.is_streamable());
        // `//item` (descendant from root) is crawling — also streamable.
        assert!(streamable("//item"));
    }

    #[test]
    fn bare_root_is_streamable() {
        // `/` selects the streamed document node itself.
        let r = ps("/", Posture::Striding);
        assert!(r.is_streamable());
    }

    #[test]
    fn relative_descendant_is_crawling() {
        let r = ps("descendant::item", Posture::Striding);
        assert_eq!(r.posture, Posture::Crawling);
        assert!(r.is_streamable());
    }

    #[test]
    fn attribute_and_ancestor_axes_are_motionless() {
        let at = ps("@id", Posture::Striding);
        assert_eq!(at.posture, Posture::Striding);
        assert_eq!(at.sweep, Sweep::Motionless);

        let anc = ps("ancestor::section", Posture::Striding);
        assert_eq!(anc.posture, Posture::Climbing);
        assert_eq!(anc.sweep, Sweep::Motionless);
    }

    #[test]
    fn sibling_and_following_axes_roam() {
        assert!(!streamable("following-sibling::x"));
        assert!(!streamable("preceding::x"));
        assert!(!streamable("following::x"));
    }

    #[test]
    fn descending_from_an_ancestor_roams() {
        // climb to parent, then descend — out of window.
        assert!(!streamable("parent::node()/other/value"));
    }

    #[test]
    fn copy_of_current_is_grounded_consuming() {
        let r = ps("copy-of(.)", Posture::Striding);
        assert_eq!(r.posture, Posture::Grounded);
        assert_eq!(r.sweep, Sweep::Consuming);
        assert!(r.is_streamable());
    }

    #[test]
    fn two_downward_selections_in_one_comparison_are_free_ranging() {
        // string(a) = string(b): each side consumes the children.
        assert!(!streamable("string(a) = string(b)"));
    }

    #[test]
    fn motionless_predicate_keeps_path_streamable() {
        assert!(streamable("item[@type='x']"));
    }

    #[test]
    fn atomic_and_position_are_streamable() {
        assert!(streamable("position()"));
        assert!(streamable("42"));
        assert!(streamable("'literal'"));
    }

    #[test]
    fn union_of_downward_selections_is_striding() {
        let r = ps("a|b", Posture::Striding);
        assert_eq!(r.posture, Posture::Striding);
        assert!(r.is_streamable());
    }

    #[test]
    fn count_of_children_consumes_but_streams() {
        let r = ps("count(item)", Posture::Striding);
        assert_eq!(r.posture, Posture::Grounded);
        assert_eq!(r.sweep, Sweep::Consuming);
        assert!(r.is_streamable());
    }

    #[test]
    fn grounded_context_navigates_freely() {
        // From a grounded (constructed/copied) tree, even an absolute
        // path with descent never touches the stream.
        assert!(ps("/a/b/c", Posture::Grounded).is_streamable());
        assert!(ps("following::x", Posture::Grounded).is_streamable());
    }
}
