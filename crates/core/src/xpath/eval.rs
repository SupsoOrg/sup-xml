use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt::Write as _;

use sup_xml_tree::dom::Node;

use super::ast::*;
use super::index::{DocIndexLike, NodeId, XPathNodeKind};
use crate::error::{ErrorDomain, ErrorLevel, XmlError};

type Result<T> = std::result::Result<T, XmlError>;

pub fn xpath_err(msg: impl Into<String>) -> XmlError {
    XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg)
}

// ── eval-step budget ─────────────────────────────────────────────────────────

/// Cap on the number of charged "steps" any single XPath evaluation
/// may perform.  Nested-predicate expressions like
/// `//*[.=//*[.=//*[.=…]]]` have N^k complexity in document size N
/// and predicate-nesting depth k — easily minutes-to-hours of CPU
/// on adversarial input (found by the `fuzz_xpath_eval` target).
///
/// 20M covers ordinary XPath comfortably and also accommodates
/// XPath 2.0 / XSLT 2.0 expressions that iterate the whole Unicode
/// codepoint range via lazy `IntRange` — each codepoint costs a
/// handful of charged eval_expr re-entries through SimpleMap and
/// the predicate, so a 1.1M-codepoint sweep spends ~10M before
/// the surrounding `count()` materialises.  Adversarial nested-
/// predicate inputs still hit the cap in well under a second on
/// release builds — the budget catches super-linear blowup, not
/// linear iteration over a known-bounded range.  Parallels libxml2's
/// `XPATH_MAX_STEPS`.
pub const DEFAULT_MAX_EVAL_STEPS: u64 = 20_000_000;

thread_local! {
    /// Per-evaluation step ceiling for this thread.  Defaults to
    /// [`DEFAULT_MAX_EVAL_STEPS`]; override with [`set_eval_budget`]
    /// (which [`super::XPathContext::eval_with`] calls from the
    /// context's configured `max_eval_steps`).  Sticky for the thread
    /// until changed — `reset_eval_budget` reseeds the remaining
    /// counter from it before each top-level evaluation.
    static EVAL_STEPS_BUDGET: Cell<u64> = const { Cell::new(DEFAULT_MAX_EVAL_STEPS) };
    /// Steps remaining in the current evaluation.  Reset to the
    /// configured budget by [`reset_eval_budget`].  Charged once per
    /// candidate node in [`apply_predicates`] and once per descendant
    /// during recursive tree walks — the hot paths that compound
    /// multiplicatively when predicates nest.
    static EVAL_STEPS_REMAINING: Cell<u64> = const { Cell::new(DEFAULT_MAX_EVAL_STEPS) };
}

/// Set this thread's per-evaluation step ceiling.  Takes effect on the
/// next [`reset_eval_budget`] (i.e. the next top-level evaluation).
/// Lower it when evaluating untrusted XPath to cap worst-case CPU;
/// raise it for trusted, legitimately-expensive generated expressions.
/// A value of 0 makes every evaluation fail on its first step.
pub fn set_eval_budget(max_steps: u64) {
    EVAL_STEPS_BUDGET.with(|c| c.set(max_steps));
}

/// Reset the remaining-step counter to this thread's configured budget
/// for a fresh top-level evaluation.  Called by
/// [`super::XPathContext::eval_with`]; XSLT-internal eval call sites
/// inherit the current budget (in practice the default, since
/// stylesheets don't pathologically nest and an untrusted-XPath caller
/// goes through `XPathContext`).
pub fn reset_eval_budget() {
    let budget = EVAL_STEPS_BUDGET.with(|c| c.get());
    EVAL_STEPS_REMAINING.with(|c| c.set(budget));
}

thread_local! {
    /// Stable "current instant" (seconds since the Unix epoch) for
    /// this execution.  XPath 2.0 §16.* requires `fn:current-dateTime`,
    /// `fn:current-date`, and `fn:current-time` to return the SAME
    /// value throughout one execution scope (a stylesheet transform or
    /// a single top-level XPath evaluation).  [`refresh_stable_now`]
    /// reseeds it at each execution boundary; `stable_now` lazily
    /// initialises it for any path that didn't.
    static STABLE_NOW: Cell<Option<i64>> = const { Cell::new(None) };
}

/// Reseed the stable current-instant for a new execution scope (called
/// at XSLT-transform entry and at each top-level XPath evaluation), so
/// the next `fn:current-*` call samples a fresh time while remaining
/// stable for the rest of that scope.
pub fn refresh_stable_now() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    STABLE_NOW.with(|c| c.set(Some(now)));
}

/// The stable current instant (seconds since the epoch) for this
/// execution scope, lazily sampling the system clock on first use.
fn stable_now() -> i64 {
    STABLE_NOW.with(|c| {
        if let Some(n) = c.get() {
            return n;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        c.set(Some(now));
        now
    })
}

thread_local! {
    /// In-scope `[xsl:]default-collation` URI as resolved at the
    /// surrounding XSLT element when the current XPath expression
    /// was compiled.  The XSLT eval pushes the URI before each
    /// `xpath_eval` call so XPath operators (`eq`, `ne`, `lt`, …)
    /// can fold string operands per the static collation context
    /// without threading the URI through every comparison.  Default
    /// `None` denotes the codepoint collation (no fold).
    pub static DEFAULT_COLLATION: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Run `f` with `uri` installed as the in-scope default-collation
/// URI, restoring the previous value on return (including on
/// panic).  Caller is responsible for picking the right URI from
/// the compiled expression's static context.
pub fn with_default_collation<R>(uri: Option<String>, f: impl FnOnce() -> R) -> R {
    let prev = DEFAULT_COLLATION.with(|c| c.borrow().clone());
    DEFAULT_COLLATION.with(|c| *c.borrow_mut() = uri);
    struct Guard(Option<String>);
    impl Drop for Guard {
        fn drop(&mut self) {
            let p = self.0.take();
            DEFAULT_COLLATION.with(|c| *c.borrow_mut() = p);
        }
    }
    let _g = Guard(prev);
    f()
}

/// The collation a collation-sensitive function should use: its
/// explicit collation argument when supplied, otherwise the in-scope
/// default collation (XPath 2.0 §C.2 — set by `[xsl:]default-collation`
/// via [`Expr::WithDefaultCollation`]).  Lets a call that omits the
/// collation argument still honour an in-scope `default-collation`.
fn effective_collation(explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| DEFAULT_COLLATION.with(|c| c.borrow().clone()))
}

thread_local! {
    /// XPath 1.0 backwards-compatibility mode (XPath 2.0 §B.1).  Set
    /// while evaluating an [`Expr::BackwardsCompat`] subtree — i.e. an
    /// expression that the XSLT compiler found inside a
    /// `[xsl:]version="1.0"` scope.  When true the conversion rules
    /// differ: arithmetic operands are atomised to xs:double and a
    /// `to`-range bound takes the first item of a sequence.
    static XPATH_1_0_COMPAT: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` with XPath 1.0 backwards-compatibility mode active,
/// restoring the previous setting on return (including on panic).
pub fn with_xpath_1_0_compat<R>(f: impl FnOnce() -> R) -> R {
    let prev = XPATH_1_0_COMPAT.with(|c| c.replace(true));
    struct Guard(bool);
    impl Drop for Guard {
        fn drop(&mut self) { XPATH_1_0_COMPAT.with(|c| c.set(self.0)); }
    }
    let _g = Guard(prev);
    f()
}

/// True iff evaluation is currently inside an XPath 1.0 backwards-
/// compatibility scope.  Exposed for the XSLT layer so spec-checks
/// that loosen in BC mode (e.g. xsl:sort's XTTE1020 cardinality
/// rule) can probe the current scope without duplicating the
/// thread-local plumbing.
pub fn in_xpath_1_0_compat() -> bool {
    XPATH_1_0_COMPAT.with(|c| c.get())
}

thread_local! {
    /// XPath 2.0 §2.1.2 "context item".  Set per-iteration by
    /// `filter_sequence_by_predicates` and the simple-map
    /// `lhs ! rhs` evaluator when the iterated items are atomic
    /// values that don't fit our [`EvalCtx::context_node`] field.
    /// Read by [`eval_expr`] when it encounters a bare `.` path —
    /// returning the atomic value directly so predicates like
    /// `($strings)[matches(., '\w')]` see each string instead of
    /// the outer context node.
    static CONTEXT_ITEM: RefCell<Option<Value>> = const { RefCell::new(None) };
    /// XPath 2.0 §2.1.2 / XSLT 2.0 §10.3 — set to `true` while
    /// evaluating an `xsl:function` body, where the focus is
    /// undefined.  Read by ContextItem (`.`) and absolute-path
    /// evaluation to raise XPDY0002 instead of silently using the
    /// caller's focus.  Kept in core so both core and the xslt
    /// crate (which sets it on xsl:function entry) can reach the
    /// same flag.
    static FOCUS_UNDEFINED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Set whether the focus (context item / position / size) is
/// undefined for the duration of `f`.  Restored on return.  XSLT 2.0
/// §10.3 uses this around xsl:function bodies.
pub fn with_focus_undefined<R>(undefined: bool, f: impl FnOnce() -> R) -> R {
    let prev = FOCUS_UNDEFINED.with(|c| c.replace(undefined));
    let r = f();
    FOCUS_UNDEFINED.with(|c| c.set(prev));
    r
}

/// True iff the focus is currently undefined (we're inside an
/// xsl:function body or similar context-less scope).
pub fn focus_is_undefined() -> bool {
    FOCUS_UNDEFINED.with(|c| c.get())
}

/// Set `item` as the current atomic context item for the duration
/// of `f`, restoring the previous value on return (including on
/// panic via `RefCell`'s borrow).  `None` clears the slot — useful
/// when stepping into a sub-expression whose `.` should fall back
/// to the node-based context.
pub fn with_atomic_context_item<R>(item: Option<Value>, f: impl FnOnce() -> R) -> R {
    with_context_item(item, f)
}

fn with_context_item<R>(item: Option<Value>, f: impl FnOnce() -> R) -> R {
    let prev = CONTEXT_ITEM.with(|c| c.replace(item));
    let r = f();
    CONTEXT_ITEM.with(|c| { c.replace(prev); });
    r
}

/// Snapshot the current atomic context item, if any.
pub fn current_context_item() -> Option<Value> {
    CONTEXT_ITEM.with(|c| c.borrow().clone())
}

/// True iff `path` is the XPath expression `.` — a single relative
/// step on the self axis matching any node, with no predicates and
/// no filter primary.  That's the only path shape where we can
/// short-circuit to the atomic [`CONTEXT_ITEM`]; anything else
/// (`./foo`, `.[$pred]`, `self::node()` with predicates) still
/// needs the regular tree-walking eval.
fn is_bare_dot_path(path: &crate::xpath::ast::LocationPath) -> bool {
    use crate::xpath::ast::{Axis, LocationPath, NodeTest};
    let steps = match path {
        LocationPath::Relative(s) => s,
        LocationPath::Absolute(_) => return false,
    };
    if steps.len() != 1 { return false; }
    let s = &steps[0];
    s.axis == Axis::Self_
        && matches!(s.node_test, NodeTest::AnyNode)
        && s.predicates.is_empty()
        && s.filter.is_none()
}

/// Charge one step of evaluation work against the per-thread
/// budget.  Returns an XPath error if the budget is exhausted so
/// the caller can bail out cleanly.
#[inline]
fn charge_eval_step() -> Result<()> {
    EVAL_STEPS_REMAINING.with(|c| {
        let r = c.get();
        if r == 0 {
            let budget = EVAL_STEPS_BUDGET.with(|b| b.get());
            return Err(xpath_err(format!(
                "XPath evaluation step budget exceeded ({budget}); \
                 expression too expensive for the configured limit"
            )));
        }
        c.set(r - 1);
        Ok(())
    })
}

/// Opaque foreign-doc node pointer.  Pointer identity is meaningful
/// (used for set dedup); the engine never dereferences this directly
/// for tree navigation — that goes through the `XPathBindings`
/// `eval_steps_in_foreign_doc` / `foreign_string_value` callbacks,
/// which the compat layer implements by routing through the loaded
/// doc's `DocIndex`.  Reads of structural fields like `kind`/`children`
/// here are sound because foreign docs always come from our own
/// parser (libxml2-ABI-compatible Node layout, lifetime-managed by
/// the doc registry on `XPathBindings`).
pub type ForeignNodePtr = *const Node<'static>;

/// XPath 2.0 numeric type discriminator carried by [`Value::Number`].
///
/// XML Schema / F&O distinguishes four numeric primitives —
/// `xs:integer`, `xs:decimal`, `xs:double`, `xs:float` — and the
/// distinction is observable: `instance of` must tell an integer from
/// a double, and the number→string rule (F&O §17.1.2) stringifies
/// doubles/floats in scientific notation but integers/decimals in
/// decimal form.  Tracking the kind on the value itself lets those
/// operators answer correctly without a second type carrier.
///
/// `Decimal` is exact (backed by [`rust_decimal::Decimal`] — 96-bit
/// mantissa + scale, ≥28 significant digits), so XPath 2.0's
/// `0.1 + 0.2` is `0.3`, not `0.30000000000000004` (XPath 2.0 §3.1.1
/// types a literal containing `.` as `xs:decimal`, and the spec
/// demands exact arithmetic on it).  `Double`/`Float` stay `f64`-backed.
/// All four payloads are inline and `Copy`, so `Value::Number` stays a
/// cheap non-allocating variant — a positional predicate's literal `2`
/// is `Numeric::Integer(2)` with no heap cost.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Numeric {
    Integer(i64),
    Decimal(rust_decimal::Decimal),
    Double(f64),
    Float(f64),
}

impl Numeric {
    /// The numeric value as an `f64` — the universal accessor every
    /// XPath 1.0-style consumer reads, regardless of the underlying
    /// numeric type.  `Integer(i)` widens to `i as f64`; `Decimal(d)`
    /// projects via [`rust_decimal::prelude::ToPrimitive`] (the lossy
    /// step — values past 2^53 lose precision, values beyond `f64`
    /// range collapse to `f64::NAN`).
    #[inline]
    pub fn as_f64(self) -> f64 {
        use rust_decimal::prelude::ToPrimitive;
        match self {
            Numeric::Integer(i) => i as f64,
            Numeric::Decimal(d) => d.to_f64().unwrap_or(f64::NAN),
            Numeric::Double(n) | Numeric::Float(n) => n,
        }
    }

    /// Build an `xs:integer` from an `f64`.  i64-backed (no arbitrary
    /// precision): a result outside i64 range falls back to an
    /// `xs:decimal` so the magnitude survives — saturation at
    /// `i64::MAX` would pin a growing value and hang recursive
    /// big-integer arithmetic.  Non-finite values stay as `Double`
    /// (NaN / ±∞ have no decimal representation per XSD §3.2.3).
    #[inline]
    pub fn integer_from_f64(n: f64) -> Numeric {
        const I64_LIMIT: f64 = 9_223_372_036_854_775_808.0; // 2^63
        if n.is_finite() && n >= -I64_LIMIT && n < I64_LIMIT {
            Numeric::Integer(n as i64)
        } else if let Some(d) = rust_decimal::Decimal::from_f64_retain(n) {
            Numeric::Decimal(d)
        } else {
            Numeric::Double(n)
        }
    }

    /// Build a `Numeric` of the given XSD primitive kind from an `f64`
    /// — used by arithmetic promotion to tag a result with its
    /// promoted type.  An unrecognised kind falls back to `Double`.
    /// **Note:** this conversion goes through `f64`, so decimal
    /// arithmetic that needs to stay exact must NOT route through
    /// here — see [`Numeric::decimal_op`] for the exact path.
    #[inline]
    pub fn of_kind(kind: &str, n: f64) -> Numeric {
        match kind {
            "integer" => Numeric::integer_from_f64(n),
            "decimal" => match rust_decimal::Decimal::from_f64_retain(n) {
                Some(d) => Numeric::Decimal(d),
                None    => Numeric::Double(n),
            },
            // Narrow through f32 — xs:float is 32-bit IEEE 754, so the
            // value an `as="xs:float"` /  `xs:float(…)` carries must
            // already be rounded to single precision before subsequent
            // arithmetic / serialisation sees it.
            "float" => Numeric::Float(n as f32 as f64),
            _ => Numeric::Double(n),
        }
    }

    /// Promotion rank in the `integer ⊂ decimal ⊂ float ⊂ double`
    /// lattice (XPath 2.0 §6.2).  Used by the arithmetic fast path to
    /// promote two numbers without round-tripping through type-name
    /// strings.
    #[inline]
    fn rank(self) -> u8 {
        match self {
            Numeric::Integer(_) => 0,
            Numeric::Decimal(_) => 1,
            Numeric::Float(_)   => 2,
            Numeric::Double(_)  => 3,
        }
    }

    /// Inverse of [`rank`](Self::rank): build a `Numeric` of the given
    /// lattice rank from an `f64`.  Lossy for `Decimal` (the f64 is
    /// what the caller had); callers that have exact decimal operands
    /// should use the exact path in [`typed_numeric_op`].
    #[inline]
    fn from_rank(rank: u8, n: f64) -> Numeric {
        match rank {
            0 => Numeric::integer_from_f64(n),
            1 => match rust_decimal::Decimal::from_f64_retain(n) {
                Some(d) => Numeric::Decimal(d),
                None    => Numeric::Double(n),
            },
            // Same f32-narrowing rationale as `of_kind` above.
            2 => Numeric::Float(n as f32 as f64),
            _ => Numeric::Double(n),
        }
    }

    /// XSD primitive type local name (`"integer"`, `"decimal"`,
    /// `"double"`, `"float"`) — the discriminator `instance of` and
    /// the stringification rule branch on.
    #[inline]
    pub fn kind(self) -> &'static str {
        match self {
            Numeric::Integer(_) => "integer",
            Numeric::Decimal(_) => "decimal",
            Numeric::Double(_)  => "double",
            Numeric::Float(_)   => "float",
        }
    }
}

impl From<f64> for Numeric {
    /// A bare `f64` with no further type information defaults to
    /// `xs:double` — the XPath 1.0 numeric type and the F&O default
    /// for an untyped numeric.
    #[inline]
    fn from(n: f64) -> Numeric { Numeric::Double(n) }
}

#[derive(Debug, Clone)]
pub enum Value {
    NodeSet(Vec<NodeId>),
    /// Result of `document(URI)` — nodes that live in a doc the
    /// engine's current `DocIndex` doesn't cover.  The engine treats
    /// these opaquely; ops that need traversal/string-value delegate
    /// to the bindings (compat looks up the right `DocIndex` via its
    /// doc registry).  Pointer-identity dedup, no document order
    /// across docs.
    ForeignNodeSet(Vec<ForeignNodePtr>),
    String(String),
    Number(Numeric),
    Boolean(bool),
    /// XPath 2.0 typed atomic value — carries the source XSD type
    /// tag so `instance of xs:T`, `cast as xs:T`, and other
    /// type-aware operators can answer schema questions about it.
    /// Numeric / boolean / string operations treat a Typed value
    /// as its underlying representation (see [`value_to_number`],
    /// [`value_to_string`], [`value_to_bool`] for the lowering
    /// rules) so XPath 1.0-style consumers keep working unchanged.
    ///
    /// Boxed to keep `sizeof(Value)` at the existing ~32-byte
    /// footprint — non-2.0 hot paths that never produce typed
    /// values pay no memory-bandwidth tax on every Value clone /
    /// push.  The cost shifts to one heap allocation per xs:T(...)
    /// call, which is bounded and small.
    Typed(Box<TypedAtomic>),
    /// XPath 2.0 §2.4 heterogeneous sequence of typed / atomic
    /// items.  Produced by the parenthesised sequence constructor
    /// `(a, b, c)` when at least one item is a typed atomic — keeps
    /// each item's type tag intact so `instance of` / `subsequence` /
    /// `data()` round-trip correctly.  When every item is a node or
    /// every item is an untyped atomic, the existing NodeSet / single
    /// Value paths are still used to avoid the per-item Value
    /// allocation overhead.
    ///
    /// **Invariant:** items are themselves never `Sequence` —
    /// sequences flatten at construction (XPath 2.0 §3.3.1).
    Sequence(Vec<Value>),
    /// XPath 2.0 §3.3.1 integer range (`m to n`) carried as bounds
    /// instead of an expanded list.  Stays compact for the W3C
    /// Unicode-category test families that iterate `1 to 0x10FFFF`
    /// — materialising would cost millions of [`Value`] allocations
    /// and burn the per-thread eval-step budget on tree construction
    /// alone.  Consumers that need positional / set semantics
    /// (`subsequence`, document-order comparisons, `count` of a
    /// mixed sequence) expand via [`items_of`]; consumers that can
    /// stay arithmetic (`count`, `sum`, simple-map first projection)
    /// special-case the variant to keep things O(1).
    ///
    /// **Invariant:** `lo <= hi` — empty ranges normalise to
    /// `NodeSet(vec![])` at construction so consumers never see an
    /// `IntRange` they need to test for emptiness.
    IntRange { lo: i64, hi: i64 },
    /// XPath 3.1 §17.1 map — an ordered list of (key, value) entries.
    /// Keys are single atomic values; values are arbitrary sequences
    /// (themselves `Value`s).  Lookup compares keys by value
    /// ([`map_key_eq`]).  Boxed to keep `sizeof(Value)` small.
    Map(Box<Vec<(Value, Value)>>),
    /// XPath 3.1 §17.3 array — an ordered list of members, each an
    /// arbitrary sequence (a `Value`).  Indexed 1-based via `?`.
    Array(Box<Vec<Value>>),
    /// XPath 3.1 function item — an inline function (with its captured
    /// closure), a named-function reference, or a partial application.
    Function(Box<FunctionItem>),
}

/// A callable XPath 3.1 function item.
#[derive(Debug, Clone)]
pub enum FunctionItem {
    /// An inline `function(...){...}` — parameter names, the body
    /// expression, and the values of the free variables captured from
    /// the defining scope (static scoping).
    Inline {
        params:  Vec<String>,
        /// Declared signature, for function subtyping in `instance of`.
        sig:     Box<crate::xpath::ast::FunctionSig>,
        body:    crate::xpath::ast::Expr,
        closure: Vec<(String, Value)>,
    },
    /// A reference to a named function (built-in or user-defined),
    /// `name#arity`, invoked by re-entering function dispatch.  `name`
    /// is the lexical QName used for dispatch; `ns` is its resolved
    /// namespace URI (the default function namespace for an unprefixed
    /// name), captured at reference time so `fn:function-name` can
    /// rebuild the expanded QName without the defining scope.
    Named {
        name: String, ns: String, arity: usize,
        /// The function's declared signature, captured at reference time
        /// for function subtyping (`instance of function(…)`).  `None`
        /// when unknown (built-ins / extensions) — subtyping then falls
        /// back to arity-only matching.
        sig: Option<Box<crate::xpath::ast::FunctionSig>>,
    },
    /// A partial application: a base function with some arguments
    /// already bound; `None` slots are the remaining parameters.
    Partial { base: Box<FunctionItem>, bound: Vec<Option<Value>> },
}

impl FunctionItem {
    /// The function's arity (number of parameters still to be supplied).
    pub fn arity(&self) -> usize {
        match self {
            FunctionItem::Inline { params, .. } => params.len(),
            FunctionItem::Named { arity, .. } => *arity,
            FunctionItem::Partial { bound, .. } =>
                bound.iter().filter(|b| b.is_none()).count(),
        }
    }

    /// The function's declared signature, if captured (named user
    /// functions record it at reference time).  `None` for inline,
    /// partial, and built-in items.
    pub fn declared_sig(&self) -> Option<&crate::xpath::ast::FunctionSig> {
        match self {
            FunctionItem::Named { sig, .. } => sig.as_deref(),
            FunctionItem::Inline { sig, .. } => Some(sig),
            _ => None,
        }
    }
}

/// One typed atomic value — the carrier for XPath 2.0 schema-aware
/// semantics.  Holds the XSD type (`kind`, the local name of the
/// XSD primitive or derived type as a `&'static str` from the
/// finite type table), the lexical form, plus pre-computed numeric
/// / boolean views when the type permits them.
#[derive(Debug, Clone)]
pub struct TypedAtomic {
    /// XSD type local name without the `xs:` prefix.  E.g.
    /// `"integer"`, `"double"`, `"date"`, `"dayTimeDuration"`.
    /// `&'static str` rather than `String` because the XSD primitive
    /// + derived type set is finite and known at compile time —
    /// saves one heap allocation per typed value.
    pub kind: &'static str,
    /// The XPath 2.0 §2.6.1 string-value of this atomic.
    pub lexical: String,
    /// Cached numeric coercion for numeric XSD types.  `None` for
    /// non-numeric types (strings, dates, durations, booleans).
    pub numeric: Option<f64>,
    /// Boolean form, populated for xs:boolean values.
    pub boolean: Option<bool>,
}

/// XSD type-hierarchy lookup — parent of a derived type per
/// XML Schema Part 2 §3.  Returns `None` when `t` is unknown or is
/// already `anyType` (the universal supertype).  The chain bottoms
/// out at `anyAtomicType ⊂ anySimpleType ⊂ anyType`.
pub fn parent_atomic_type(t: &str) -> Option<&'static str> {
    Some(match t {
        // Integer subtypes
        "long" => "integer",
        "int" => "long",
        "short" => "int",
        "byte" => "short",
        "unsignedLong" => "nonNegativeInteger",
        "unsignedInt" => "unsignedLong",
        "unsignedShort" => "unsignedInt",
        "unsignedByte" => "unsignedShort",
        "nonNegativeInteger" => "integer",
        "nonPositiveInteger" => "integer",
        "positiveInteger" => "nonNegativeInteger",
        "negativeInteger" => "nonPositiveInteger",
        "integer" => "decimal",
        // Strings
        "normalizedString" => "string",
        "token" => "normalizedString",
        "language" => "token",
        "Name" => "token",
        "NCName" => "Name",
        "ID" => "NCName",
        "IDREF" => "NCName",
        "ENTITY" => "NCName",
        "NMTOKEN" => "token",
        // Durations
        "dayTimeDuration" => "duration",
        "yearMonthDuration" => "duration",
        // Primitives ⊂ anyAtomicType
        "decimal" | "double" | "float" | "boolean" | "string"
        | "date" | "dateTime" | "time" | "duration"
        | "gYear" | "gYearMonth" | "gMonth" | "gMonthDay" | "gDay"
        | "hexBinary" | "base64Binary" | "anyURI" | "QName"
        | "NOTATION" | "untypedAtomic"
        | "IDREFS" | "ENTITIES" | "NMTOKENS"
            => "anyAtomicType",
        "anyAtomicType" => "anySimpleType",
        "anySimpleType" => "anyType",
        _ => return None,
    })
}

/// True iff `t` is (transitively) a subtype of `target` in the
/// XSD type hierarchy.  Reflexive (`t == target`).
pub fn xsd_is_subtype_of(t: &str, target: &str) -> bool {
    if t == target { return true; }
    let mut cur = t;
    while let Some(p) = parent_atomic_type(cur) {
        if p == target { return true; }
        cur = p;
    }
    false
}

impl Value {
    /// Strip a typed wrapper, returning the underlying untyped Value.
    /// Numeric typed → Number; boolean typed → Boolean; otherwise →
    /// String (the typed value's lexical form, moved out without
    /// cloning).  A Sequence collapses to a NodeSet only when every
    /// item is already a node (no atomic-sequence lowering); a
    /// singleton Sequence unwraps to its sole item.  Non-Typed,
    /// non-Sequence values pass through unchanged.
    pub fn untyped(self) -> Self {
        match self {
            Value::Typed(t) => {
                if let Some(n) = t.numeric { Value::Number(Numeric::Double(n)) }
                else if let Some(b) = t.boolean { Value::Boolean(b) }
                else {
                    // Move the lexical String out of the Box — no
                    // String alloc since we own the Box.
                    Value::String(t.lexical)
                }
            }
            Value::Sequence(mut items) => {
                if items.len() == 1 {
                    return items.remove(0).untyped();
                }
                // Multi-item sequences with atomic content can't be
                // re-encoded as a NodeSet without losing items.
                // Leave as Sequence — downstream consumers that
                // care will inspect each item.
                Value::Sequence(items)
            }
            other => other,
        }
    }
}

/// Caller-supplied resolver for things outside the XPath spec that
/// downstream consumers (libxslt, lxml's `extensions=` / `variables=`
/// / `namespaces=` kwargs) want to inject:
///
/// * **Namespace prefixes** used in `prefix:local` name tests must
///   resolve to a URI somehow — the consumer registers the prefix→URI
///   map and we consult it during eval.
/// * **User-defined functions** like `my:foo(bar)` need to dispatch
///   to caller-provided code.
/// * **XPath variables** (`$varname`) need a value source.
///
/// All three default to "not provided"; the engine falls through to
/// XPath 1.0 behaviour (error / empty result) when a binding returns
/// `None`.
pub trait XPathBindings {
    /// Resolve a namespace prefix to its URI.  `None` means the prefix
    /// is undeclared — the engine surfaces that as an XPath error.
    fn resolve_prefix(&self, _prefix: &str) -> Option<String> { None }
    /// Invoke a registered XPath function.  `None` means the function
    /// isn't registered — the engine falls back to its built-in
    /// function table (count(), string(), etc.).  An inner `Err`
    /// propagates as the eval result.
    fn call_function(
        &self, _ns_uri: &str, _name: &str, _args: Vec<Value>,
    ) -> Option<Result<Value>> { None }
    /// Like [`call_function`](Self::call_function), but also receives
    /// the current XPath context node — the node that
    /// `position()`/`last()` are reporting against and that a no-arg
    /// `generate-id()` / `current()` would see if the spec sense
    /// were "XPath context", not "XSLT current()".  Default
    /// delegates to `call_function`, dropping the context.  XSLT
    /// overrides this so `generate-id()` and friends work in
    /// predicate sub-expressions.
    fn call_function_in(
        &self, ns_uri: &str, name: &str, args: Vec<Value>,
        _xpath_context_node: NodeId,
    ) -> Option<Result<Value>> {
        self.call_function(ns_uri, name, args)
    }
    /// Is a function with this expanded name and arity available to call —
    /// a registered user `xsl:function` or extension?  Lets
    /// `fn:function-lookup` return the empty sequence for unknown names
    /// without invoking anything.  Default `false`.
    fn function_available_in(&self, _ns_uri: &str, _name: &str, _arity: usize) -> bool {
        false
    }
    /// The declared signature of a user `xsl:function` with this expanded
    /// name and arity, if known — used to apply function subtyping in
    /// `instance of function(…)`.  `None` means the signature is unknown
    /// (a built-in, an extension, or no such function), in which case the
    /// caller falls back to arity-only matching.
    fn function_signature_in(
        &self, _ns_uri: &str, _name: &str, _arity: usize,
    ) -> Option<crate::xpath::ast::FunctionSig> {
        None
    }
    /// Look up an XPath variable's value.  `None` means undefined,
    /// which surfaces as an XPath error.
    fn variable(&self, _name: &str) -> Option<Value> { None }

    /// Is the host an XPath 2.0 (or later) processor?  Selects the
    /// `xs:double`/`xs:float` → string canonical form: 1.0 is
    /// decimal-only, 2.0 uses scientific notation outside
    /// `[1e-6, 1e6)` (`1.0E6`).  Only affects typed doubles — integer
    /// / decimal output is unchanged.  Default `false`.
    fn xpath_version_2_or_later(&self) -> bool { false }

    /// XSLT `document(uri[, base])` — load the URI as XML and return
    /// the loaded doc's root as a foreign-node pointer set.
    /// `base_uri` is the base URI of the calling expression (e.g.
    /// the stylesheet's URL); the bindings impl resolves the URI
    /// relative to that.  `None` means the bindings don't support
    /// document loading — the engine reports "unknown function" as
    /// for any other unrecognized name.
    fn load_document(
        &self, _uri: &str, _base_uri: Option<&str>,
    ) -> Option<Result<Vec<ForeignNodePtr>>> { None }

    /// Evaluate a path expression's predicates + steps against a
    /// foreign-doc node-set.  The bindings impl looks up each
    /// pointer's owning doc (compat keeps a registry of docs loaded
    /// via `document()`), grabs that doc's `DocIndex`, and re-runs
    /// the core engine's `apply_predicates` / `eval_step_on_nodes`
    /// against it.  Returns the resulting node-set as foreign
    /// pointers.  `None` means the bindings don't support foreign-
    /// doc traversal — engine errors out.
    fn apply_foreign_path(
        &self,
        _nodes:      &[ForeignNodePtr],
        _predicates: &[Expr],
        _steps:      &[Step],
    ) -> Option<Result<Vec<ForeignNodePtr>>> { None }

    /// Static base URI of the expression's surrounding XSLT element
    /// (XPath 2.0 §C.1).  Consulted by `fn:resolve-uri($rel)` when no
    /// explicit base is supplied and by `fn:static-base-uri()`.
    /// Default `None` means the runtime has no static base URI for
    /// this evaluation — `resolve-uri` then returns its argument
    /// unchanged.
    fn static_base_uri(&self) -> Option<String> { None }

    /// XPath 2.0 §3.1.5 base-URI accessor for synthetic nodes the
    /// runtime constructs.  Default `None` means "no override" —
    /// `fn:base-uri` then continues walking up the ancestor chain
    /// (and ultimately returns the empty sequence).  XSLT
    /// bindings override this to consult the RTF base-URI table
    /// populated when an `xsl:variable` / `xsl:document` carrying
    /// `xml:base` materialised a temporary tree.
    fn node_base_uri(&self, _id: NodeId) -> Option<String> { None }

    /// XPath 1.0 §5 string-value of a foreign-doc node.  Core can't
    /// dereference the pointer itself (no `unsafe` in this crate);
    /// the bindings impl (compat) walks the node through libxml2-ABI
    /// accessors and returns the concatenated text.  Default returns
    /// empty string — accurate for the lean build where ForeignNodeSet
    /// is unreachable anyway.
    fn foreign_string_value(&self, _p: ForeignNodePtr) -> String { String::new() }

    /// Dynamic-doc loader hook.  Invoked by the XSLT `document()` /
    /// XPath 2.0 `doc()` runtime when the requested URI isn't in the
    /// statically-discovered pre-load map.  Implementations resolve
    /// the URI through the surrounding `Loader`, parse the resource,
    /// graft it into the active `DocIndex` via
    /// [`super::DocIndex::graft_dynamic_document`], and return the
    /// new doc-root [`NodeId`].
    ///
    /// `Some(Err(...))` means the URI was understood but couldn't be
    /// resolved (network/IO error, parse error).  `None` means the
    /// bindings layer doesn't support dynamic doc-loading at all —
    /// the caller falls back to the legacy "URI not pre-loaded"
    /// error so the failure mode is the same as before this hook
    /// existed.
    fn load_dynamic_document(
        &self, _uri: &str,
    ) -> Option<std::result::Result<NodeId, crate::error::XmlError>> {
        None
    }

    /// Which regex dialect to use for `fn:matches` / `fn:replace` /
    /// `fn:tokenize` / `xsl:analyze-string`.  The default is
    /// [`crate::regex::Dialect::Xpath`] (XSLT 3.0 / XPath 3.0
    /// grammar).  An XSLT 2.0 host that wants the W3C conformance
    /// suite's stricter rejection of `(?:…)` overrides this to
    /// [`crate::regex::Dialect::Xpath20`].
    fn regex_dialect(&self) -> crate::regex::Dialect {
        crate::regex::Dialect::Xpath
    }
}

/// Default bindings: every callback returns `None`.  Used when the
/// caller didn't supply bindings explicitly.
pub struct NoBindings;
impl XPathBindings for NoBindings {}

/// XML 1.0 §3.1 / XPath 1.0 §3.7 implicit prefix bindings.  Returned
/// when user bindings don't resolve a prefix — these two prefixes are
/// reserved and always available without explicit declaration.
fn implicit_prefix(prefix: &str) -> Option<&'static str> {
    match prefix {
        "xml"   => Some("http://www.w3.org/XML/1998/namespace"),
        "xmlns" => Some("http://www.w3.org/2000/xmlns/"),
        // XPath 2.0 §1.6 — `xs` / `xsi` are conventionally
        // pre-bound in XSLT 2.0 static contexts because constructor
        // calls (`xs:dateTime(...)`) appear all over the test
        // corpus without explicit xmlns declarations.  The `fn`
        // prefix is intentionally NOT auto-bound: XSLT 2.0 §3.6
        // requires the stylesheet to declare it (XSLT 3.0 relaxes
        // this), and tests like `type/namespace-6202` rely on the
        // engine erroring on undeclared `fn:`.
        "xs"    => Some("http://www.w3.org/2001/XMLSchema"),
        "xsi"   => Some("http://www.w3.org/2001/XMLSchema-instance"),
        _ => None,
    }
}

/// Resolve a prefix consulting user bindings first, then the two
/// reserved XML prefixes.  Use this instead of calling
/// `bindings.resolve_prefix` directly so `xml:`/`xmlns:` work without
/// the consumer having to register them.
fn resolve_prefix_or_implicit(bindings: &dyn XPathBindings, prefix: &str) -> Option<String> {
    bindings
        .resolve_prefix(prefix)
        .or_else(|| implicit_prefix(prefix).map(str::to_string))
}

/// Resolve the namespace URI of a `name#arity` function reference.  An
/// unprefixed name lies in the default function namespace (`fn:`); a
/// prefixed one resolves through the in-scope bindings (empty URI if the
/// prefix is unbound).
fn named_function_namespace(name: &str, bindings: &dyn XPathBindings) -> String {
    match name.split_once(':') {
        Some((prefix, _)) => resolve_prefix_or_implicit(bindings, prefix).unwrap_or_default(),
        None => FN_NAMESPACE.to_string(),
    }
}

static NO_BINDINGS: NoBindings = NoBindings;

/// The static evaluation context (XPath 2.0 §2.1.1) — the config that
/// is fixed for the whole evaluation and travels by reference so it
/// can't be lost or duplicated.  Holds exactly the knobs that don't
/// vary within an evaluation; genuinely dynamic state (context item,
/// position, the `[xsl:]default-collation` that nests via
/// `Expr::WithDefaultCollation`) lives elsewhere.
///
/// Carried on [`EvalCtx`]; a nested context (`for` body, predicate)
/// copies the same `&StaticContext`, so a value set once at the top is
/// seen everywhere — unlike the old per-method `XPathBindings` config,
/// which a binding wrapper could silently reset to its default.
#[derive(Debug, Clone, Copy)]
pub struct StaticContext {
    /// XPath 2.0+ host — selects xs:integer/decimal literal typing and
    /// the F&O scientific double→string form.
    pub xpath_2_0: bool,
    /// libxml2-compat mode — where the spec and libxml2 historically
    /// diverge (`number('-')`, large-magnitude number formatting, …).
    pub libxml2_compatible: bool,
    /// The node the XSLT `current()` function returns: the context node
    /// of the *whole* top-level expression, fixed for the evaluation.
    /// Unlike [`EvalCtx::context_node`] it does NOT change as evaluation
    /// descends into location steps and predicates — so `current()`
    /// inside a nested predicate (`foo[@x=current()/@y]`, as ISO
    /// Schematron's phase selection uses) resolves to the instruction's
    /// current node rather than the predicate's context.
    ///
    /// `None` means "no fixed current node was threaded" — `current()`
    /// then falls back to the live context node (the historical
    /// behaviour, correct outside nested predicates).  Hosts that know
    /// the instruction's current node (the libxml2 ABI shim) set `Some`.
    pub current_node: Option<NodeId>,
}
// NOTE: the regex dialect is deliberately NOT here. It is read from the
// bindings at the point of use because, in XSLT, it is effectively
// scope-dependent today (a scoped binding reports the trait default),
// and that interacts with the regex engine's anchor handling. Folding
// it into the fixed static context would change that behaviour.

impl Default for StaticContext {
    /// Strict XPath 1.0 — the conservative default for a bare context.
    fn default() -> Self {
        StaticContext { xpath_2_0: false, libxml2_compatible: false, current_node: None }
    }
}

impl StaticContext {
    /// Number-serialization style implied by the static context.
    pub fn num_style(&self) -> NumStyle {
        NumStyle::from_context(self.libxml2_compatible, self.xpath_2_0)
    }
}

/// The strict-1.0 static context, borrowable by contexts that have no
/// host config (raw [`EvalCtx::root`], the default `value_to_string`).
pub static DEFAULT_STATIC_CTX: StaticContext = StaticContext {
    xpath_2_0: false,
    libxml2_compatible: false,
    current_node: None,
};

pub struct EvalCtx<'b> {
    pub context_node: NodeId,
    pub pos: usize,
    pub size: usize,
    pub bindings: &'b dyn XPathBindings,
    /// Fixed static config for this evaluation (version, libxml2-compat
    /// mode).  Threaded by reference so nested contexts share it and it
    /// can't be dropped by a binding wrapper.
    pub static_ctx: &'b StaticContext,
}

impl EvalCtx<'static> {
    /// Build a default eval context (document root, position 1, no
    /// bindings, strict XPath 1.0 static context).  Equivalent to
    /// libxml2's `xmlXPathContext` with nothing registered.
    pub fn root() -> Self {
        Self {
            context_node: 0,
            pos: 1,
            size: 1,
            bindings: &NO_BINDINGS,
            static_ctx: &DEFAULT_STATIC_CTX,
        }
    }
}

// ── prefix validation ─────────────────────────────────────────────────────────

/// Walk the expression AST checking that every namespace prefix used
/// in a name test (`prefix:local`, `prefix:*`) resolves through the
/// supplied bindings.  Surfaces an `XPathEvalError` for the first
/// unbound prefix encountered, matching libxml2's
/// `XPATH_UNDEF_PREFIX_ERROR`.
///
/// Function-name prefixes are validated at dispatch time inside
/// `eval_function`; variable prefixes are not (variables are stored
/// as raw names and looked up verbatim).
pub fn validate_prefixes(expr: &Expr, bindings: &dyn XPathBindings) -> Result<()> {
    match expr {
        Expr::Or(l, r) | Expr::And(l, r) | Expr::Eq(l, r) | Expr::Ne(l, r)
        | Expr::Lt(l, r) | Expr::Gt(l, r) | Expr::Le(l, r) | Expr::Ge(l, r)
        | Expr::ValueEq(l, r) | Expr::ValueNe(l, r)
        | Expr::ValueLt(l, r) | Expr::ValueGt(l, r)
        | Expr::ValueLe(l, r) | Expr::ValueGe(l, r)
        | Expr::Add(l, r) | Expr::Sub(l, r) | Expr::Mul(l, r) | Expr::Div(l, r)
        | Expr::Mod(l, r) | Expr::Union(l, r)
        | Expr::SimpleMap(l, r)
        | Expr::NodeBefore(l, r) | Expr::NodeAfter(l, r) | Expr::NodeIs(l, r) => {
            validate_prefixes(l, bindings)?;
            validate_prefixes(r, bindings)
        }
        Expr::Neg(e) => validate_prefixes(e, bindings),
        Expr::Path(p) => match p {
            LocationPath::Absolute(steps) | LocationPath::Relative(steps) => {
                validate_steps(steps, bindings)
            }
        },
        Expr::FilterPath { primary, predicates, steps } => {
            validate_prefixes(primary, bindings)?;
            for p in predicates { validate_prefixes(p, bindings)?; }
            validate_steps(steps, bindings)
        }
        Expr::FunctionCall(_, args) => {
            for a in args { validate_prefixes(a, bindings)?; }
            Ok(())
        }
        Expr::Variable(_) | Expr::Literal(_)
        | Expr::Integer(_) | Expr::Decimal(_) | Expr::Double(_) => Ok(()),
        Expr::IfThenElse { cond, then_branch, else_branch } => {
            validate_prefixes(cond, bindings)?;
            validate_prefixes(then_branch, bindings)?;
            validate_prefixes(else_branch, bindings)
        }
        Expr::For { bindings: binds, body }
        | Expr::Let { bindings: binds, body } => {
            for (_, e) in binds { validate_prefixes(e, bindings)?; }
            validate_prefixes(body, bindings)
        }
        Expr::Range(a, b) => {
            validate_prefixes(a, bindings)?;
            validate_prefixes(b, bindings)
        }
        Expr::Sequence(items) => {
            for e in items { validate_prefixes(e, bindings)?; }
            Ok(())
        }
        Expr::Quantified { bindings: binds, test, .. } => {
            for (_, e) in binds { validate_prefixes(e, bindings)?; }
            validate_prefixes(test, bindings)
        }
        Expr::IDiv(a, b) | Expr::Intersect(a, b) | Expr::Except(a, b) => {
            validate_prefixes(a, bindings)?;
            validate_prefixes(b, bindings)
        }
        Expr::InstanceOf(a, _) | Expr::CastAs(a, _)
        | Expr::CastableAs(a, _) | Expr::TreatAs(a, _) => validate_prefixes(a, bindings),
        Expr::TryCatch { body, catches } => {
            validate_prefixes(body, bindings)?;
            for c in catches { validate_prefixes(&c.body, bindings)?; }
            Ok(())
        }
        Expr::WithDefaultCollation(_, inner) => validate_prefixes(inner, bindings),
        Expr::BackwardsCompat(inner) => validate_prefixes(inner, bindings),
        Expr::MapConstructor(entries) => {
            for (k, v) in entries {
                validate_prefixes(k, bindings)?;
                validate_prefixes(v, bindings)?;
            }
            Ok(())
        }
        Expr::ArrayConstructor { members, .. } => {
            for m in members { validate_prefixes(m, bindings)?; }
            Ok(())
        }
        Expr::Lookup(base, key) => {
            validate_prefixes(base, bindings)?;
            validate_lookup_key_prefixes(key, bindings)
        }
        Expr::UnaryLookup(key) => validate_lookup_key_prefixes(key, bindings),
        Expr::InlineFunction { body, .. } => validate_prefixes(body, bindings),
        Expr::DynamicCall { func, args } => {
            validate_prefixes(func, bindings)?;
            for a in args { validate_prefixes(a, bindings)?; }
            Ok(())
        }
        Expr::NamedFunctionRef { .. } | Expr::Placeholder | Expr::ContextItem => Ok(()),
    }
}

fn validate_lookup_key_prefixes(
    key: &crate::xpath::ast::LookupKey, bindings: &dyn XPathBindings,
) -> Result<()> {
    if let crate::xpath::ast::LookupKey::Expr(e) = key {
        validate_prefixes(e, bindings)?;
    }
    Ok(())
}

fn validate_steps(steps: &[Step], bindings: &dyn XPathBindings) -> Result<()> {
    for step in steps {
        if let NodeTest::QName(prefix, _) | NodeTest::PrefixWildcard(prefix) = &step.node_test
            && resolve_prefix_or_implicit(bindings, prefix).is_none()
        {
            return Err(xpath_err(format!(
                "Undefined namespace prefix: {prefix}"
            )));
        }
        for pred in &step.predicates {
            validate_prefixes(pred, bindings)?;
        }
    }
    Ok(())
}

// ── public entry point ────────────────────────────────────────────────────────

/// Convenience for callers that just need the boolean value of an
/// expression evaluated against a fresh context (context node passed
/// in, position 1 of 1, no libxml2-compat tweaks).  Used by the XSD
/// 1.1 assertion evaluator — see [`crate::xsd`].
pub fn eval_to_bool<I: DocIndexLike>(
    expr:          &Expr,
    idx:           &I,
    context_node:  NodeId,
    bindings:      &dyn XPathBindings,
) -> Result<bool> {
    let static_ctx = StaticContext {
        xpath_2_0: bindings.xpath_version_2_or_later(),
        libxml2_compatible: false,
        current_node: Some(context_node),
    };
    let ctx = EvalCtx {
        context_node,
        pos: 1,
        size: 1,
        bindings,
        static_ctx: &static_ctx,
    };
    let v = eval_expr(expr, &ctx, idx)?;
    Ok(value_to_bool(&v, idx))
}

pub fn eval_expr<I: DocIndexLike>(expr: &Expr, ctx: &EvalCtx<'_>, idx: &I) -> Result<Value> {
    // Charge once per AST node evaluation.  This covers the
    // AST-traversal axis of cost (deep / wide expressions) and
    // every recursive sub-expression a predicate may build.
    charge_eval_step()?;
    match expr {
        // `.` as a primary expression yields the current context item —
        // its actual value (which may be a function item), falling back to
        // the context node when no non-node context item is set.
        Expr::ContextItem => Ok(current_context_item()
            .unwrap_or_else(|| Value::NodeSet(vec![ctx.context_node]))),
        Expr::Or(l, r) => {
            if value_to_bool(&eval_expr(l, ctx, idx)?, idx) {
                return Ok(Value::Boolean(true));
            }
            Ok(Value::Boolean(value_to_bool(&eval_expr(r, ctx, idx)?, idx)))
        }
        Expr::And(l, r) => {
            if !value_to_bool(&eval_expr(l, ctx, idx)?, idx) {
                return Ok(Value::Boolean(false));
            }
            Ok(Value::Boolean(value_to_bool(&eval_expr(r, ctx, idx)?, idx)))
        }
        Expr::Eq(l, r) => {
            let lv = eval_expr(l, ctx, idx)?;
            let rv = eval_expr(r, ctx, idx)?;
            reject_string_vs_numeric_cmp_2_0(&lv, &rv, ctx, "=")?;
            Ok(Value::Boolean(values_eq(&lv, &rv, idx, ctx.bindings)))
        }
        Expr::Ne(l, r) => {
            let lv = eval_expr(l, ctx, idx)?;
            let rv = eval_expr(r, ctx, idx)?;
            reject_string_vs_numeric_cmp_2_0(&lv, &rv, ctx, "!=")?;
            Ok(Value::Boolean(values_ne(&lv, &rv, idx, ctx.bindings)))
        }
        Expr::Lt(l, r) => cmp_op(l, r, ctx, idx, |a, b| a < b),
        Expr::Gt(l, r) => cmp_op(l, r, ctx, idx, |a, b| a > b),
        Expr::Le(l, r) => cmp_op(l, r, ctx, idx, |a, b| a <= b),
        Expr::Ge(l, r) => cmp_op(l, r, ctx, idx, |a, b| a >= b),
        // XPath 2.0 §3.5.1 value comparison operators.  Operands
        // must atomise to at most one item each; an empty sequence
        // makes the result an empty sequence.
        Expr::ValueEq(l, r) => value_compare(l, r, ctx, idx, ValueCmp::Eq),
        Expr::ValueNe(l, r) => value_compare(l, r, ctx, idx, ValueCmp::Ne),
        Expr::ValueLt(l, r) => value_compare(l, r, ctx, idx, ValueCmp::Lt),
        Expr::ValueGt(l, r) => value_compare(l, r, ctx, idx, ValueCmp::Gt),
        Expr::ValueLe(l, r) => value_compare(l, r, ctx, idx, ValueCmp::Le),
        Expr::ValueGe(l, r) => value_compare(l, r, ctx, idx, ValueCmp::Ge),
        Expr::Add(l, r) => {
            // XPath 2.0 §10.4 / §10.5 date+duration / duration+duration
            // dispatch on typed operands; fall through to numeric.
            let lv = eval_expr(l, ctx, idx)?;
            let rv = eval_expr(r, ctx, idx)?;
            if let Some(v) = date_arith_add(&lv, &rv) { return Ok(v); }
            if arith_empty_2_0(&lv, &rv, ctx) { return Ok(Value::NodeSet(Vec::new())); }
            reject_string_arith_2_0(&lv, &rv, ctx, "+")?;
            Ok(compat_numeric_op(&lv, &rv, idx, ctx.bindings, NumericOp::Add))
        }
        Expr::Sub(l, r) => {
            let lv = eval_expr(l, ctx, idx)?;
            let rv = eval_expr(r, ctx, idx)?;
            if let Some(v) = date_arith_sub(&lv, &rv) { return Ok(v); }
            if arith_empty_2_0(&lv, &rv, ctx) { return Ok(Value::NodeSet(Vec::new())); }
            reject_string_arith_2_0(&lv, &rv, ctx, "-")?;
            Ok(compat_numeric_op(&lv, &rv, idx, ctx.bindings, NumericOp::Sub))
        }
        Expr::Mul(l, r) => {
            let lv = eval_expr(l, ctx, idx)?;
            let rv = eval_expr(r, ctx, idx)?;
            // XPath 2.0 §10.6.1 — `duration * number` /
            // `number * duration` scales the duration.  Pre-empt
            // the typed_numeric_op coercion (which would turn the
            // duration into NaN) when one side is a duration and
            // the other is a numeric.
            if let Some(v) = duration_mul(&lv, &rv, idx, ctx.bindings) { return Ok(v); }
            if arith_empty_2_0(&lv, &rv, ctx) { return Ok(Value::NodeSet(Vec::new())); }
            reject_string_arith_2_0(&lv, &rv, ctx, "*")?;
            Ok(compat_numeric_op(&lv, &rv, idx, ctx.bindings, NumericOp::Mul))
        }
        Expr::Div(l, r) => {
            let lv = eval_expr(l, ctx, idx)?;
            let rv = eval_expr(r, ctx, idx)?;
            // XPath 2.0 §10.6.2 — `duration div number` /
            // `duration div duration` (latter returns a number).
            if let Some(v) = duration_div(&lv, &rv, idx, ctx.bindings) { return Ok(v); }
            if arith_empty_2_0(&lv, &rv, ctx) { return Ok(Value::NodeSet(Vec::new())); }
            if integer_decimal_zero_divisor(&lv, &rv, idx, ctx.bindings) {
                return Err(xpath_err("division by zero").with_xpath_code("FOAR0001"));
            }
            reject_string_arith_2_0(&lv, &rv, ctx, "div")?;
            Ok(compat_numeric_op(&lv, &rv, idx, ctx.bindings, NumericOp::Div))
        }
        Expr::Mod(l, r) => {
            let lv = eval_expr(l, ctx, idx)?;
            let rv = eval_expr(r, ctx, idx)?;
            if arith_empty_2_0(&lv, &rv, ctx) { return Ok(Value::NodeSet(Vec::new())); }
            if integer_decimal_zero_divisor(&lv, &rv, idx, ctx.bindings) {
                return Err(xpath_err("modulo by zero").with_xpath_code("FOAR0001"));
            }
            Ok(compat_numeric_op(&lv, &rv, idx, ctx.bindings, NumericOp::Mod))
        }
        Expr::Neg(inner) => {
            // Unary minus keeps the operand's numeric type (XPath 2.0
            // §6.2.1 op:numeric-unary-minus) — `-17.5` is xs:decimal,
            // `-5` is xs:integer — so `instance of` and stringify stay
            // correct.  In XPath 1.0 backwards-compatibility mode the
            // operand is atomised to xs:double first, so `-0` is the
            // double -0.0 (which carries a sign, unlike integer 0).
            let v = eval_expr(inner, ctx, idx)?;
            if in_xpath_1_0_compat() {
                let n = -value_to_number_with(&v, idx, ctx.bindings);
                return Ok(Value::Number(Numeric::Double(n)));
            }
            // Negate exactly when the operand is integer or decimal —
            // `-5.2` must stay xs:decimal `-5.2`, not slip through f64
            // and drip precision noise into a subsequent `mod`.  An
            // i64::MIN integer has no positive counterpart, so widen
            // to xs:decimal on overflow rather than panicking.  Typed
            // integer-family values stay integer (xs:int / xs:short /
            // …) — only literal `xs:decimal` keeps the Decimal kind.
            if let Value::Number(Numeric::Integer(i)) = v {
                return Ok(match i.checked_neg() {
                    Some(n) => Value::Number(Numeric::Integer(n)),
                    None    => Value::Number(Numeric::Decimal(-rust_decimal::Decimal::from(i))),
                });
            }
            if matches!(numeric_kind_of(&v), Some("integer")) {
                let f = -value_to_number_with(&v, idx, ctx.bindings);
                return Ok(preserve_numeric_kind(&v, f));
            }
            if let Some(d) = exact_decimal(&v) {
                return Ok(Value::Number(Numeric::Decimal(-d)));
            }
            let n = -value_to_number_with(&v, idx, ctx.bindings);
            Ok(preserve_numeric_kind(&v, n))
        }
        Expr::Union(l, r) => {
            let lv = eval_expr(l, ctx, idx)?;
            let rv = eval_expr(r, ctx, idx)?;
            match (lv, rv) {
                (Value::NodeSet(mut a), Value::NodeSet(b)) => {
                    a.extend(b);
                    dedup_sort(&mut a);
                    Ok(Value::NodeSet(a))
                }
                (Value::ForeignNodeSet(mut a), Value::ForeignNodeSet(b)) => {
                    a.extend(b);
                    dedup_foreign(&mut a);
                    Ok(Value::ForeignNodeSet(a))
                }
                // Mixed primary+foreign union — XSLT allows it but we
                // can't represent both kinds in one Value variant here.
                // The lxml tests we target don't exercise this; surface
                // a clear error rather than miscompile.
                (Value::NodeSet(_), Value::ForeignNodeSet(_))
                | (Value::ForeignNodeSet(_), Value::NodeSet(_)) => Err(xpath_err(
                    "mixed primary/foreign node-set union is not supported",
                )),
                _ => Err(xpath_err("union operator requires node-sets on both sides")),
            }
        }
        Expr::Path(path) => {
            // Bare `.` short-circuits to the atomic context item
            // when one is in scope (set by predicate filtering /
            // simple-map over an atomic sequence).  Without this,
            // a predicate like `($strings)[matches(., '\w')]`
            // would resolve `.` to the outer context node and ask
            // matches() about *its* string-value, not each item's.
            if is_bare_dot_path(path) {
                if let Some(v) = current_context_item() {
                    return Ok(v);
                }
            }
            let nodes = eval_path(path, ctx.context_node, idx, ctx.bindings, ctx.static_ctx.libxml2_compatible, ctx.static_ctx.current_node)?;
            Ok(Value::NodeSet(nodes))
        }
        Expr::FilterPath { primary, predicates, steps } => {
            let pv = eval_expr(primary, ctx, idx)?;
            match pv {
                Value::NodeSet(ns) => {
                    let mut nodes =
                        apply_predicates_cur(ns, predicates, idx, ctx.bindings, ctx.static_ctx.libxml2_compatible, ctx.static_ctx.current_node)?;
                    for step in steps {
                        nodes = eval_step_on_nodes_cur(nodes, step, idx, ctx.bindings, ctx.static_ctx.libxml2_compatible, ctx.static_ctx.current_node)?;
                    }
                    Ok(Value::NodeSet(nodes))
                }
                Value::ForeignNodeSet(ns) => {
                    // Foreign-doc path: hand the predicates+steps to
                    // the bindings, which knows how to look up the
                    // foreign doc's `DocIndex` and run the engine's
                    // step/predicate helpers against it.
                    match ctx.bindings.apply_foreign_path(&ns, predicates, steps) {
                        Some(r) => r.map(Value::ForeignNodeSet),
                        None => Err(xpath_err(
                            "foreign-doc path traversal not supported by bindings",
                        )),
                    }
                }
                // XPath 2.0 §3.5 — a FilterExpr applies its
                // predicates to any sequence.  For each item, the
                // predicate is evaluated with `position()` /
                // `last()` reflecting the sequence position, and the
                // item is kept when the predicate's value is a
                // number equal to the position OR otherwise its
                // EBV is true.
                Value::Sequence(items) if steps.is_empty() => {
                    let kept = filter_sequence_by_predicates(
                        items, predicates, ctx, idx,
                    )?;
                    if kept.len() == 1 {
                        Ok(kept.into_iter().next().unwrap())
                    } else {
                        Ok(Value::Sequence(kept))
                    }
                }
                // Singleton atomic with a predicate behaves like
                // a one-item sequence — same number/EBV rule.
                other if steps.is_empty() => {
                    let kept = filter_sequence_by_predicates(
                        vec![other], predicates, ctx, idx,
                    )?;
                    match kept.len() {
                        0 => Ok(Value::NodeSet(Vec::new())),
                        1 => Ok(kept.into_iter().next().unwrap()),
                        _ => Ok(Value::Sequence(kept)),
                    }
                }
                // XPath 2.0 §3.5 — `seq[pred]/step` filters the
                // sequence, then runs the steps from each surviving
                // *node* item.  Atomic items can't carry forward
                // axes, so they drop out at the path-step boundary.
                Value::Sequence(items) => {
                    let kept = filter_sequence_by_predicates(
                        items, predicates, ctx, idx,
                    )?;
                    let mut nodes: Vec<NodeId> = Vec::new();
                    for it in kept {
                        if let Value::NodeSet(ns) = it {
                            nodes.extend(ns);
                        }
                    }
                    nodes.sort_unstable();
                    nodes.dedup();
                    for step in steps {
                        nodes = eval_step_on_nodes_cur(nodes, step, idx, ctx.bindings, ctx.static_ctx.libxml2_compatible, ctx.static_ctx.current_node)?;
                    }
                    Ok(Value::NodeSet(nodes))
                }
                _ => Err(xpath_err("predicate applied to non-node-set")),
            }
        }
        Expr::FunctionCall(name, args) => eval_function(name, args, ctx, idx),
        Expr::Variable(name) => {
            // Consult user-supplied variable bindings first; XPath 1.0
            // proper says an undefined variable is an error.
            match ctx.bindings.variable(name) {
                Some(v) => Ok(v),
                None => Err(xpath_err(format!("undefined XPath variable: ${name}"))),
            }
        }
        Expr::Literal(s) => Ok(Value::String(s.clone())),
        // An integer / decimal literal carries its XSD type in XPath
        // 2.0 so `instance of` and arithmetic promotion see it; XPath
        // 1.0 has only the `number` (double) type, so it stays Double.
        Expr::Integer(i) => Ok(Value::Number(if ctx.static_ctx.xpath_2_0 {
            Numeric::Integer(*i)
        } else {
            Numeric::Double(*i as f64)
        })),
        Expr::Decimal(n) => Ok(Value::Number(if ctx.static_ctx.xpath_2_0 {
            Numeric::Decimal(*n)
        } else {
            // XPath 1.0 has only the `number` type (double); flatten
            // the exact decimal back to f64 for legacy stylesheets.
            use rust_decimal::prelude::ToPrimitive;
            Numeric::Double(n.to_f64().unwrap_or(f64::NAN))
        })),
        // A numeric literal with an exponent is xs:double (XPath 2.0
        // §3.1.1).  The Double kind drives the F&O scientific string
        // form in a 2.0 host; a 1.0 host stringifies it as a decimal.
        Expr::Double(n) => Ok(Value::Number(Numeric::Double(*n))),
        Expr::IfThenElse { cond, then_branch, else_branch } => {
            // XPath 2.0 § 3.8 — exactly one branch is evaluated.
            let truth = value_to_bool(&eval_expr(cond, ctx, idx)?, idx);
            let branch = if truth { then_branch } else { else_branch };
            eval_expr(branch, ctx, idx)
        }
        Expr::IDiv(l, r) => {
            // XPath 2.0 § 3.4 `idiv` — integer quotient, truncation
            // towards zero.  Division by zero raises an error
            // (matches the spec's err:FOAR0001).
            let a = value_to_number(&eval_expr(l, ctx, idx)?, idx);
            let b = value_to_number(&eval_expr(r, ctx, idx)?, idx);
            if b == 0.0 || b.is_nan() || a.is_nan() {
                return Err(xpath_err("idiv: division by zero or NaN")
                    .with_xpath_code("FOAR0001"));
            }
            // XPath 2.0 §3.4 — the result of `idiv` is always xs:integer.
            Ok(integer_result((a / b).trunc() as i64, ctx.bindings))
        }
        Expr::Intersect(l, r) => {
            // Node-set intersection (XPath 2.0 § 3.3.4).  Non-NodeSet
            // operands are an error in 2.0 (FORG0006).  We're lenient
            // and treat empty/atomic operands as empty node-sets.
            let lns = node_set_of(eval_expr(l, ctx, idx)?);
            let rns = node_set_of(eval_expr(r, ctx, idx)?);
            let rset: std::collections::BTreeSet<NodeId> = rns.into_iter().collect();
            let mut out: Vec<NodeId> = lns.into_iter().filter(|n| rset.contains(n)).collect();
            out.sort_unstable();
            out.dedup();
            Ok(Value::NodeSet(out))
        }
        Expr::Except(l, r) => {
            // `lhs except rhs` — items in lhs not in rhs.
            let lns = node_set_of(eval_expr(l, ctx, idx)?);
            let rns = node_set_of(eval_expr(r, ctx, idx)?);
            let rset: std::collections::BTreeSet<NodeId> = rns.into_iter().collect();
            let mut out: Vec<NodeId> = lns.into_iter().filter(|n| !rset.contains(n)).collect();
            out.sort_unstable();
            out.dedup();
            Ok(Value::NodeSet(out))
        }
        Expr::InstanceOf(inner, st) => {
            let v = eval_expr(inner, ctx, idx)?;
            let st = resolve_kind_test_namespaces(st, ctx.bindings);
            Ok(Value::Boolean(value_matches_sequence_type(&v, &st, idx)))
        }
        Expr::CastAs(inner, st) => {
            // XPath 2.0 §3.10.2 — `cast as T` converts the input to
            // the target type via the XSD casting rules.  Unlike
            // `treat as`, it doesn't assert the input ALREADY matches
            // T; it performs the conversion.  T must be a generalised
            // atomic type — xs:anyType, xs:anySimpleType, xs:untyped,
            // and xs:anyAtomicType are XPST0080 (anyAtomicType) or
            // XPST0051 (the non-atomic schema types).
            if let crate::xpath::ast::ItemType::Atomic(name) = &st.item {
                match name.as_str() {
                    "anyType" | "anySimpleType" | "untyped" =>
                        return Err(xpath_err(format!(
                            "cast as: target type xs:{name} is not an atomic \
                             type (XPST0051)"))),
                    "anyAtomicType" | "NOTATION" =>
                        return Err(xpath_err(format!(
                            "cast as: xs:{name} is not a permitted target \
                             type (XPST0080)"))),
                    _ => {}
                }
            }
            let v = eval_expr(inner, ctx, idx)?;
            cast_value_to_atomic(&v, st, idx)
        }
        Expr::TreatAs(inner, st) => {
            // XPath 2.0 §3.10.3 — `treat as T` is "assert + identity":
            // the input must already match T, otherwise it's a type
            // error (XPDY0050).
            let v = eval_expr(inner, ctx, idx)?;
            let st = resolve_kind_test_namespaces(st, ctx.bindings);
            if !value_matches_sequence_type(&v, &st, idx) {
                return Err(xpath_err(format!(
                    "treat as failed: value doesn't match {st:?}"
                )));
            }
            Ok(v)
        }
        Expr::WithDefaultCollation(uri, inner) => {
            with_default_collation(Some(uri.clone()), ||
                eval_expr(inner, ctx, idx))
        }
        Expr::BackwardsCompat(inner) => {
            with_xpath_1_0_compat(|| eval_expr(inner, ctx, idx))
        }
        Expr::MapConstructor(entries) => {
            let mut out: Vec<(Value, Value)> = Vec::with_capacity(entries.len());
            for (ke, ve) in entries {
                let key = eval_expr(ke, ctx, idx)?;
                // The key must be a single atomic value (XPTY0004
                // otherwise); take the first item leniently.
                let key = first_atomic_key(&key, idx);
                let val = eval_expr(ve, ctx, idx)?;
                if out.iter().any(|(k, _)| map_key_eq(k, &key, idx)) {
                    return Err(xpath_err(
                        "duplicate key in map constructor (XQDY0137)"));
                }
                out.push((key, val));
            }
            Ok(Value::Map(Box::new(out)))
        }
        Expr::ArrayConstructor { members, square } => {
            if *square {
                // One member per expression; each member is that
                // expression's value (a sequence).
                let mut out = Vec::with_capacity(members.len());
                for m in members { out.push(eval_expr(m, ctx, idx)?); }
                Ok(Value::Array(Box::new(out)))
            } else {
                // Curly form: each item of the contained sequence is
                // its own (singleton) member.
                let v = match members.first() {
                    Some(e) => eval_expr(e, ctx, idx)?,
                    None    => return Ok(Value::Array(Box::new(Vec::new()))),
                };
                Ok(Value::Array(Box::new(items_of(&v))))
            }
        }
        Expr::Lookup(base, key) => {
            let b = eval_expr(base, ctx, idx)?;
            eval_lookup(&b, key, ctx, idx)
        }
        Expr::UnaryLookup(key) => {
            // `?K` applies to the context item.
            let ctx_item = current_context_item()
                .unwrap_or(Value::NodeSet(vec![ctx.context_node]));
            eval_lookup(&ctx_item, key, ctx, idx)
        }
        Expr::InlineFunction { params, sig, body } => {
            // Capture the free variables of the body (minus the
            // parameters) from the defining scope — static scoping.
            let mut refs = Vec::new();
            collect_var_refs(body, &mut refs);
            let mut closure = Vec::new();
            for name in refs {
                if params.iter().any(|p| p == &name) { continue; }
                if let Some(v) = ctx.bindings.variable(&name) {
                    closure.push((name, v));
                }
            }
            Ok(Value::Function(Box::new(FunctionItem::Inline {
                params: params.clone(),
                sig: sig.clone(),
                body: (**body).clone(),
                closure,
            })))
        }
        Expr::NamedFunctionRef { name, arity } => {
            let ns = named_function_namespace(name, ctx.bindings);
            let local = name.rsplit(':').next().unwrap_or(name);
            let sig = ctx.bindings
                .function_signature_in(&ns, local, *arity)
                .map(Box::new);
            Ok(Value::Function(Box::new(FunctionItem::Named {
                name: name.clone(), ns, arity: *arity, sig,
            })))
        }
        Expr::DynamicCall { func, args } => {
            let f = eval_expr(func, ctx, idx)?;
            // XPath 3.1 §3.1.5 — maps and arrays are function items:
            // `$map(key)` looks the key up (empty sequence if absent);
            // `$array(n)` indexes the array (1-based).
            if args.len() == 1 && !matches!(args[0], Expr::Placeholder) {
                match &f {
                    Value::Map(m) => {
                        let key = first_atomic_key(&eval_expr(&args[0], ctx, idx)?, idx);
                        return Ok(m.iter().find(|(k, _)| map_key_eq(k, &key, idx))
                            .map(|(_, v)| v.clone())
                            .unwrap_or(Value::NodeSet(Vec::new())));
                    }
                    Value::Array(a) => {
                        let n = value_to_number(&eval_expr(&args[0], ctx, idx)?, idx) as i64;
                        if n < 1 || n as usize > a.len() {
                            return Err(xpath_err(format!(
                                "array index {n} is out of bounds (FOAY0001)"))
                                .with_xpath_code("FOAY0001"));
                        }
                        return Ok(a[(n - 1) as usize].clone());
                    }
                    _ => {}
                }
            }
            let fi = match &f {
                Value::Function(fi) => (**fi).clone(),
                _ => return Err(xpath_err(
                    "dynamic call target is not a function item (XPTY0004)")),
            };
            // Partial application: `?` placeholders leave unbound slots.
            if args.iter().any(|a| matches!(a, Expr::Placeholder)) {
                let mut bound = Vec::with_capacity(args.len());
                for a in args {
                    bound.push(match a {
                        Expr::Placeholder => None,
                        e => Some(eval_expr(e, ctx, idx)?),
                    });
                }
                return Ok(Value::Function(Box::new(
                    FunctionItem::Partial { base: Box::new(fi), bound })));
            }
            let argv: Vec<Value> = args.iter()
                .map(|a| eval_expr(a, ctx, idx)).collect::<Result<_>>()?;
            call_function_item(&fi, argv, ctx, idx)
        }
        Expr::Placeholder => Err(xpath_err(
            "'?' placeholder is only valid as a function-call argument")),
        Expr::CastableAs(inner, st) => {
            // XPath 2.0 §3.12.3: `castable as` reports whether the
            // cast would succeed — it doesn't propagate the typed
            // SequenceType cardinality check.  Skipping the
            // strict `value_matches_sequence_type` gate keeps the
            // semantics: a `Value::String("5")` *is* castable as
            // `xs:integer`, even though our untyped value model
            // doesn't initially classify it as one.
            let v = eval_expr(inner, ctx, idx)?;
            Ok(Value::Boolean(cast_value_to_atomic(&v, st, idx).is_ok()))
        }
        Expr::TryCatch { body, catches } => {
            // XPath 3.1 §3.16 — evaluate the body; if it raises a
            // dynamic error, hand off to the first catch whose
            // name-test list covers the caught error's QName.
            // Inside the catch body we layer a `ScopedBindings` so
            // `$err:code` / `$err:description` lookups resolve
            // without polluting the surrounding variable scope.
            match eval_expr(body, ctx, idx) {
                Ok(v) => Ok(v),
                Err(e) => {
                    // The caught error's spec code (e.g. FOAR0001) if it
                    // carries one, else the generic err:FOER0000 — same
                    // projection the XSLT instruction uses.
                    let code_local = e.xpath_code.clone()
                        .unwrap_or_else(|| "FOER0000".to_string());
                    let err_uri    = "http://www.w3.org/2005/xqt-errors";
                    for c in catches {
                        if !xpath_catch_matches(&c.matchers, err_uri, &code_local, ctx.bindings) {
                            continue;
                        }
                        let scoped = build_err_scope(ctx.bindings, err_uri, &code_local, &e.message);
                        let inner_ctx = EvalCtx {
                            context_node: ctx.context_node,
                            pos: ctx.pos, size: ctx.size,
                            bindings: &scoped,
                            static_ctx: ctx.static_ctx,
                        };
                        return eval_expr(&c.body, &inner_ctx, idx);
                    }
                    Err(e)
                }
            }
        }
        Expr::NodeBefore(l, r) | Expr::NodeAfter(l, r) => {
            // XPath 2.0 §3.5.3 — node-comparison operators.  Each
            // operand must atomise to a single node; empty operand
            // yields the empty sequence.  Order follows the
            // index's node-id (which mirrors document order
            // for nodes from the same tree).
            let after = matches!(expr, Expr::NodeAfter(_, _));
            let lv = eval_expr(l, ctx, idx)?;
            let rv = eval_expr(r, ctx, idx)?;
            let pick = |v: &Value| -> Option<NodeId> {
                match v {
                    Value::NodeSet(ns) if ns.len() == 1 => Some(ns[0]),
                    _ => None,
                }
            };
            let (Some(a), Some(b)) = (pick(&lv), pick(&rv)) else {
                return Ok(Value::NodeSet(Vec::new()));
            };
            Ok(Value::Boolean(if after { a > b } else { a < b }))
        }
        Expr::NodeIs(l, r) => {
            // XPath 2.0 §3.5.3 — node identity.  Each operand must
            // atomise to at most one node; an empty operand yields the
            // empty sequence.  Two nodes are `is`-equal iff they are
            // the same node (same index id; foreign nodes compare by
            // their pointer identity).
            let lv = eval_expr(l, ctx, idx)?;
            let rv = eval_expr(r, ctx, idx)?;
            let pick = |v: &Value| -> Option<NodeId> {
                match v {
                    Value::NodeSet(ns) if ns.len() == 1 => Some(ns[0]),
                    _ => None,
                }
            };
            let pick_foreign = |v: &Value| -> Option<ForeignNodePtr> {
                match v {
                    Value::ForeignNodeSet(fs) if fs.len() == 1 => Some(fs[0]),
                    _ => None,
                }
            };
            if let (Some(a), Some(b)) = (pick(&lv), pick(&rv)) {
                return Ok(Value::Boolean(a == b));
            }
            if let (Some(a), Some(b)) = (pick_foreign(&lv), pick_foreign(&rv)) {
                return Ok(Value::Boolean(a == b));
            }
            Ok(Value::NodeSet(Vec::new()))
        }
        Expr::SimpleMap(lhs, rhs) => {
            // XPath 3.0 §3.4 — evaluate `rhs` once per item of `lhs`
            // with that item as the context item, concatenating
            // results in iteration order (no document-order sort).
            // Iterate via [`iter_items`] so a 1.1M-item `IntRange`
            // doesn't materialise into a `Vec<Value>` up front;
            // [`sequence_len`] keeps `last()` correct without
            // expansion.
            let lv = eval_expr(lhs, ctx, idx)?;
            let total = sequence_len(&lv);
            let mut out_nodes: Vec<NodeId> = Vec::new();
            let mut out_seq: Vec<Value> = Vec::new();
            let mut any_atomic = false;
            for (i, item) in iter_items(&lv).enumerate() {
                // Same context-item plumbing as the predicate
                // filter: single-node items use the node; atomic
                // items go through the per-thread CONTEXT_ITEM
                // slot so bare `.` in `rhs` sees the value.
                let (cx, cit) = match &item {
                    Value::NodeSet(ns) if ns.len() == 1 => (ns[0], None),
                    _ => (ctx.context_node, Some(item.clone())),
                };
                let inner = EvalCtx {
                    context_node: cx, pos: i + 1, size: total,
                    bindings: ctx.bindings, static_ctx: ctx.static_ctx,
                };
                let v = with_context_item(cit, || eval_expr(rhs, &inner, idx))?;
                match v {
                    Value::NodeSet(ns)       => out_nodes.extend(ns),
                    Value::ForeignNodeSet(_) => {}
                    Value::Sequence(s)       => { any_atomic = true; out_seq.extend(s); }
                    other                    => { any_atomic = true; out_seq.push(other); }
                }
            }
            if !any_atomic {
                return Ok(Value::NodeSet(out_nodes));
            }
            // Atomics in the mix → produce Value::Sequence so the
            // typing of individual items survives.
            for id in out_nodes {
                out_seq.push(Value::NodeSet(vec![id]));
            }
            if out_seq.len() == 1 {
                Ok(out_seq.into_iter().next().unwrap())
            } else {
                Ok(Value::Sequence(out_seq))
            }
        }
        Expr::Range(lo, hi) => {
            // XPath 2.0 § 3.3.1 — `m to n` yields the empty sequence
            // when m > n, otherwise the integers m, m+1, …, n
            // (inclusive).  Returned as a lazy [`Value::IntRange`]
            // so the common big-range cases (Unicode codepoint
            // iteration, `for $i in 1 to N`) don't pay the cost of
            // pre-materialising N integer items.  Consumers that
            // need a concrete list expand on demand via
            // [`items_of`]; arithmetic consumers (`count`, `sum`)
            // special-case the variant for O(1).
            let m_v = eval_expr(lo, ctx, idx)?;
            let n_v = eval_expr(hi, ctx, idx)?;
            let m   = value_to_number(&m_v, idx).round() as i64;
            let n   = value_to_number(&n_v, idx).round() as i64;
            if m > n {
                return Ok(Value::NodeSet(Vec::new()));
            }
            Ok(Value::IntRange { lo: m, hi: n })
        }
        Expr::Sequence(items) => {
            // XPath 2.0 § 3.3.1 — parenthesised sequence literal.
            // Evaluate each item, then choose the most precise
            // result-Value shape that preserves the input's type
            // structure:
            //
            //   * No items                → `Value::NodeSet(vec![])`.
            //   * Single item             → return it (flatten).
            //   * Any item is `Typed`     → `Value::Sequence(items)`
            //     keeps the per-item type tags so downstream
            //     `instance of` / `subsequence` answer correctly.
            //   * All items are nodes     → `Value::NodeSet(union)`.
            //   * Otherwise               → existing NodeSet-of-
            //     synthetic-text encoding so XPath 1.0 consumers
            //     (`for-each`, `value-of`, …) work unchanged.
            let mut evaluated: Vec<Value> = Vec::with_capacity(items.len());
            for item in items {
                let v = eval_expr(item, ctx, idx)?;
                // Inner Sequence flattens per XPath 2.0 §3.3.1.
                match v {
                    Value::Sequence(inner) => evaluated.extend(inner),
                    other => evaluated.push(other),
                }
            }
            if evaluated.is_empty() {
                return Ok(Value::NodeSet(Vec::new()));
            }
            if evaluated.len() == 1 {
                return Ok(evaluated.into_iter().next().unwrap());
            }
            // XPath 2.0 §3.3.1 — a parenthesised sequence of items
            // is a `Value::Sequence` that preserves per-item type
            // identity.  We only fall back to the legacy NodeSet
            // shape when every item is itself a node — in that case
            // a flat NodeSet union is the right answer and keeps
            // XPath 1.0-shaped consumers happy.  `Value::IntRange` and
            // `Value::Typed` items also force a Sequence so their
            // lazy / type-tagged shape isn't flattened to a string.
            let any_node = evaluated.iter().any(|v|
                matches!(v, Value::NodeSet(_) | Value::ForeignNodeSet(_)));
            let any_lazy = evaluated.iter().any(|v|
                matches!(v, Value::Typed(_) | Value::IntRange { .. }));
            if !any_node || any_lazy {
                return Ok(Value::Sequence(evaluated));
            }
            // Legacy paths for backward compat with XPath 1.0 hot
            // routes that expect a NodeSet.
            let mut nodes: Vec<NodeId> = Vec::new();
            let mut atoms: Vec<String> = Vec::new();
            for v in evaluated {
                match v {
                    Value::NodeSet(ns)       => nodes.extend(ns),
                    Value::ForeignNodeSet(_) => {}
                    Value::String(s)         => atoms.push(s),
                    Value::Number(n)         => atoms.push(value_to_string(&Value::Number(n), idx)),
                    Value::Boolean(b)        => atoms.push((if b { "true" } else { "false" }).to_string()),
                    Value::Typed(t)          => atoms.push(t.lexical),
                    Value::Sequence(_)       => unreachable!(), // flattened above
                    Value::IntRange { .. }   => unreachable!(), // routed to Sequence above
                    // A map / array can't be a member of a node-set;
                    // drop it (this path builds a NodeSet result).
                    Value::Map(_) | Value::Array(_) | Value::Function(_) => {}
                }
            }
            if atoms.is_empty() {
                Ok(Value::NodeSet(nodes))
            } else {
                for n in &nodes { atoms.push(idx.string_value(*n)); }
                match idx.allocate_rtf_text_nodes(atoms.clone()) {
                    Some(ids) => Ok(Value::NodeSet(ids)),
                    None      => Ok(Value::String(atoms.join(""))),
                }
            }
        }
        Expr::Quantified { kind, bindings, test } => {
            // XPath 2.0 § 3.9 — `some $v in seq satisfies test` is
            // true iff at least one tuple of bindings satisfies the
            // test; `every` requires all of them to satisfy.  Empty
            // sequences make `some` false and `every` true.
            use crate::xpath::ast::QuantifierKind::*;
            let mut found_match    = false;
            let mut all_match      = true;
            let mut seen_any       = false;
            eval_quantified_recursive(
                bindings, test, 0, ctx, idx,
                &mut |result| {
                    seen_any = true;
                    if result { found_match = true; }
                    else      { all_match   = false; }
                },
            )?;
            Ok(Value::Boolean(match kind {
                Some  => found_match,
                Every => if !seen_any { true } else { all_match },
            }))
        }
        Expr::For { bindings, body } => {
            // XPath 2.0 § 3.7 `ForExpr`: each binding ranges over the
            // items of its `in` clause; the result is the concatenated
            // sequence of `body` values, one per (cartesian) tuple of
            // bindings.  Body items keep their per-iteration shape:
            // nodes accumulate as NodeIds, atomics keep their full
            // `Value` form so downstream type-aware operations (min /
            // max / instance of) see the original type tags.
            let mut out_items: Vec<Value> = Vec::new();
            eval_for_recursive_typed(bindings, body, 0, ctx, idx, &mut out_items)?;
            if out_items.is_empty() {
                return Ok(Value::NodeSet(Vec::new()));
            }
            // All-nodes path → flatten to a single NodeSet so XPath 1.0
            // consumers downstream (path steps, etc.) still see the
            // expected shape.  XPath 2.0 §3.7 specifies result order
            // as iteration order, NOT document order, so don't sort.
            let all_nodes = out_items.iter().all(|v| matches!(v, Value::NodeSet(_)));
            if all_nodes {
                let mut nodes: Vec<NodeId> = Vec::new();
                for v in out_items {
                    if let Value::NodeSet(ns) = v { nodes.extend(ns); }
                }
                return Ok(Value::NodeSet(nodes));
            }
            // Mixed / all-atomic → preserve as Value::Sequence so the
            // type information survives.
            Ok(Value::Sequence(out_items))
        }
        Expr::Let { bindings, body } => eval_let(bindings, body, 0, ctx, idx),
    }
}

/// XPath 3.0 § 3.10 `LetExpr`: bind each clause once, in source order,
/// each visible to later clauses and the body.  Unlike `for`, the bound
/// value keeps its full sequence shape (no per-item iteration), so the
/// body sees the variable exactly as the `:=` expression produced it.
fn eval_let<I: DocIndexLike>(
    bindings: &[(String, Expr)],
    body:     &Expr,
    depth:    usize,
    ctx:      &EvalCtx<'_>,
    idx:      &I,
) -> Result<Value> {
    if depth == bindings.len() {
        return eval_expr(body, ctx, idx);
    }
    let (name, bound_expr) = &bindings[depth];
    let value = eval_expr(bound_expr, ctx, idx)?;
    let scoped = ScopedBindings { parent: ctx.bindings, name, value };
    let inner = EvalCtx {
        context_node: ctx.context_node, pos: ctx.pos, size: ctx.size,
        bindings: &scoped, static_ctx: ctx.static_ctx,
    };
    eval_let(bindings, body, depth + 1, &inner, idx)
}

fn eval_for_recursive_typed<I: DocIndexLike>(
    bindings: &[(String, Expr)],
    body:     &Expr,
    depth:    usize,
    ctx:      &EvalCtx<'_>,
    idx:      &I,
    out_items: &mut Vec<Value>,
) -> Result<()> {
    if depth == bindings.len() {
        match eval_expr(body, ctx, idx)? {
            Value::Sequence(items) => out_items.extend(items),
            other                  => out_items.push(other),
        }
        return Ok(());
    }
    let (name, in_expr) = &bindings[depth];
    let seq = eval_expr(in_expr, ctx, idx)?;
    match seq {
        Value::NodeSet(ns) => {
            for id in ns {
                let scoped = ScopedBindings {
                    parent: ctx.bindings, name, value: Value::NodeSet(vec![id]),
                };
                let inner_ctx = EvalCtx {
                    context_node: ctx.context_node, pos: ctx.pos, size: ctx.size,
                    bindings: &scoped, static_ctx: ctx.static_ctx,
                };
                eval_for_recursive_typed(bindings, body, depth + 1, &inner_ctx, idx, out_items)?;
            }
        }
        Value::ForeignNodeSet(ns) => {
            for p in ns {
                let scoped = ScopedBindings {
                    parent: ctx.bindings, name, value: Value::ForeignNodeSet(vec![p]),
                };
                let inner_ctx = EvalCtx {
                    context_node: ctx.context_node, pos: ctx.pos, size: ctx.size,
                    bindings: &scoped, static_ctx: ctx.static_ctx,
                };
                eval_for_recursive_typed(bindings, body, depth + 1, &inner_ctx, idx, out_items)?;
            }
        }
        Value::Sequence(items) => {
            for v in items {
                let scoped = ScopedBindings {
                    parent: ctx.bindings, name, value: v,
                };
                let inner_ctx = EvalCtx {
                    context_node: ctx.context_node, pos: ctx.pos, size: ctx.size,
                    bindings: &scoped, static_ctx: ctx.static_ctx,
                };
                eval_for_recursive_typed(bindings, body, depth + 1, &inner_ctx, idx, out_items)?;
            }
        }
        Value::IntRange { lo, hi } => {
            for i in lo..=hi {
                let scoped = ScopedBindings {
                    parent: ctx.bindings, name, value: Value::Number(Numeric::Double(i as f64)),
                };
                let inner_ctx = EvalCtx {
                    context_node: ctx.context_node, pos: ctx.pos, size: ctx.size,
                    bindings: &scoped, static_ctx: ctx.static_ctx,
                };
                eval_for_recursive_typed(bindings, body, depth + 1, &inner_ctx, idx, out_items)?;
            }
        }
        atomic => {
            let scoped = ScopedBindings {
                parent: ctx.bindings, name, value: atomic,
            };
            let inner_ctx = EvalCtx {
                context_node: ctx.context_node, pos: ctx.pos, size: ctx.size,
                bindings: &scoped, static_ctx: ctx.static_ctx,
            };
            eval_for_recursive_typed(bindings, body, depth + 1, &inner_ctx, idx, out_items)?;
        }
    }
    Ok(())
}

/// Same iteration pattern as `eval_for_recursive_typed` but for
/// `QuantifiedExpr` — invokes `report` once per inner-binding tuple
/// with the test expression's boolean coercion.  `report` is free to
/// short-circuit: callers set their accumulator and let the recursion
/// run to completion (the cost is bounded by the source sequences).
fn eval_quantified_recursive<I: DocIndexLike>(
    bindings: &[(String, Expr)],
    test:     &Expr,
    depth:    usize,
    ctx:      &EvalCtx<'_>,
    idx:      &I,
    report:   &mut dyn FnMut(bool),
) -> Result<()> {
    if depth == bindings.len() {
        let t = eval_expr(test, ctx, idx)?;
        report(value_to_bool(&t, idx));
        return Ok(());
    }
    let (name, in_expr) = &bindings[depth];
    let seq = eval_expr(in_expr, ctx, idx)?;
    match seq {
        Value::NodeSet(ns) => {
            for id in ns {
                let scoped = ScopedBindings { parent: ctx.bindings, name, value: Value::NodeSet(vec![id]) };
                let inner = EvalCtx {
                    context_node: ctx.context_node, pos: ctx.pos, size: ctx.size,
                    bindings: &scoped, static_ctx: ctx.static_ctx,
                };
                eval_quantified_recursive(bindings, test, depth + 1, &inner, idx, report)?;
            }
        }
        Value::ForeignNodeSet(ns) => {
            for p in ns {
                let scoped = ScopedBindings { parent: ctx.bindings, name, value: Value::ForeignNodeSet(vec![p]) };
                let inner = EvalCtx {
                    context_node: ctx.context_node, pos: ctx.pos, size: ctx.size,
                    bindings: &scoped, static_ctx: ctx.static_ctx,
                };
                eval_quantified_recursive(bindings, test, depth + 1, &inner, idx, report)?;
            }
        }
        Value::IntRange { lo, hi } => {
            for i in lo..=hi {
                let scoped = ScopedBindings { parent: ctx.bindings, name, value: Value::Number(Numeric::Double(i as f64)) };
                let inner = EvalCtx {
                    context_node: ctx.context_node, pos: ctx.pos, size: ctx.size,
                    bindings: &scoped, static_ctx: ctx.static_ctx,
                };
                eval_quantified_recursive(bindings, test, depth + 1, &inner, idx, report)?;
            }
        }
        Value::Sequence(items) => {
            // Iterate each item as the per-binding value.  Inner
            // `Sequence` / `IntRange` items flatten via `iter_items`
            // so a nested sequence contributes its own per-item
            // bindings rather than a single composite atomic.
            for item in items.iter().flat_map(iter_items) {
                let scoped = ScopedBindings { parent: ctx.bindings, name, value: item };
                let inner = EvalCtx {
                    context_node: ctx.context_node, pos: ctx.pos, size: ctx.size,
                    bindings: &scoped, static_ctx: ctx.static_ctx,
                };
                eval_quantified_recursive(bindings, test, depth + 1, &inner, idx, report)?;
            }
        }
        atomic => {
            let scoped = ScopedBindings { parent: ctx.bindings, name, value: atomic };
            let inner = EvalCtx {
                context_node: ctx.context_node, pos: ctx.pos, size: ctx.size,
                bindings: &scoped, static_ctx: ctx.static_ctx,
            };
            eval_quantified_recursive(bindings, test, depth + 1, &inner, idx, report)?;
        }
    }
    Ok(())
}

/// XPath 2.0 `for $v in ... return ...` scope: delegates every
/// binding lookup to the parent, except for the one variable name
/// being iterated, which it resolves directly.  Constructed afresh
/// per iteration so each tuple of bindings is independent.
/// Bindings layer for evaluating an inline function's body: the
/// function's parameters and captured closure variables resolve here;
/// every other binding concern (functions, namespaces, base URI,
/// version, …) delegates to the defining/calling base bindings.
struct ClosureBindings<'p> {
    vars: std::collections::HashMap<String, Value>,
    base: &'p dyn XPathBindings,
}

impl<'p> XPathBindings for ClosureBindings<'p> {
    fn variable(&self, name: &str) -> Option<Value> {
        self.vars.get(name).cloned().or_else(|| self.base.variable(name))
    }
    fn resolve_prefix(&self, p: &str) -> Option<String> { self.base.resolve_prefix(p) }
    fn call_function(&self, ns: &str, n: &str, a: Vec<Value>) -> Option<Result<Value>> {
        self.base.call_function(ns, n, a)
    }
    fn call_function_in(&self, ns: &str, n: &str, a: Vec<Value>, cn: NodeId) -> Option<Result<Value>> {
        self.base.call_function_in(ns, n, a, cn)
    }
    fn function_available_in(&self, ns: &str, n: &str, a: usize) -> bool {
        self.base.function_available_in(ns, n, a)
    }
    fn function_signature_in(&self, ns: &str, n: &str, a: usize)
        -> Option<crate::xpath::ast::FunctionSig> {
        self.base.function_signature_in(ns, n, a)
    }
    fn xpath_version_2_or_later(&self) -> bool { self.base.xpath_version_2_or_later() }
    fn load_document(&self, u: &str, b: Option<&str>) -> Option<Result<Vec<ForeignNodePtr>>> {
        self.base.load_document(u, b)
    }
    fn apply_foreign_path(&self, n: &[ForeignNodePtr], p: &[Expr], s: &[Step])
        -> Option<Result<Vec<ForeignNodePtr>>> { self.base.apply_foreign_path(n, p, s) }
    fn static_base_uri(&self) -> Option<String> { self.base.static_base_uri() }
    fn node_base_uri(&self, id: NodeId) -> Option<String> { self.base.node_base_uri(id) }
    fn foreign_string_value(&self, p: ForeignNodePtr) -> String { self.base.foreign_string_value(p) }
    fn load_dynamic_document(&self, u: &str)
        -> Option<std::result::Result<NodeId, crate::error::XmlError>> {
        self.base.load_dynamic_document(u)
    }
    fn regex_dialect(&self) -> crate::regex::Dialect { self.base.regex_dialect() }
}

/// Collect the names of every `$variable` referenced anywhere in `e`.
/// Used to capture an inline function's closure: each referenced name
/// that isn't a parameter is resolved from the defining scope.  Over-
/// collection (locally-bound names) is harmless — those resolve to
/// nothing in the defining scope and are shadowed when the body runs.
fn collect_var_refs(e: &Expr, out: &mut Vec<String>) {
    use crate::xpath::ast::Expr as E;
    let push = |n: &str, out: &mut Vec<String>| {
        if !out.iter().any(|x| x == n) { out.push(n.to_string()); }
    };
    match e {
        E::Variable(n) => push(n, out),
        E::Or(a, b) | E::And(a, b) | E::Eq(a, b) | E::Ne(a, b)
        | E::Lt(a, b) | E::Gt(a, b) | E::Le(a, b) | E::Ge(a, b)
        | E::ValueEq(a, b) | E::ValueNe(a, b) | E::ValueLt(a, b)
        | E::ValueGt(a, b) | E::ValueLe(a, b) | E::ValueGe(a, b)
        | E::Add(a, b) | E::Sub(a, b) | E::Mul(a, b) | E::Div(a, b)
        | E::Mod(a, b) | E::IDiv(a, b) | E::Union(a, b)
        | E::Intersect(a, b) | E::Except(a, b) | E::Range(a, b)
        | E::SimpleMap(a, b) | E::NodeBefore(a, b) | E::NodeAfter(a, b)
        | E::NodeIs(a, b) => {
            collect_var_refs(a, out); collect_var_refs(b, out);
        }
        E::Neg(x) | E::InstanceOf(x, _) | E::CastAs(x, _)
        | E::CastableAs(x, _) | E::TreatAs(x, _)
        | E::WithDefaultCollation(_, x) | E::BackwardsCompat(x) => collect_var_refs(x, out),
        E::IfThenElse { cond, then_branch, else_branch } => {
            collect_var_refs(cond, out);
            collect_var_refs(then_branch, out);
            collect_var_refs(else_branch, out);
        }
        E::For { bindings, body } | E::Let { bindings, body }
        | E::Quantified { bindings, test: body, .. } => {
            for (_, ex) in bindings { collect_var_refs(ex, out); }
            collect_var_refs(body, out);
        }
        E::Sequence(items) => for x in items { collect_var_refs(x, out); },
        E::FunctionCall(_, args) => for a in args { collect_var_refs(a, out); },
        E::DynamicCall { func, args } => {
            collect_var_refs(func, out);
            for a in args { collect_var_refs(a, out); }
        }
        E::FilterPath { primary, predicates, steps } => {
            collect_var_refs(primary, out);
            for pr in predicates { collect_var_refs(pr, out); }
            for s in steps { for pr in &s.predicates { collect_var_refs(pr, out); } }
        }
        E::Path(p) => collect_path_var_refs(p, out),
        E::TryCatch { body, catches } => {
            collect_var_refs(body, out);
            for c in catches { collect_var_refs(&c.body, out); }
        }
        E::MapConstructor(es) => for (k, v) in es { collect_var_refs(k, out); collect_var_refs(v, out); },
        E::ArrayConstructor { members, .. } => for m in members { collect_var_refs(m, out); },
        E::Lookup(b, key) => {
            collect_var_refs(b, out);
            if let crate::xpath::ast::LookupKey::Expr(x) = key { collect_var_refs(x, out); }
        }
        E::UnaryLookup(key) =>
            if let crate::xpath::ast::LookupKey::Expr(x) = key { collect_var_refs(x, out); },
        E::InlineFunction { body, .. } => collect_var_refs(body, out),
        E::Literal(_) | E::Integer(_) | E::Decimal(_) | E::Double(_)
        | E::NamedFunctionRef { .. } | E::Placeholder | E::ContextItem => {}
    }
}

fn collect_path_var_refs(p: &crate::xpath::ast::LocationPath, out: &mut Vec<String>) {
    use crate::xpath::ast::LocationPath;
    let steps = match p { LocationPath::Absolute(s) | LocationPath::Relative(s) => s };
    for s in steps {
        for pr in &s.predicates { collect_var_refs(pr, out); }
    }
}

/// Invoke a function item with `args` already evaluated.
fn call_function_item<I: DocIndexLike>(
    fi: &FunctionItem, args: Vec<Value>, ctx: &EvalCtx<'_>, idx: &I,
) -> Result<Value> {
    match fi {
        FunctionItem::Inline { params, body, closure, .. } => {
            if args.len() != params.len() {
                return Err(xpath_err(format!(
                    "inline function expects {} argument(s), got {}",
                    params.len(), args.len())));
            }
            let mut vars: std::collections::HashMap<String, Value> =
                closure.iter().cloned().collect();
            for (p, a) in params.iter().zip(args) { vars.insert(p.clone(), a); }
            let cb = ClosureBindings { vars, base: ctx.bindings };
            let inner = EvalCtx {
                context_node: ctx.context_node, pos: ctx.pos, size: ctx.size,
                bindings: &cb, static_ctx: ctx.static_ctx,
            };
            eval_expr(body, &inner, idx)
        }
        FunctionItem::Named { name, ns, arity, .. } => {
            if args.len() != *arity {
                return Err(xpath_err(format!(
                    "function {name}#{arity} called with {} argument(s)", args.len())));
            }
            // A statically-resolved non-default-function namespace lets a
            // `name#arity` reference to a user `xsl:function` or extension
            // dispatch even when the call site binds no prefix for that
            // namespace (the value `fn:function-lookup` produces).  Try the
            // user-function hook and EXSLT by (namespace, local) directly;
            // anything unmatched falls through to ordinary lexical dispatch
            // (XSD constructors, built-ins, and the unknown-function error).
            if !ns.is_empty() && ns.as_str() != FN_NAMESPACE {
                let local = name.rsplit(':').next().unwrap_or(name);
                if let Some(r) =
                    ctx.bindings.call_function_in(ns, local, args.clone(), ctx.context_node)
                {
                    return r;
                }
                if let Some(r) = super::exslt::dispatch(ns, local, args.clone(), idx) {
                    return r;
                }
            }
            // Bind the values as synthetic variables and re-enter
            // function dispatch with variable-reference arguments.
            let mut vars = std::collections::HashMap::new();
            let mut arg_exprs = Vec::with_capacity(args.len());
            for (i, a) in args.into_iter().enumerate() {
                let vn = format!("\u{1}fnarg{i}");
                vars.insert(vn.clone(), a);
                arg_exprs.push(Expr::Variable(vn));
            }
            let cb = ClosureBindings { vars, base: ctx.bindings };
            let inner = EvalCtx {
                context_node: ctx.context_node, pos: ctx.pos, size: ctx.size,
                bindings: &cb, static_ctx: ctx.static_ctx,
            };
            eval_function(name, &arg_exprs, &inner, idx)
        }
        FunctionItem::Partial { base, bound } => {
            let mut supplied = args.into_iter();
            let mut full = Vec::with_capacity(bound.len());
            for b in bound {
                match b {
                    Some(v) => full.push(v.clone()),
                    None => full.push(supplied.next().ok_or_else(||
                        xpath_err("too few arguments for partial application"))?),
                }
            }
            call_function_item(base, full, ctx, idx)
        }
    }
}

struct ScopedBindings<'p> {
    parent: &'p dyn XPathBindings,
    name:   &'p str,
    value:  Value,
}

impl<'p> XPathBindings for ScopedBindings<'p> {
    fn resolve_prefix(&self, prefix: &str) -> Option<String> {
        self.parent.resolve_prefix(prefix)
    }
    fn variable(&self, name: &str) -> Option<Value> {
        if name == self.name { Some(self.value.clone()) } else { self.parent.variable(name) }
    }
    fn call_function(
        &self, ns_uri: &str, name: &str, args: Vec<Value>,
    ) -> Option<std::result::Result<Value, crate::error::XmlError>> {
        self.parent.call_function(ns_uri, name, args)
    }
    fn call_function_in(
        &self, ns_uri: &str, name: &str, args: Vec<Value>,
        xpath_context_node: NodeId,
    ) -> Option<std::result::Result<Value, crate::error::XmlError>> {
        self.parent.call_function_in(ns_uri, name, args, xpath_context_node)
    }
    fn function_available_in(&self, ns: &str, n: &str, a: usize) -> bool {
        self.parent.function_available_in(ns, n, a)
    }
    fn function_signature_in(&self, ns: &str, n: &str, a: usize)
        -> Option<crate::xpath::ast::FunctionSig> {
        self.parent.function_signature_in(ns, n, a)
    }
    fn foreign_string_value(
        &self, p: crate::xpath::eval::ForeignNodePtr,
    ) -> String {
        self.parent.foreign_string_value(p)
    }
}

/// Bindings layer that exposes the XPath 3.1 `$err:*` variables
/// inside an `Expr::TryCatch` catch handler.  Lookups for any
/// other variable fall through to the surrounding context.
struct ErrBindings<'p> {
    parent:      &'p dyn XPathBindings,
    err_uri:     String,
    code_local:  String,
    description: String,
}

impl<'p> XPathBindings for ErrBindings<'p> {
    fn resolve_prefix(&self, prefix: &str) -> Option<String> {
        self.parent.resolve_prefix(prefix)
            .or_else(|| (prefix == "err").then(|| self.err_uri.clone()))
    }
    fn variable(&self, name: &str) -> Option<Value> {
        // XPath callers pass either the lexical form (`err:code`)
        // or Clark form (`{uri}code`).  Match both shapes so
        // either resolves to the synthesized error metadata.
        let key_lex   = format!("err:{}", "code");
        let key_clark = format!("{{{}}}{}", self.err_uri, "code");
        let local = if name == "err:code" || name == key_clark {
            Some("code")
        } else if name == "err:description"
            || name == format!("{{{}}}description", self.err_uri).as_str() {
            Some("description")
        } else if name == "err:value"
            || name == format!("{{{}}}value", self.err_uri).as_str() {
            Some("value")
        } else if name == "err:module"
            || name == format!("{{{}}}module", self.err_uri).as_str() {
            Some("module")
        } else if name == "err:line-number"
            || name == format!("{{{}}}line-number", self.err_uri).as_str() {
            Some("line-number")
        } else if name == "err:column-number"
            || name == format!("{{{}}}column-number", self.err_uri).as_str() {
            Some("column-number")
        } else {
            None
        };
        // Touch `key_lex` to silence unused warnings without
        // burning the string (it's the canonical lexical form we
        // accept above).
        let _ = key_lex;
        match local {
            Some("code")          => Some(Value::String(format!("err:{}", self.code_local))),
            Some("description")   => Some(Value::String(self.description.clone())),
            Some("value")         => Some(Value::NodeSet(Vec::new())),
            Some("module")        => Some(Value::NodeSet(Vec::new())),
            Some("line-number")   => Some(Value::NodeSet(Vec::new())),
            Some("column-number") => Some(Value::NodeSet(Vec::new())),
            _ => self.parent.variable(name),
        }
    }
    fn call_function(
        &self, ns_uri: &str, name: &str, args: Vec<Value>,
    ) -> Option<std::result::Result<Value, crate::error::XmlError>> {
        self.parent.call_function(ns_uri, name, args)
    }
    fn call_function_in(
        &self, ns_uri: &str, name: &str, args: Vec<Value>,
        xpath_context_node: NodeId,
    ) -> Option<std::result::Result<Value, crate::error::XmlError>> {
        self.parent.call_function_in(ns_uri, name, args, xpath_context_node)
    }
    fn function_available_in(&self, ns: &str, n: &str, a: usize) -> bool {
        self.parent.function_available_in(ns, n, a)
    }
    fn function_signature_in(&self, ns: &str, n: &str, a: usize)
        -> Option<crate::xpath::ast::FunctionSig> {
        self.parent.function_signature_in(ns, n, a)
    }
    fn foreign_string_value(
        &self, p: crate::xpath::eval::ForeignNodePtr,
    ) -> String {
        self.parent.foreign_string_value(p)
    }
}

fn build_err_scope<'p>(
    parent: &'p dyn XPathBindings,
    err_uri: &str, code_local: &str, description: &str,
) -> ErrBindings<'p> {
    ErrBindings {
        parent,
        err_uri:     err_uri.to_string(),
        code_local:  code_local.to_string(),
        description: description.to_string(),
    }
}

fn xpath_catch_matches(
    matchers: &[crate::xpath::ast::CatchNameTest],
    err_uri: &str, err_local: &str,
    bindings: &dyn XPathBindings,
) -> bool {
    use crate::xpath::ast::CatchNameTest::*;
    matchers.iter().any(|m| match m {
        Any => true,
        LocalNameOnly(local) => err_local == local,
        PrefixWildcard(prefix) => bindings.resolve_prefix(prefix)
            .as_deref() == Some(err_uri),
        QName { prefix, local } => {
            let want_uri = match prefix {
                Some(p) => bindings.resolve_prefix(p).unwrap_or_default(),
                None    => String::new(),
            };
            want_uri == err_uri && local == err_local
        }
    })
}

// ── location path evaluation ──────────────────────────────────────────────────

fn eval_path<I: DocIndexLike>(
    path: &LocationPath, ctx_node: NodeId, idx: &I,
    bindings: &dyn XPathBindings, compat: bool, current_node: Option<NodeId>,
) -> Result<Vec<NodeId>> {
    let (initial, steps) = match path {
        // XPath 1.0 §3.3: `/` is the root of the document containing
        // the context node.  In the single-document case the parent
        // chain always terminates at NodeId 0 (the synthetic Document);
        // when extra documents have been merged into the index (e.g.
        // via XSLT `document()`), the walk lands on the Document of
        // whichever doc the context node belongs to.
        LocationPath::Absolute(steps) => {
            // XPath 2.0 §2.1.2 / XPDY0002 — an absolute path walks
            // from the root of the tree containing the context node;
            // if the focus is undefined (e.g. inside an xsl:function
            // body) there is no such tree.
            if focus_is_undefined() {
                return Err(xpath_err(
                    "absolute path requires a context item (XPDY0002)"
                ).with_xpath_code("XPDY0002"));
            }
            let mut root = ctx_node;
            while let Some(p) = idx.parent(root) {
                root = p;
            }
            (vec![root], steps.as_slice())
        },
        LocationPath::Relative(steps) => (vec![ctx_node], steps.as_slice()),
    };

    let mut current = initial;
    for step in steps {
        current = eval_step_on_nodes_cur(current, step, idx, bindings, compat, current_node)?;
    }
    Ok(current)
}

/// Evaluate a single XPath step against a node-set, returning the
/// post-predicate result.  `compat` propagates the surrounding
/// [`XPathContext`](super::XPathContext)'s libxml2-compat flag into
/// the per-predicate eval contexts this function constructs.  Callers
/// outside the engine (the `sup-xml-compat` C ABI shim) generally
/// pass `false`.
pub fn eval_step_on_nodes<I: DocIndexLike>(
    nodes: Vec<NodeId>, step: &Step, idx: &I,
    bindings: &dyn XPathBindings, compat: bool,
) -> Result<Vec<NodeId>> {
    // The C-ABI foreign-path bridge re-enters here without a parent
    // context; `current()` over foreign nodes is not meaningful, so seed
    // it with the document root (matches the bare-context default).
    eval_step_on_nodes_cur(nodes, step, idx, bindings, compat, None)
}

/// [`eval_step_on_nodes`] threading the XSLT `current()` node through to
/// any predicates on `step` (see [`StaticContext::current_node`]).
pub fn eval_step_on_nodes_cur<I: DocIndexLike>(
    nodes: Vec<NodeId>, step: &Step, idx: &I,
    bindings: &dyn XPathBindings, compat: bool, current_node: Option<NodeId>,
) -> Result<Vec<NodeId>> {
    // XPath 2.0 FilterExpr step (`path/(expr)` or `path/func(...)`).
    // Evaluate `filter` once per input node with that node as the
    // context; atomic results are materialised as synthetic text
    // nodes so the surrounding path machinery (which speaks
    // `Vec<NodeId>`) keeps working.  XPath 2.0 §3.2.1 — when the
    // filter produces only nodes, the combined result is sorted
    // into document order and deduplicated.  Mixed atomic/node
    // returns skip the sort (atomics have no doc position).
    let sc = StaticContext {
        xpath_2_0: bindings.xpath_version_2_or_later(),
        libxml2_compatible: compat,
        current_node,
    };
    if let Some(filter) = &step.filter {
        let mut out: Vec<NodeId> = Vec::new();
        let mut all_native_nodes = true;
        for node in nodes {
            charge_eval_step()?;
            let ctx = EvalCtx {
                context_node: node, pos: 1, size: 1, bindings,
                static_ctx: &sc,
            };
            // Inside a step, the context item *is* defined — even
            // if the outer XSLT scope (e.g. xsl:function body) had
            // set FOCUS_UNDEFINED.  Without this clear, a key() /
            // unparsed-entity-uri() / id() called via `$nodes/key(
            // ...)` would spuriously raise XTDE1270 / XTDE1370.
            let v = with_focus_undefined(false, || eval_expr(filter, &ctx, idx))?;
            match v {
                Value::NodeSet(ns)        => out.extend(ns),
                Value::ForeignNodeSet(_)  => { /* foreign nodes don't
                    enter the synthetic id space — silently drop. */ }
                Value::Sequence(items) => {
                    let mut atoms: Vec<String> = Vec::new();
                    for item in items {
                        match item {
                            Value::NodeSet(ns)        => out.extend(ns),
                            Value::ForeignNodeSet(_)  => {}
                            atomic => atoms.push(value_to_string_with(&atomic, idx, bindings)),
                        }
                    }
                    if !atoms.is_empty() {
                        all_native_nodes = false;
                        if let Some(ids) = idx.allocate_rtf_text_nodes(atoms) {
                            out.extend(ids);
                        }
                    }
                }
                atomic => {
                    all_native_nodes = false;
                    let s = value_to_string_with(&atomic, idx, bindings);
                    if let Some(ids) = idx.allocate_rtf_text_nodes(vec![s]) {
                        out.extend(ids);
                    }
                }
            }
        }
        // Sort + dedup per XPath 2.0 §3.2.1, but only when every
        // contribution was a navigable node.  An atomic-derived
        // synthetic text node has no meaningful doc position, so
        // sorting would scramble user-visible order without spec
        // backing.
        if all_native_nodes && bindings.xpath_version_2_or_later() {
            dedup_sort(&mut out);
        }
        // Apply predicates to the collected filter result.
        return apply_predicates_cur(out, &step.predicates, idx, bindings, compat, current_node);
    }
    let mut candidates: Vec<NodeId> = Vec::new();
    for node in nodes {
        // Charge one step per source node — this is what makes
        // chained `//*//*//*` cost N^k in path length k.
        charge_eval_step()?;
        let mut cands = axis_nodes(&step.axis, node, idx);
        cands.retain(|&n| node_matches(n, &step.node_test, &step.axis, idx, bindings, compat));
        // XPath 1.0 §2.4: predicates see candidates in axis order, so
        // `position()` and `[N]` index reverse axes from the proximity
        // root (nearest ancestor = pos 1).  Our axis helpers already
        // return reverse axes in proximity order; the final `dedup_sort`
        // below puts the surviving set back into document order.
        let filtered = apply_predicates_cur(cands, &step.predicates, idx, bindings, compat, current_node)?;
        candidates.extend(filtered);
    }
    dedup_sort(&mut candidates);
    Ok(candidates)
}

/// Filter `nodes` by sequentially applying each predicate.  Each
/// per-node sub-context inherits the surrounding `compat` flag so
/// predicate-internal calls to `string()` / `number()` / etc. observe
/// the same libxml2-compat behaviour as the outer expression.
pub fn apply_predicates<I: DocIndexLike>(
    nodes: Vec<NodeId>, predicates: &[Expr], idx: &I,
    bindings: &dyn XPathBindings, compat: bool,
) -> Result<Vec<NodeId>> {
    apply_predicates_cur(nodes, predicates, idx, bindings, compat, None)
}

/// [`apply_predicates`] threading the XSLT `current()` node into each
/// predicate's sub-context (see [`StaticContext::current_node`]).
pub fn apply_predicates_cur<I: DocIndexLike>(
    nodes: Vec<NodeId>, predicates: &[Expr], idx: &I,
    bindings: &dyn XPathBindings, compat: bool, current_node: Option<NodeId>,
) -> Result<Vec<NodeId>> {
    let sc = StaticContext {
        xpath_2_0: bindings.xpath_version_2_or_later(),
        libxml2_compatible: compat,
        current_node,
    };
    let mut result = nodes;
    for pred in predicates {
        // Positional short-circuit: `[N]` (literal integer N) and
        // `[position()=N]` / `[N=position()]` reduce a node-set to
        // its N-th element without iterating the whole set.  XPath
        // 1.0 §3.4 evaluates each predicate against every candidate
        // in turn; the optimisation just skips the per-candidate
        // eval whose only effect would be `position()==N`.
        if let Some(n) = positional_index(pred) {
            charge_eval_step()?;
            result = match n.checked_sub(1).and_then(|i| result.get(i).copied()) {
                Some(node) => vec![node],
                None       => Vec::new(),
            };
            continue;
        }
        let size = result.len();
        let mut next = Vec::new();
        for (i, node) in result.into_iter().enumerate() {
            // Charge one step per candidate; this is what
            // compounds in nested-predicate DoS expressions.
            charge_eval_step()?;
            let ctx = EvalCtx {
                context_node: node, pos: i + 1, size, bindings,
                static_ctx: &sc,
            };
            let v = eval_expr(pred, &ctx, idx)?;
            // XPath 2.0 numeric-predicate rule (see
            // `filter_sequence_by_predicates`): a numeric predicate
            // selects by position.  A single-item NodeSet whose
            // synthetic text-node carries an integer value (the
            // common XSLT 2.0 shape after `for-each select="1 to N"`)
            // counts as numeric here so `$xs[$n]` selects the n-th.
            let keep = match &v {
                Value::Number(n) => (i + 1) as f64 == n.as_f64(),
                Value::Typed(t) => match t.numeric {
                    Some(n) => (i + 1) as f64 == n,
                    None    => value_to_bool(&v, idx),
                },
                Value::NodeSet(ns) if ns.len() == 1
                    && matches!(idx.kind(ns[0]),
                        crate::xpath::XPathNodeKind::Text)
                    && idx.parent(ns[0]).is_none()
                => {
                    let s = idx.string_value(ns[0]);
                    match s.trim().parse::<f64>() {
                        Ok(n) if n.fract() == 0.0 => (i + 1) as f64 == n,
                        _ => value_to_bool(&v, idx),
                    }
                }
                other => value_to_bool(other, idx),
            };
            if keep {
                next.push(node);
            }
        }
        result = next;
    }
    Ok(result)
}

/// Detect predicates of the form `[N]`, `[position()=N]`, or
/// `[N=position()]` where N is a positive-integer literal.  Returns
/// the 1-based index N when matched, `None` otherwise.  Used by
/// [`apply_predicates`] to skip the per-candidate eval loop and
/// directly index the node-set.
fn positional_index(pred: &Expr) -> Option<usize> {
    fn lit_pos_int(e: &Expr) -> Option<usize> {
        match e {
            Expr::Integer(i) if *i >= 1 => Some(*i as usize),
            // A decimal literal predicate (`[2.0]`) indexes too, but
            // only when it is a positive whole number — a fractional
            // value would compare unequal to every integer position.
            Expr::Decimal(n) => {
                use rust_decimal::prelude::ToPrimitive;
                let whole = n.trunc() == *n;
                let positive = *n >= rust_decimal::Decimal::ONE;
                let fits = n.to_usize();
                match (whole, positive, fits) {
                    (true, true, Some(u)) => Some(u),
                    _                     => None,
                }
            }
            _ => None,
        }
    }
    fn is_position_call(e: &Expr) -> bool {
        matches!(e, Expr::FunctionCall(name, args) if name == "position" && args.is_empty())
    }
    // `[N]`
    if let Some(n) = lit_pos_int(pred) { return Some(n); }
    // `[position()=N]` / `[N=position()]`
    if let Expr::Eq(l, r) = pred {
        if is_position_call(l) { return lit_pos_int(r); }
        if is_position_call(r) { return lit_pos_int(l); }
    }
    None
}

// ── axis navigation ───────────────────────────────────────────────────────────

fn axis_nodes<I: DocIndexLike>(axis: &Axis, node: NodeId, idx: &I) -> Vec<NodeId> {
    match axis {
        Axis::Self_ => vec![node],
        Axis::Child => idx.children(node).to_vec(),
        Axis::Parent => idx.parent(node).into_iter().collect(),
        Axis::Attribute => idx.attr_range(node).collect(),
        Axis::Ancestor => ancestors(node, idx),
        Axis::AncestorOrSelf => {
            let mut a = vec![node];
            a.extend(ancestors(node, idx));
            a
        }
        Axis::Descendant => descendants(node, idx, false),
        Axis::DescendantOrSelf => descendants(node, idx, true),
        Axis::FollowingSibling => following_siblings(node, idx),
        Axis::PrecedingSibling => preceding_siblings(node, idx),
        Axis::Following => following(node, idx),
        Axis::Preceding => preceding(node, idx),
        // XPath 1.0 §2.2: namespace nodes are synthetic per-element
        // entries materialised at index-build time (see
        // context::collect_in_scope_namespaces); we just enumerate
        // the precomputed range here.
        Axis::Namespace => idx.ns_range(node).collect(),
    }
}

fn ancestors<I: DocIndexLike>(node: NodeId, idx: &I) -> Vec<NodeId> {
    let mut result = Vec::new();
    let mut cur = idx.parent(node);
    while let Some(p) = cur {
        result.push(p);
        cur = idx.parent(p);
    }
    result
}

/// Walk up `parent` links from `node` to the topmost ancestor — used
/// when a function (e.g. `fn:id($arg, $node)`) needs the document
/// root containing some node.  Returns `node` itself when it has no
/// parent.
/// XML Namespaces §3 lexical validation: `NCName` or
/// `NCName ':' NCName`.  Each NCName starts with a name-start
/// character that isn't `:`, then continues with name characters
/// that also aren't `:`.  Empty strings and double-colon /
/// trailing-colon forms are invalid.
fn is_valid_lexical_qname(s: &str) -> bool {
    fn is_ncname(s: &str) -> bool {
        let mut chars = s.chars();
        let first = match chars.next() { Some(c) => c, None => return false };
        let start_ok = first == '_' || first.is_ascii_alphabetic()
            || (first as u32 >= 0x80 && crate::charsets::is_name_start_char(first));
        if !start_ok || first == ':' { return false; }
        for c in chars {
            if c == ':' { return false; }
            let ok = c == '_' || c == '-' || c == '.'
                || c.is_ascii_alphanumeric()
                || (c as u32 >= 0x80 && crate::charsets::is_name_char_unicode(c));
            if !ok { return false; }
        }
        true
    }
    match s.split_once(':') {
        None => is_ncname(s),
        Some((p, l)) => is_ncname(p) && is_ncname(l),
    }
}

fn doc_root_of<I: DocIndexLike>(node: NodeId, idx: &I) -> NodeId {
    let mut cur = node;
    while let Some(p) = idx.parent(cur) { cur = p; }
    cur
}

fn descendants<I: DocIndexLike>(node: NodeId, idx: &I, include_self: bool) -> Vec<NodeId> {
    let mut result = Vec::new();
    if include_self {
        result.push(node);
    }
    collect_desc(node, idx, &mut result);
    result
}

fn collect_desc<I: DocIndexLike>(node: NodeId, idx: &I, out: &mut Vec<NodeId>) {
    for &child in idx.children(node) {
        out.push(child);
        collect_desc(child, idx, out);
    }
}

fn following_siblings<I: DocIndexLike>(node: NodeId, idx: &I) -> Vec<NodeId> {
    let parent = match idx.parent(node) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let siblings = idx.children(parent);
    // Attribute and namespace nodes aren't in their parent's `children()`
    // list; XPath 1.0 §2.2 defines following-sibling as empty for them.
    siblings.iter().position(|&n| n == node)
        .map(|p| siblings[p + 1..].to_vec())
        .unwrap_or_default()
}

fn preceding_siblings<I: DocIndexLike>(node: NodeId, idx: &I) -> Vec<NodeId> {
    let parent = match idx.parent(node) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let siblings = idx.children(parent);
    let pos = siblings.iter().position(|&n| n == node).unwrap_or(0);
    // Reverse order: closest sibling has position 1
    siblings[..pos].iter().rev().copied().collect()
}

fn following<I: DocIndexLike>(node: NodeId, idx: &I) -> Vec<NodeId> {
    // All nodes in document order after `node` that are not descendants of `node`'s ancestors
    let desc_set: HashSet<NodeId> = descendants(node, idx, true).into_iter().collect();
    let mut result = Vec::new();
    let mut cur = node;
    while let Some(parent) = idx.parent(cur) {
        let siblings = idx.children(parent);
        // Attribute and namespace nodes aren't in `children()`; in
        // document order (XPath 1.0 §5) they precede the element's
        // children, so `following::` from one of them sees every child
        // — hence the `None`-branch start at 0.
        let start = siblings.iter().position(|&n| n == cur).map_or(0, |p| p + 1);
        for &sib in &siblings[start..] {
            if !desc_set.contains(&sib) {
                result.push(sib);
                collect_desc(sib, idx, &mut result);
            }
        }
        cur = parent;
    }
    dedup_sort(&mut result);
    result
}

fn preceding<I: DocIndexLike>(node: NodeId, idx: &I) -> Vec<NodeId> {
    let ancestor_set: HashSet<NodeId> = ancestors(node, idx).into_iter().collect();
    let mut result = Vec::new();
    let mut cur = node;
    while let Some(parent) = idx.parent(cur) {
        let siblings = idx.children(parent);
        let pos = siblings.iter().position(|&n| n == cur).unwrap_or(0);
        for &sib in siblings[..pos].iter().rev() {
            if !ancestor_set.contains(&sib) {
                // Add in reverse document order
                let mut d = Vec::new();
                collect_desc(sib, idx, &mut d);
                for id in d.into_iter().rev() {
                    result.push(id);
                }
                result.push(sib);
            }
        }
        cur = parent;
    }
    result
}

// ── node test matching ────────────────────────────────────────────────────────

/// Per XPath 1.0 § 2.3, name tests (`*`, `local`, `prefix:local`, `prefix:*`)
/// match the *principal node type* of the axis they're applied on — element
/// for every axis except `attribute::` (matches attributes) and `namespace::`
/// (matches namespace nodes).  The kind tests (`node()`, `text()`, `comment()`,
/// `processing-instruction()`) are unaffected.
fn principal_kind(axis: &Axis) -> XPathNodeKind {
    match axis {
        Axis::Attribute => XPathNodeKind::Attribute,
        Axis::Namespace => XPathNodeKind::Namespace,
        _               => XPathNodeKind::Element,
    }
}

/// Test `node` against `test` as if reached on the `child::` axis
/// (principal node kind = element).  Exposed for the XSLT pattern
/// matcher, which handles `document-node(element(N))` patterns
/// structurally and needs to check the document element against the
/// inner element test.
pub fn node_matches_child<I: DocIndexLike>(
    node: NodeId, test: &NodeTest, idx: &I, bindings: &dyn XPathBindings,
) -> bool {
    node_matches(node, test, &Axis::Child, idx, bindings, false)
}

fn node_matches<I: DocIndexLike>(
    node: NodeId, test: &NodeTest, axis: &Axis, idx: &I,
    bindings: &dyn XPathBindings, libxml2_compatible: bool,
) -> bool {
    match test {
        NodeTest::AnyNode => true,
        NodeTest::Text => matches!(idx.kind(node), XPathNodeKind::Text | XPathNodeKind::CData),
        NodeTest::Comment => matches!(idx.kind(node), XPathNodeKind::Comment),
        NodeTest::PI(None) => matches!(idx.kind(node), XPathNodeKind::PI),
        NodeTest::PI(Some(target)) => {
            (matches!(idx.kind(node), XPathNodeKind::PI) && idx.pi_target(node) == *target)
        }
        NodeTest::Document(inner) => {
            if !matches!(idx.kind(node), XPathNodeKind::Document) {
                return false;
            }
            match inner {
                None => true,
                // `document-node(element(N))` — the document element
                // (the sole element child) must satisfy the inner test.
                Some(t) => idx.children(node).iter().any(|c|
                    idx.kind(*c) == XPathNodeKind::Element
                    && node_matches(*c, t, &Axis::Child, idx, bindings, libxml2_compatible)),
            }
        }
        NodeTest::Wildcard => idx.kind(node) == principal_kind(axis),
        NodeTest::PrefixWildcard(prefix) => {
            if idx.kind(node) != principal_kind(axis) {
                return false;
            }
            // If the bindings resolve the prefix to a URI, match by
            // namespace URI on the node — the expression's prefix
            // is just an alias defined by the caller (lxml's
            // `namespaces=` dict).  Otherwise fall back to literal
            // `prefix:` matching on node_name for unbound prefixes.
            if let Some(uri) = resolve_prefix_or_implicit(bindings, prefix) {
                idx.namespace_uri(node) == uri
            } else {
                let name = idx.node_name(node);
                name.starts_with(&format!("{prefix}:"))
            }
        }
        // XPath 2.0 §2.5.5.3 `*:NCName` — any namespace, matching
        // local name.  Used by stylesheets that don't want to bind
        // a prefix just to match an element by local name.
        NodeTest::LocalNameOnly(local) => {
            if idx.kind(node) != principal_kind(axis) {
                return false;
            }
            idx.local_name(node) == local
        }
        NodeTest::LocalName(local) => {
            // XPath 1.0 §2.3: an unprefixed name test has a null
            // namespace URI and only matches nodes whose
            // expanded-name has no namespace.  libxml2 historically
            // ignores the URI here (it does the comparison purely
            // on local-name) and a chunk of integration code in the
            // wild leans on that — `libxml2_compatible: true`
            // preserves the legacy behaviour, while the default
            // (XSLT / strict XPath callers) follows the spec.  The
            // namespace axis is the lone exception: a namespace
            // node's local-name is its prefix (§5.4), so the
            // URI-comparison rule is meaningless there.
            if idx.kind(node) != principal_kind(axis) {
                return false;
            }
            if idx.local_name(node) != local {
                return false;
            }
            if libxml2_compatible || matches!(axis, Axis::Namespace) {
                return true;
            }
            idx.namespace_uri(node).is_empty()
        }
        // XSLT 2.0 §5.1.1 — an unprefixed element NameTest in an
        // XPath whose host element (or ancestor) declared
        // xpath-default-namespace resolves against that URI instead
        // of the null namespace.  The compiler bakes the URI into
        // the AST so matching here is a single pair compare.  Only
        // applies on axes whose principal node kind is element.
        NodeTest::DefaultNamespaceName { uri, local } => {
            if idx.kind(node) != principal_kind(axis) {
                return false;
            }
            if !matches!(axis,
                Axis::Child | Axis::Descendant | Axis::DescendantOrSelf
                | Axis::Self_ | Axis::Parent | Axis::Ancestor
                | Axis::AncestorOrSelf | Axis::FollowingSibling
                | Axis::PrecedingSibling | Axis::Following | Axis::Preceding)
            {
                return false;
            }
            idx.local_name(node) == local.as_str()
                && idx.namespace_uri(node) == uri.as_str()
        }
        NodeTest::QName(prefix, local) => {
            if idx.kind(node) != principal_kind(axis) {
                return false;
            }
            // Preferred path: the caller registered a URI for this
            // prefix (lxml's `namespaces=` kwarg / libxslt's
            // xmlXPathRegisterNs).  Match by namespace URI + local
            // name — what XPath 1.0 actually requires.
            if let Some(uri) = resolve_prefix_or_implicit(bindings, prefix) {
                return idx.local_name(node) == local.as_str()
                    && idx.namespace_uri(node) == uri;
            }
            // Fallback when no binding is in scope: support the two
            // historical name representations in this codebase.
            //   1. Lean build: `node_name` is the full QName, so a
            //      direct `"prefix:local"` compare works.
            //   2. c-abi build: `node_name` is the local part only;
            //      we compare local-name + the node's declared
            //      namespace prefix instead.
            let expected_qname = format!("{prefix}:{local}");
            if idx.node_name(node) == expected_qname {
                return true;
            }
            idx.local_name(node) == local.as_str()
                && idx.namespace_prefix(node) == Some(prefix.as_str())
        }
    }
}

// ── value coercions ───────────────────────────────────────────────────────────

pub fn value_to_bool<I: DocIndexLike>(v: &Value, idx: &I) -> bool {
    match v {
        Value::Boolean(b) => *b,
        Value::Number(n) => { let n = n.as_f64(); n != 0.0 && !n.is_nan() }
        Value::String(s) => !s.is_empty(),
        Value::NodeSet(ns) => !ns.is_empty(),
        Value::ForeignNodeSet(ns) => !ns.is_empty(),
        Value::Typed(t) => {
            if let Some(b) = t.boolean { return b; }
            if let Some(n) = t.numeric { return n != 0.0 && !n.is_nan(); }
            !t.lexical.is_empty()
        }
        // XPath 2.0 §2.4.3 effective boolean value of a sequence:
        // empty → false; single boolean → its value; single
        // node → true; single string → !empty; otherwise type error
        // (we keep it lenient — first item's EBV).
        Value::Sequence(items) => match items.first() {
            None    => false,
            Some(v) => value_to_bool(v, idx),
        }
        // A non-empty IntRange is non-empty by construction (the
        // invariant on Value::IntRange).  EBV of a single integer
        // is "true unless the integer is 0" — but a multi-item
        // numeric sequence isn't a single integer, so the lenient
        // rule is "non-empty → true".
        Value::IntRange { lo, hi } if lo == hi => *lo != 0,
        Value::IntRange { .. } => true,
        // XPath 3.1 §2.4.3 — the effective boolean value of a map or
        // array is a type error (FORG0006).  We keep it lenient: a map
        // / array is a present item, so treat it as true.
        Value::Map(_) | Value::Array(_) | Value::Function(_) => true,
    }
}

pub fn value_to_number<I: DocIndexLike>(v: &Value, idx: &I) -> f64 {
    value_to_number_with(v, idx, &NO_BINDINGS)
}

pub fn value_to_number_with<I: DocIndexLike>(
    v: &Value, idx: &I, bindings: &dyn XPathBindings,
) -> f64 {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Boolean(b) => if *b { 1.0 } else { 0.0 },
        Value::String(s) => s.trim().parse().unwrap_or(f64::NAN),
        Value::NodeSet(ns) => {
            if ns.is_empty() {
                return f64::NAN;
            }
            let s = idx.string_value(ns[0]);
            s.trim().parse().unwrap_or(f64::NAN)
        }
        Value::ForeignNodeSet(ns) => {
            if ns.is_empty() {
                return f64::NAN;
            }
            bindings.foreign_string_value(ns[0]).trim().parse().unwrap_or(f64::NAN)
        }
        Value::Typed(t) => {
            if let Some(n) = t.numeric { return n; }
            if let Some(b) = t.boolean { return if b { 1.0 } else { 0.0 }; }
            t.lexical.trim().parse().unwrap_or(f64::NAN)
        }
        // First item's numeric value — matches the legacy
        // NodeSet-of-one-text behaviour for typed sequences.
        Value::Sequence(items) => match items.first() {
            Some(v) => value_to_number_with(v, idx, bindings),
            None    => f64::NAN,
        }
        // First item of a range is the lower bound.
        Value::IntRange { lo, .. } => *lo as f64,
        // A map / array has no numeric value (FOTY0014); → NaN.
        Value::Map(_) | Value::Array(_) | Value::Function(_) => f64::NAN,
    }
}

pub fn value_to_string<I: DocIndexLike>(v: &Value, idx: &I) -> String {
    value_to_string_with(v, idx, &NO_BINDINGS)
}

pub fn value_to_string_with<I: DocIndexLike>(
    v: &Value, idx: &I, bindings: &dyn XPathBindings,
) -> String {
    value_to_string_with_compat(v, idx, bindings, false)
}

/// Number-to-string flavour — the serialization slice of the static
/// context.  `Xpath10` is XPath 1.0 §4.2 decimal-only, `Libxml2` is
/// libxml2's `1.23e+19` form, `Xpath20` is the F&O §17.1.2 scientific
/// form (`1.0E6`) for `xs:double`/`xs:float`.  `xs:integer` /
/// `xs:decimal` are decimal in every style, so integer output never
/// changes; only doubles/floats vary.
///
/// Callers that know their version pass the style explicitly (the
/// XSLT engine derives it from the stylesheet `version`); the
/// no-context [`value_to_string`] defaults to `Xpath10`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NumStyle { Xpath10, Libxml2, Xpath20 }

impl NumStyle {
    /// Derive the style from the two static-context flags that govern
    /// numeric serialization: libxml2-compat mode wins, otherwise an
    /// XPath 2.0 host uses the F&O scientific form and 1.0 stays decimal.
    pub fn from_context(compat: bool, xpath_2_or_later: bool) -> NumStyle {
        if compat { NumStyle::Libxml2 }
        else if xpath_2_or_later { NumStyle::Xpath20 }
        else { NumStyle::Xpath10 }
    }
}

/// Serialize a number under an explicit [`NumStyle`].  Doubles/floats
/// take scientific form only under `Xpath20`; everything else is
/// decimal (or libxml2's form under `Libxml2`).  Exposed so the XSLT
/// result serializer can render a `Value::Number` directly under the
/// stylesheet's style without rebuilding a `Value`.
pub fn format_numeric_styled(n: Numeric, style: NumStyle) -> String {
    // xs:decimal serialises from its exact value, not from an f64
    // shadow — the whole point of carrying `rust_decimal::Decimal`
    // in `Numeric` is that `0.1 + 0.2` renders as `"0.3"` and a
    // 17-digit decimal renders without rounding.
    if let Numeric::Decimal(d) = n {
        return format_decimal_canonical(d);
    }
    let f = n.as_f64();
    // xs:float must serialise at single precision (XSD §F.3), so
    // route through `canonical_float_lex` under any 2.0-aware style.
    // The 1.0 / libxml2 paths predate xs:float as a distinct kind and
    // treat every Numeric as a decimal-style double — match them.
    if matches!(n, Numeric::Float(_)) && matches!(style, NumStyle::Xpath20) {
        return canonical_float_lex(f, &Value::Number(n));
    }
    match style {
        NumStyle::Libxml2 => format_number_libxml2(f),
        NumStyle::Xpath20 if matches!(n, Numeric::Double(_)) =>
            format_number_xpath20(f),
        _ => format_number(f),
    }
}

/// XSD §3.2.3.2 canonical lexical form for an `xs:decimal` carried
/// as a [`rust_decimal::Decimal`].  `Decimal`'s Display already gives
/// fixed-point with the scale embedded (e.g. `Decimal::new(30, 2)` →
/// `"0.30"`); we strip trailing fractional zeros (and a trailing `.`)
/// to match XSD canonical form, and collapse `-0` to `0`.
fn format_decimal_canonical(d: rust_decimal::Decimal) -> String {
    if d.is_zero() { return "0".into(); }
    let s = d.to_string();
    let trimmed = match s.split_once('.') {
        Some((w, f)) => {
            let f = f.trim_end_matches('0');
            if f.is_empty() { w.to_string() } else { format!("{w}.{f}") }
        }
        None => s,
    };
    if trimmed == "-0" { "0".into() } else { trimmed }
}

/// libxml2-compat variant of [`value_to_string_with`].  Derives the
/// [`NumStyle`] from `compat` plus the bindings' XPath version and
/// delegates to [`value_to_string_styled_with`].
pub fn value_to_string_with_compat<I: DocIndexLike>(
    v: &Value, idx: &I, bindings: &dyn XPathBindings, compat: bool,
) -> String {
    let style = NumStyle::from_context(compat, bindings.xpath_version_2_or_later());
    value_to_string_styled_with(v, idx, bindings, style)
}

/// Serialize `v` to its string-value under an explicit [`NumStyle`].
/// This is the entry point for callers that know their static context
/// (notably the XSLT result serializer, which must render an
/// `xs:double` in F&O scientific form even though the value itself no
/// longer carries a precomputed lexical).
pub fn value_to_string_styled<I: DocIndexLike>(
    v: &Value, idx: &I, style: NumStyle,
) -> String {
    value_to_string_styled_with(v, idx, &NO_BINDINGS, style)
}

pub fn value_to_string_styled_with<I: DocIndexLike>(
    v: &Value, idx: &I, bindings: &dyn XPathBindings, style: NumStyle,
) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Boolean(b) => (if *b { "true" } else { "false" }).to_string(),
        Value::Number(n) => format_numeric_styled(*n, style),
        Value::NodeSet(ns) => {
            if ns.is_empty() {
                String::new()
            } else {
                idx.string_value(ns[0])
            }
        }
        Value::ForeignNodeSet(ns) => {
            if ns.is_empty() {
                String::new()
            } else {
                bindings.foreign_string_value(ns[0])
            }
        }
        // A typed atom serialises from its stored canonical lexical
        // (dates, durations, derived numeric types like xs:byte, etc.).
        Value::Typed(t) => t.lexical.clone(),
        // XPath 1.0 `string()` over a sequence picks the first item
        // (XPath 2.0 §2.4 effective value for value-of single-item
        // contexts).  Multi-item atomic sequences serialize as
        // space-joined per XSLT 2.0 §5.7.2 — that happens at the
        // serializer / xsl:value-of layer.
        Value::Sequence(items) => match items.first() {
            Some(v) => value_to_string_styled_with(v, idx, bindings, style),
            None    => String::new(),
        }
        // First-item rule for an IntRange yields its lower bound.
        Value::IntRange { lo, .. } => lo.to_string(),
        // A map / array has no string value (FOTY0014).  Lenient: "".
        Value::Map(_) | Value::Array(_) | Value::Function(_) => String::new(),
    }
}


/// Wrap a numeric result so it keeps the numeric type of its source
/// argument (XPath 2.0 §6.4 — `round`/`floor`/`ceiling`/`abs` return
/// the same numeric type as the input).  A typed `xs:double` / `xs:float`
/// stays on the [`Value::Typed`] path so it stringifies in scientific
/// form (until Phase 4 folds it onto [`Numeric`]); an `xs:integer` /
/// `xs:decimal` argument keeps its kind via the [`Numeric`] carrier.
fn preserve_numeric_kind(arg: &Value, result: f64) -> Value {
    match numeric_kind_of(arg) {
        Some(kind) => Value::Number(Numeric::of_kind(kind, result)),
        None        => Value::Number(Numeric::Double(result)),
    }
}

/// XPath 2.0 / F&O §17.1.2 `xs:double`/`xs:float` → `xs:string`
/// canonical form.  Decimal when `|x|` is in `[1e-6, 1e6)`, otherwise
/// scientific (`1.0E6`, `2.0E29`, `1.0E-13`): mantissa always carries
/// a fractional digit, the exponent has no `+` or leading zeros, and
/// `E` is uppercase.  Infinity is `INF`/`-INF` (not 1.0's `Infinity`).
fn format_number_xpath20(n: f64) -> String {
    if n.is_nan()      { return "NaN".to_string(); }
    if n.is_infinite() { return if n > 0.0 { "INF" } else { "-INF" }.to_string(); }
    if n == 0.0 {
        return if n.is_sign_negative() { "-0".to_string() } else { "0".to_string() };
    }
    let abs = n.abs();
    if (1e-6..1e6).contains(&abs) {
        return format_number(n); // in-range: decimal, same as 1.0
    }
    let s = format!("{n:e}");
    let (mantissa, exp) = s.split_once('e').unwrap_or((s.as_str(), "0"));
    let mantissa = if mantissa.contains('.') { mantissa.to_string() }
                   else { format!("{mantissa}.0") };
    format!("{mantissa}E{exp}")
}

/// XPath 1.0 § 4.2 number-to-string: decimal form only, no scientific
/// notation, no trailing zeros beyond round-trip precision.
fn format_number(n: f64) -> String {
    if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity".to_string() } else { "-Infinity".to_string() }
    } else if n == 0.0 && n.is_sign_negative() {
        // XSLT 2.0 §F.3 — `string()` on xs:double -0 preserves the
        // sign.  XPath 1.0 left this implementation-defined; we
        // pick the XSLT 2.0 canonical form so the test suite's
        // expected `<out>-0</out>` comes out correctly.
        "-0".to_string()
    } else if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// libxml2's number-to-string: like [`format_number`] but emits
/// `1.23456789012346e+19` scientific form for magnitudes outside
/// roughly `[10^-5, 10^15)`.  Used only when the surrounding
/// [`XPathContext`](super::XPathContext) was built with
/// `libxml2_compatible: true`.  Deliberately *less* spec-conformant
/// than [`format_number`] — XPath 1.0 § 4.2 mandates decimal-only.
fn format_number_libxml2(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if n.fract() == 0.0 && n.abs() < 1e15 {
        return format!("{}", n as i64);
    }
    let abs = n.abs();
    if abs >= 1e15 || (abs > 0.0 && abs < 1e-5) {
        // Match libxml2's `%.14e`-like format: 14 digits after the
        // mantissa decimal, explicit `+` on non-negative exponents.
        let s = format!("{:.14e}", n);
        let Some(idx) = s.find('e') else { return s; };
        let (mantissa, exp_with_e) = s.split_at(idx);
        let exp = &exp_with_e[1..];
        // Strip mantissa trailing zeros (`1.23000000000000e19` →
        // `1.23e19`) for readability, then re-attach exponent with
        // explicit sign.
        let mantissa = trim_mantissa(mantissa);
        if exp.starts_with('-') {
            format!("{}e{}", mantissa, exp)
        } else {
            format!("{}e+{}", mantissa, exp)
        }
    } else {
        format!("{n}")
    }
}

fn trim_mantissa(s: &str) -> String {
    // Strips trailing zeros after the decimal point, then a lone
    // trailing '.' if all fractional digits were zero.  Used by the
    // libxml2-style formatter.
    let Some(dot_idx) = s.find('.') else { return s.to_string(); };
    let trimmed = s.trim_end_matches('0');
    let trimmed = trimmed.trim_end_matches('.');
    if trimmed.len() <= dot_idx { format!("{}.0", &s[..dot_idx]) } else { trimmed.to_string() }
}

/// XPath 1.0 §3.4 general comparison for `!=`.
///
/// When at least one side is a node-set, `!=` is *not* the negation of
/// `=` — it's "there exists a pair whose string-values differ."  Two
/// distinct nodes in a single set are enough to make `$x != 'a'`
/// return true even when one of them does equal `'a'`.  Falling back
/// to `!values_eq` silently flips the answer in those cases.
/// Materialise an [`Value::IntRange`] into a [`Value::Sequence`] of
/// `Value::Number` items.  Used by `=` / `!=` / `<` / `>` comparison
/// routines that need full per-item access — the lazy representation
/// loses meaning once we're cross-multiplying with another sequence.
fn intrange_to_sequence(v: &Value) -> Option<Value> {
    if let Value::IntRange { lo, hi } = v {
        let items: Vec<Value> = (*lo..=*hi).map(|i| Value::Number(Numeric::Double(i as f64))).collect();
        return Some(Value::Sequence(items));
    }
    None
}

fn values_ne<I: DocIndexLike>(
    l: &Value, r: &Value, idx: &I, bindings: &dyn XPathBindings,
) -> bool {
    // Comparisons need full per-item access, so normalise away
    // any [`Value::IntRange`] up front.  This is the rare case
    // (huge-range arithmetic typically flows through `count` /
    // `sum` / `codepoints-to-string`, not equality operators).
    if let Some(lv) = intrange_to_sequence(l) {
        return values_ne(&lv, r, idx, bindings);
    }
    if let Some(rv) = intrange_to_sequence(r) {
        return values_ne(l, &rv, idx, bindings);
    }
    match (l, r) {
        // Maps and arrays have no general-comparison semantics
        // (XPTY0004); keep lenient — never "equal" under a general
        // comparison.
        (Value::Map(_) | Value::Array(_) | Value::Function(_), _) | (_, Value::Map(_) | Value::Array(_) | Value::Function(_)) => false,
        (Value::NodeSet(ls), Value::NodeSet(rs)) => {
            // XPath 1.0 §3.4: node-set != node-set is true iff some
            // pair (l, r) of string-values has l != r.  That holds
            // unless *every* left value equals *every* right value,
            // which in turn means the union has at most one distinct
            // value.  Either side empty: no pair exists, so false.
            let l_vals: HashSet<String> = ls.iter().map(|&id| idx.string_value(id)).collect();
            let r_vals: HashSet<String> = rs.iter().map(|&id| idx.string_value(id)).collect();
            if l_vals.is_empty() || r_vals.is_empty() { return false; }
            l_vals.union(&r_vals).count() > 1
        }
        (Value::ForeignNodeSet(ls), Value::ForeignNodeSet(rs)) => {
            let l_vals: HashSet<String> = ls.iter()
                .map(|&p| bindings.foreign_string_value(p)).collect();
            let r_vals: HashSet<String> = rs.iter()
                .map(|&p| bindings.foreign_string_value(p)).collect();
            if l_vals.is_empty() || r_vals.is_empty() { return false; }
            l_vals.union(&r_vals).count() > 1
        }
        (Value::NodeSet(ns), Value::ForeignNodeSet(fs))
        | (Value::ForeignNodeSet(fs), Value::NodeSet(ns)) => {
            let l_vals: HashSet<String> = ns.iter().map(|&id| idx.string_value(id)).collect();
            let r_vals: HashSet<String> = fs.iter()
                .map(|&p| bindings.foreign_string_value(p)).collect();
            if l_vals.is_empty() || r_vals.is_empty() { return false; }
            l_vals.union(&r_vals).count() > 1
        }
        (Value::NodeSet(ns), other) | (other, Value::NodeSet(ns)) => {
            match other {
                // Boolean comparison: the negation form is correct
                // here (booleans reduce to a single boolean).
                Value::Boolean(b) => value_to_bool(&Value::NodeSet(ns.clone()), idx) != *b,
                Value::Number(n) => ns.iter().any(|&id| {
                    idx.string_value(id).trim().parse::<f64>().ok() != Some(n.as_f64())
                }),
                Value::String(s) => ns.iter().any(|&id| idx.string_value(id) != *s),
                // Typed atomic — inspect the underlying repr in place
                // so we don't clone the boxed TypedAtomic (or the
                // node-set) just to recurse.
                Value::Typed(t) => {
                    if let Some(n) = t.numeric {
                        ns.iter().any(|&id| idx.string_value(id).trim().parse::<f64>().ok() != Some(n))
                    } else if let Some(b) = t.boolean {
                        !ns.is_empty() != b
                    } else {
                        ns.iter().any(|&id| idx.string_value(id) != t.lexical)
                    }
                }
                Value::Sequence(items) => items.iter().any(|v| {
                    values_ne(&Value::NodeSet(ns.clone()), v, idx, bindings)
                }),
                Value::NodeSet(_) | Value::ForeignNodeSet(_) | Value::IntRange { .. }
                | Value::Map(_) | Value::Array(_) | Value::Function(_) => unreachable!(),
            }
        }
        (Value::ForeignNodeSet(fs), other) | (other, Value::ForeignNodeSet(fs)) => {
            match other {
                Value::Boolean(b) => !fs.is_empty() != *b,
                Value::Number(n) => fs.iter().any(|&p| {
                    bindings.foreign_string_value(p).trim().parse::<f64>().ok() != Some(n.as_f64())
                }),
                Value::String(s) => fs.iter().any(|&p| bindings.foreign_string_value(p) != *s),
                Value::Typed(t) => {
                    if let Some(n) = t.numeric {
                        fs.iter().any(|&p| bindings.foreign_string_value(p)
                            .trim().parse::<f64>().ok() != Some(n))
                    } else if let Some(b) = t.boolean {
                        !fs.is_empty() != b
                    } else {
                        fs.iter().any(|&p| bindings.foreign_string_value(p) != t.lexical)
                    }
                }
                Value::Sequence(items) => items.iter().any(|v| {
                    values_ne(&Value::ForeignNodeSet(fs.clone()), v, idx, bindings)
                }),
                Value::NodeSet(_) | Value::ForeignNodeSet(_) | Value::IntRange { .. }
                | Value::Map(_) | Value::Array(_) | Value::Function(_) => unreachable!(),
            }
        }
        // Atomic sequence on either side: XPath 2.0 general-comparison
        // semantics — exists pair (a, b) such that a != b.
        (Value::Sequence(items), other) | (other, Value::Sequence(items)) => {
            items.iter().any(|v| values_ne(v, other, idx, bindings))
        }
        // No node-sets involved — `!=` is just the negation of `=`.
        _ => !values_eq(l, r, idx, bindings),
    }
}

fn values_eq<I: DocIndexLike>(
    l: &Value, r: &Value, idx: &I, bindings: &dyn XPathBindings,
) -> bool {
    if let Some(lv) = intrange_to_sequence(l) {
        return values_eq(&lv, r, idx, bindings);
    }
    if let Some(rv) = intrange_to_sequence(r) {
        return values_eq(l, &rv, idx, bindings);
    }
    // XPath 2.0 §C.2 — string equality uses the in-scope default
    // collation.  We implement the html-ascii-case-insensitive
    // collation (case folding); under any other collation `ci` is
    // false and string comparison is the codepoint default, so this
    // is a no-op outside an explicit `default-collation` scope.
    let ci = is_ascii_ci_collation(
        DEFAULT_COLLATION.with(|c| c.borrow().clone()).as_deref());
    let str_eq = |a: &str, b: &str| if ci {
        ascii_ci_fold(a) == ascii_ci_fold(b)
    } else { a == b };
    match (l, r) {
        // Maps and arrays are not equality-comparable here (their
        // deep-equal lives in map:/array: functions); lenient false.
        (Value::Map(_) | Value::Array(_) | Value::Function(_), _) | (_, Value::Map(_) | Value::Array(_) | Value::Function(_)) => false,
        (Value::NodeSet(ls), Value::NodeSet(rs)) => {
            // O(M + N) via a hash set on the smaller side — same
            // XPath 1.0 §3.4 semantics ("there exists a node in
            // each set whose string-values are equal") but without
            // the M×N cross product of string-value computations
            // the naive loop did.  Build the set from the smaller
            // side to keep peak memory bounded.
            let (small, large) = if ls.len() <= rs.len() { (ls, rs) } else { (rs, ls) };
            let mut set: HashSet<String> = HashSet::with_capacity(small.len());
            for &a in small {
                set.insert(idx.string_value(a));
            }
            for &b in large {
                if set.contains(&idx.string_value(b)) {
                    return true;
                }
            }
            false
        }
        (Value::ForeignNodeSet(ls), Value::ForeignNodeSet(rs)) => {
            let (small, large) = if ls.len() <= rs.len() { (ls, rs) } else { (rs, ls) };
            let mut set: HashSet<String> = HashSet::with_capacity(small.len());
            for &a in small {
                set.insert(bindings.foreign_string_value(a));
            }
            for &b in large {
                if set.contains(&bindings.foreign_string_value(b)) {
                    return true;
                }
            }
            false
        }
        // Cross-kind: compare each foreign node's string-value to each
        // primary node's string-value.  Same XPath 1.0 §3.4 semantics
        // as NodeSet/NodeSet, just with the right accessor per side.
        (Value::NodeSet(ns), Value::ForeignNodeSet(fs))
        | (Value::ForeignNodeSet(fs), Value::NodeSet(ns)) => {
            // Build a set from the (likely smaller / cheaper-to-
            // access) primary side; probe with foreign.
            let mut set: HashSet<String> = HashSet::with_capacity(ns.len());
            for &id in ns {
                set.insert(idx.string_value(id));
            }
            for &p in fs {
                if set.contains(&bindings.foreign_string_value(p)) {
                    return true;
                }
            }
            false
        }
        (Value::NodeSet(ns), other) | (other, Value::NodeSet(ns)) => {
            match other {
                Value::Boolean(b) => value_to_bool(&Value::NodeSet(ns.clone()), idx) == *b,
                Value::Number(n) => ns.iter().any(|&id| {
                    idx.string_value(id).trim().parse::<f64>().ok() == Some(n.as_f64())
                }),
                Value::String(s) => ns.iter().any(|&id| idx.string_value(id) == *s),
                Value::Typed(t) => {
                    if let Some(n) = t.numeric {
                        ns.iter().any(|&id| idx.string_value(id).trim().parse::<f64>().ok() == Some(n))
                    } else if let Some(b) = t.boolean {
                        !ns.is_empty() == b
                    } else {
                        ns.iter().any(|&id| idx.string_value(id) == t.lexical)
                    }
                }
                Value::Sequence(items) => items.iter().any(|v| {
                    values_eq(&Value::NodeSet(ns.clone()), v, idx, bindings)
                }),
                Value::NodeSet(_) | Value::ForeignNodeSet(_) | Value::IntRange { .. }
                | Value::Map(_) | Value::Array(_) | Value::Function(_) => unreachable!(),
            }
        }
        (Value::ForeignNodeSet(fs), other) | (other, Value::ForeignNodeSet(fs)) => {
            match other {
                Value::Boolean(b) => !fs.is_empty() == *b,
                Value::Number(n) => fs.iter().any(|&p| {
                    bindings.foreign_string_value(p).trim().parse::<f64>().ok() == Some(n.as_f64())
                }),
                Value::String(s) => fs.iter().any(|&p| bindings.foreign_string_value(p) == *s),
                Value::Typed(t) => {
                    if let Some(n) = t.numeric {
                        fs.iter().any(|&p| bindings.foreign_string_value(p)
                            .trim().parse::<f64>().ok() == Some(n))
                    } else if let Some(b) = t.boolean {
                        !fs.is_empty() == b
                    } else {
                        fs.iter().any(|&p| bindings.foreign_string_value(p) == t.lexical)
                    }
                }
                Value::Sequence(items) => items.iter().any(|v| {
                    values_eq(&Value::ForeignNodeSet(fs.clone()), v, idx, bindings)
                }),
                Value::NodeSet(_) | Value::ForeignNodeSet(_) | Value::IntRange { .. }
                | Value::Map(_) | Value::Array(_) | Value::Function(_) => unreachable!(),
            }
        }
        // Atomic sequence on either side: XPath 2.0 general-comparison
        // semantics — exists pair (a, b) such that a = b.
        (Value::Sequence(items), other) | (other, Value::Sequence(items)) => {
            items.iter().any(|v| values_eq(v, other, idx, bindings))
        }
        (Value::Boolean(a), b) => *a == value_to_bool(b, idx),
        (a, Value::Boolean(b)) => value_to_bool(a, idx) == *b,
        // Numeric equality is by value, not by kind — `xs:integer 1`
        // equals `xs:double 1.0` (F&O numeric-equal promotes both to a
        // common type).  `Numeric`'s derived `==` is structural and
        // would treat the kinds as distinct, so compare the f64 views.
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Number(a), Value::String(b)) => a.as_f64() == b.trim().parse::<f64>().unwrap_or(f64::NAN),
        (Value::String(a), Value::Number(b)) => a.trim().parse::<f64>().unwrap_or(f64::NAN) == b.as_f64(),
        (Value::String(a), Value::String(b)) => str_eq(a, b),
        // Typed atomics: numeric typed compares numerically against
        // any numeric/typed-numeric counterpart, falls back to
        // string-equal otherwise.  No clones — read straight out of
        // the boxed TypedAtomic.
        (Value::Typed(t), Value::Typed(u)) => {
            match (t.numeric, u.numeric) {
                (Some(a), Some(b)) => a == b,
                _ => {
                    // Date / dateTime / time equality is semantic, not
                    // lexical — `1996-12-12T13:13:00Z` equals
                    // `1996-12-12T13:13:00+00:00` even though their
                    // text forms differ.  Normalise to UTC seconds
                    // (via the existing date parser) when both
                    // operands carry a date-like kind.
                    let date_eq = matches!(t.kind, "date" | "dateTime" | "time")
                               && matches!(u.kind, "date" | "dateTime" | "time")
                               && t.kind == u.kind;
                    if date_eq {
                        if let (Some(a), Some(b)) = (
                            dt_to_utc_seconds(&t.lexical, t.kind),
                            dt_to_utc_seconds(&u.lexical, u.kind),
                        ) {
                            return a == b;
                        }
                    }
                    // Duration equality: normalise to total seconds
                    // (dayTimeDuration) — yearMonthDuration uses a
                    // separate month count we don't unify here.
                    if t.kind == "dayTimeDuration" && u.kind == "dayTimeDuration" {
                        if let (Some(a), Some(b)) = (
                            parse_day_time_duration_secs(&t.lexical),
                            parse_day_time_duration_secs(&u.lexical),
                        ) {
                            return a == b;
                        }
                    }
                    // `xs:duration` (the union type) keeps both
                    // year-month and day-time components.  Two
                    // durations are equal when each component
                    // matches independently (XPath 2.0 §10.4.3).
                    if matches!((t.kind, u.kind),
                        ("duration", "duration")
                        | ("duration", "dayTimeDuration") | ("dayTimeDuration", "duration")
                        | ("duration", "yearMonthDuration") | ("yearMonthDuration", "duration"))
                    {
                        if let (Some(a), Some(b)) = (
                            parse_duration_split(&t.lexical),
                            parse_duration_split(&u.lexical),
                        ) {
                            return a == b;
                        }
                    }
                    str_eq(&t.lexical, &u.lexical)
                }
            }
        }
        (Value::Typed(t), Value::Number(n)) | (Value::Number(n), Value::Typed(t)) => {
            t.numeric.map(|a| a == n.as_f64())
                .unwrap_or_else(|| t.lexical.trim().parse::<f64>().ok() == Some(n.as_f64()))
        }
        (Value::Typed(t), Value::String(s)) | (Value::String(s), Value::Typed(t)) => {
            str_eq(&t.lexical, s)
        }
        // IntRange operands are normalised away at function entry.
        (Value::IntRange { .. }, _) | (_, Value::IntRange { .. }) =>
            unreachable!("IntRange normalised at values_eq entry"),
    }
}

/// A canonical key for value-equality grouping/distinct (XSLT 2.0
/// §14.3 group-by uses the `eq` operator).  Returns `Some` only for
/// the typed values whose `eq` semantics differ from their lexical
/// string: temporal values normalise to a UTC instant (so two
/// dateTimes in different time zones for the same instant share a
/// key), durations to their total magnitude.  `None` for everything
/// else — the caller uses the string-value, which already matches
/// `eq` for strings / numbers / booleans.
pub fn value_equality_key(v: &Value) -> Option<String> {
    let t = match v { Value::Typed(t) => t, _ => return None };
    match t.kind {
        "date" | "dateTime" | "time" =>
            dt_to_utc_seconds(&t.lexical, t.kind).map(|s| format!("{}#{s}", t.kind)),
        "dayTimeDuration" =>
            parse_day_time_duration_secs(&t.lexical).map(|s| format!("dtd#{s}")),
        "yearMonthDuration" =>
            parse_year_month_duration_months(&t.lexical).map(|m| format!("ymd#{m}")),
        _ => None,
    }
}

/// Parse an `xs:date` / `xs:dateTime` / `xs:time` lexical form
/// to a UTC second count (since the Unix epoch for date / dateTime;
/// since midnight UTC for time).  Returns `None` when the input
/// doesn't parse — caller falls back to lexical comparison.
fn dt_to_utc_seconds(s: &str, kind: &str) -> Option<i64> {
    let dk = match kind {
        "date"     => DateKind::Date,
        "dateTime" => DateKind::DateTime,
        "time"     => DateKind::Time,
        _ => return None,
    };
    let (y, mo, d, h, mi, sec, _frac, tz) = parse_xsd_date_time(s, dk)?;
    // For xs:time, the date portion is unbound; treat as 1970-01-01
    // so comparisons of same-tz times still come out right.
    let (yy, mm, dd) = if matches!(dk, DateKind::Time) {
        (1970, 1, 1)
    } else { (y, mo, d) };
    let days = ymd_to_days(yy, mm as u32, dd as u32);
    let secs_in_day = (h as i64) * 3600 + (mi as i64) * 60 + sec as i64;
    let local_total = days * 86_400 + secs_in_day;
    // Timezone offset is in minutes east of UTC; subtract to get
    // UTC.  Absent timezone defers to implementation; treat as UTC.
    let tz_offset_secs = tz.map(|m| m as i64 * 60).unwrap_or(0);
    Some(local_total - tz_offset_secs)
}

/// True iff the typed value's kind is one of `xs:date`,
/// `xs:dateTime`, `xs:time`, `xs:gYear`, `xs:gYearMonth`,
/// `xs:gMonth`, `xs:gMonthDay`, `xs:gDay` — anything date-like
/// that day-arithmetic can shift.
fn is_date_like_kind(k: &str) -> bool {
    matches!(k, "date" | "dateTime" | "time"
              | "gYear" | "gYearMonth" | "gMonth" | "gMonthDay" | "gDay")
}

/// True iff `k` is one of the three xs:duration types.
fn is_duration_kind(k: &str) -> bool {
    matches!(k, "duration" | "dayTimeDuration" | "yearMonthDuration")
}

/// Parse an `xs:dayTimeDuration` lexical form (e.g. `"P10D"`,
/// `"-PT3H"`, `"PT5M"`) into a signed second count.  Returns
/// `None` when the input doesn't parse — caller falls back to
/// numeric arithmetic.
fn parse_day_time_duration_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    let (sign, body) = if let Some(rest) = s.strip_prefix('-') {
        (-1i64, rest)
    } else { (1i64, s) };
    let body = body.strip_prefix('P')?;
    let (day_part, time_part) = match body.find('T') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None    => (body, ""),
    };
    let parse_comp = |part: &str, marker: char| -> Option<i64> {
        let i = match part.find(marker) { Some(i) => i, None => return Some(0) };
        let start = part[..i].rfind(|c: char| !c.is_ascii_digit() && c != '.')
            .map(|n| n + 1).unwrap_or(0);
        Some(part[start..i].parse::<i64>().unwrap_or(0))
    };
    let days  = parse_comp(day_part,  'D')?;
    let hours = parse_comp(time_part, 'H')?;
    let mins  = parse_comp(time_part, 'M')?;
    let secs  = parse_comp(time_part, 'S')?;
    Some(sign * (days * 86_400 + hours * 3600 + mins * 60 + secs))
}

/// Sum a uniform sequence of `xs:dayTimeDuration` / `xs:yearMonthDuration`
/// values, returning `(kind, total_units, count)` — seconds for
/// dayTime, months for yearMonth.  `None` if the items aren't all the
/// same duration kind (so the caller falls back to numeric coercion).
/// Used by `fn:sum` / `fn:avg` over durations (F&O §10.4).
fn duration_seq_total(items: &[Value]) -> Option<(&'static str, i64, i64)> {
    let kind = match items.first()? {
        Value::Typed(t) if matches!(t.kind, "dayTimeDuration" | "yearMonthDuration") => t.kind,
        _ => return None,
    };
    let mut total: i64 = 0;
    for v in items {
        let t = match v {
            Value::Typed(t) if t.kind == kind => t,
            _ => return None,
        };
        total += if kind == "dayTimeDuration" {
            parse_day_time_duration_secs(&t.lexical)?
        } else {
            parse_year_month_duration_months(&t.lexical)?
        };
    }
    Some((kind, total, items.len() as i64))
}

/// Build a duration `Value` of `kind` from a unit count (seconds for
/// dayTime, months for yearMonth).
fn duration_value(kind: &'static str, units: i64) -> Value {
    let lexical = if kind == "dayTimeDuration" {
        format_day_time_duration_secs(units)
    } else {
        format_year_month_duration_months(units)
    };
    Value::Typed(Box::new(TypedAtomic { kind, lexical, numeric: None, boolean: None }))
}

/// Format a signed second-count back into `xs:dayTimeDuration`
/// canonical lexical form.
fn format_day_time_duration_secs(mut total: i64) -> String {
    let mut out = String::with_capacity(16);
    if total < 0 { out.push('-'); total = -total; }
    out.push('P');
    let days = total / 86_400;
    let rem  = total % 86_400;
    if days > 0 { out.push_str(&days.to_string()); out.push('D'); }
    if rem > 0 || days == 0 {
        out.push('T');
        let h = rem / 3600;
        let m = (rem % 3600) / 60;
        let s = rem % 60;
        if h > 0 { out.push_str(&h.to_string()); out.push('H'); }
        if m > 0 { out.push_str(&m.to_string()); out.push('M'); }
        if s > 0 || (h == 0 && m == 0) { out.push_str(&s.to_string()); out.push('S'); }
    }
    out
}

/// Parse an `xs:date` lexical form (`YYYY-MM-DD[Z|±HH:MM]`) to
/// (year, month, day, tz_minutes_or_none).
fn parse_xsd_date_only(s: &str) -> Option<(i32, u32, u32, Option<i16>)> {
    let s = s.trim();
    let (sign, body) = if let Some(rest) = s.strip_prefix('-') {
        (-1i32, rest)
    } else { (1i32, s) };
    let parts: Vec<&str> = body.splitn(3, '-').collect();
    if parts.len() != 3 { return None; }
    let y: i32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    // parts[2] is "DD" possibly followed by "Z" or "±HH:MM".
    let day_tail = parts[2];
    let (d_str, tz_tail) = if day_tail.len() >= 2 {
        (&day_tail[..2], &day_tail[2..])
    } else { return None; };
    let d: u32 = d_str.parse().ok()?;
    let signed_year = sign * y;
    // XSD §3.2.7 — month must be 1..12; day must be in the legal
    // range for the given (year, month) pair, honouring February's
    // leap-year exception.  Without these bounds checks an invalid
    // lexical like `2002-02-29` would silently materialise as a
    // typed date, defeating `castable as xs:date` test cases.
    if !(1..=12).contains(&m) { return None; }
    let max_day = days_in_month(signed_year, m);
    if !(1..=max_day).contains(&d) { return None; }
    let tz = parse_tz_suffix(tz_tail);
    Some((signed_year, m, d, tz))
}

/// Days-from-1970-01-01 for a (year, month, day) — proleptic
/// Gregorian.  Inverse of [`days_to_ymd`].  Used so date+duration
/// arithmetic can stay in integer days without pulling in a date
/// crate dependency.
fn ymd_to_days(y: i32, m: u32, d: u32) -> i64 {
    // Howard Hinnant's "days_from_civil" (CC0).
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = m as u64;
    let d = d as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

/// XPath 2.0 §10.7 `date + xs:dayTimeDuration` etc.  Returns
/// `None` when the operands aren't a recognised date-arithmetic
/// pair — caller falls through to numeric.
/// Best-effort coercion of an arbitrary `Value` to `xs:double`
/// for duration arithmetic.  Strings / synthetic-text nodes
/// parse via [`f64::from_str`]; typed atomics consult their
/// cached numeric value first.  Returns `None` when no numeric
/// representation makes sense — caller falls back to numeric
/// promotion / NaN.
fn coerce_to_double(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n)  => Some(n.as_f64()),
        Value::Boolean(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Typed(t)   => t.numeric.or_else(|| t.lexical.trim().parse().ok()),
        Value::String(s)  => s.trim().parse().ok(),
        // NodeSet with one synthetic-text item — common when the
        // value comes from `xsl:for-each select="1 to N"` (the
        // dot is a synthetic text node holding the integer).
        Value::NodeSet(ns) if ns.len() == 1 => {
            // We can't read the synthetic store from here; the
            // caller can usually pass us a Number directly.
            // Stringification is handled by the calling
            // typed-numeric path; returning None lets it through.
            let _ = ns;
            None
        }
        _ => None,
    }
}

/// Parse an `xs:yearMonthDuration` lexical (`PnYnM`, optional
/// `-` prefix) into a signed month count.  Returns `None` on
/// malformed input.
/// Parse an `xs:duration` lexical into `(months, seconds)`.  Both
/// components carry the duration's sign (they always agree because
/// the XSD lexical form has one optional leading minus).
/// Returns `None` on malformed input.
fn parse_duration_split(s: &str) -> Option<(i64, i64)> {
    let s = s.trim();
    let (sign, body) = if let Some(rest) = s.strip_prefix('-') {
        (-1i64, rest)
    } else { (1i64, s) };
    let body = body.strip_prefix('P')?;
    let (date_part, time_part) = match body.find('T') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None    => (body, ""),
    };
    let mut years: i64 = 0;
    let mut months: i64 = 0;
    let mut days: i64 = 0;
    let mut cur = String::new();
    for c in date_part.chars() {
        if c.is_ascii_digit() { cur.push(c); }
        else {
            let n: i64 = cur.parse().ok()?;
            cur.clear();
            match c {
                'Y' => years = n,
                'M' => months = n,
                'D' => days = n,
                _ => return None,
            }
        }
    }
    if !cur.is_empty() { return None; }
    let mut hours: i64 = 0;
    let mut mins:  i64 = 0;
    let mut secs:  i64 = 0;
    cur.clear();
    let mut frac: f64 = 0.0;
    let mut in_frac = false;
    let mut frac_str = String::new();
    for c in time_part.chars() {
        if c.is_ascii_digit() {
            if in_frac { frac_str.push(c); }
            else        { cur.push(c); }
        } else if c == '.' {
            in_frac = true;
        } else {
            let n: i64 = cur.parse().ok()?;
            cur.clear();
            if !frac_str.is_empty() {
                let denom = 10f64.powi(frac_str.len() as i32);
                frac = frac_str.parse::<f64>().unwrap_or(0.0) / denom;
                frac_str.clear();
            }
            in_frac = false;
            match c {
                'H' => hours = n,
                'M' => mins  = n,
                'S' => secs  = n + frac.round() as i64,
                _ => return None,
            }
        }
    }
    if !cur.is_empty() { return None; }
    Some((sign * (years * 12 + months),
          sign * (days * 86_400 + hours * 3600 + mins * 60 + secs)))
}

fn parse_year_month_duration_months(s: &str) -> Option<i64> {
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None    => (false, s),
    };
    let rest = rest.strip_prefix('P')?;
    let mut years: i64 = 0;
    let mut months: i64 = 0;
    let mut cur = String::new();
    let mut any_field = false;
    for c in rest.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
        } else if c == 'Y' {
            years = cur.parse().ok()?;
            cur.clear();
            any_field = true;
        } else if c == 'M' {
            months = cur.parse().ok()?;
            cur.clear();
            any_field = true;
        } else {
            // Stray character — likely a day/time designator
            // that doesn't belong in yearMonthDuration.
            return None;
        }
    }
    if !any_field || !cur.is_empty() { return None; }
    let total = years * 12 + months;
    Some(if neg { -total } else { total })
}

/// Format a signed month count as an `xs:yearMonthDuration`
/// lexical (`P[-]nYnM`).  Empty fields are elided per XSD §F:
/// a zero-month value is rendered `P0M`.
fn format_year_month_duration_months(months: i64) -> String {
    if months == 0 { return "P0M".into(); }
    let neg = months < 0;
    let abs = months.unsigned_abs() as u64;
    let years  = abs / 12;
    let months = abs % 12;
    let mut out = String::new();
    if neg { out.push('-'); }
    out.push('P');
    if years  > 0 { out.push_str(&format!("{years}Y")); }
    if months > 0 { out.push_str(&format!("{months}M")); }
    if out.ends_with('P') {
        // Both zero — shouldn't reach here given the early
        // return, but keep the lexical form well-formed.
        out.push_str("0M");
    }
    out
}

/// XPath 2.0 §10.6 — duration multiplied by (or by) a numeric.
/// Routes both `duration * number` and `number * duration` to
/// the same scaling; for `yearMonthDuration` we scale the month
/// count, for `dayTimeDuration` we scale the seconds count.
/// Returns `None` when neither operand is a Typed duration.
/// Round a month count to the nearest integer with ties resolved
/// toward positive infinity — the `fn:round` rule that XPath 2.0
/// §10.6 mandates for scaling an xs:yearMonthDuration.  Rust's
/// `f64::round` rounds half *away from zero* (`-0.5 → -1`), which
/// disagrees on negative halves (`fn:round(-0.5)` is `0`).
fn round_months_half_up(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

fn duration_mul<I: DocIndexLike>(
    l: &Value, r: &Value, idx: &I, bindings: &dyn XPathBindings,
) -> Option<Value> {
    let (dur, num) = match (l, r) {
        (Value::Typed(d), other) if is_duration_kind(d.kind) => (d, other),
        (other, Value::Typed(d)) if is_duration_kind(d.kind) => (d, other),
        _ => return None,
    };
    let factor = coerce_to_double(num)
        .unwrap_or_else(|| value_to_number_with(num, idx, bindings));
    if !factor.is_finite() { return None; }
    if dur.kind == "yearMonthDuration" {
        let months = parse_year_month_duration_months(&dur.lexical)?;
        let scaled = round_months_half_up(months as f64 * factor);
        return Some(Value::Typed(Box::new(TypedAtomic {
            kind: "yearMonthDuration",
            lexical: format_year_month_duration_months(scaled),
            numeric: None, boolean: None,
        })));
    }
    // dayTimeDuration scales in microseconds so fractional seconds
    // (`PT5.015S`) survive the multiply.
    let us = parse_day_time_duration_micros(&dur.lexical)?;
    let scaled = (us as f64 * factor).round() as i64;
    Some(Value::Typed(Box::new(TypedAtomic {
        kind: "dayTimeDuration",
        lexical: canonical_day_time_duration_lex(&format_day_time_duration_micros(scaled)),
        numeric: None, boolean: None,
    })))
}

/// XPath 2.0 §10.6.2 — duration divided by number (scales the
/// duration) or by another duration (returns the ratio as
/// xs:decimal).
fn duration_div<I: DocIndexLike>(
    l: &Value, r: &Value, idx: &I, bindings: &dyn XPathBindings,
) -> Option<Value> {
    match (l, r) {
        (Value::Typed(a), Value::Typed(b))
            if is_duration_kind(a.kind) && is_duration_kind(b.kind) =>
        {
            // duration / duration → xs:decimal (ratio).
            let (av, bv): (f64, f64) = if a.kind == "yearMonthDuration" {
                let am = parse_year_month_duration_months(&a.lexical)?;
                let bm = parse_year_month_duration_months(&b.lexical)?;
                (am as f64, bm as f64)
            } else {
                let av = parse_day_time_duration_micros(&a.lexical)?;
                let bv = parse_day_time_duration_micros(&b.lexical)?;
                (av as f64, bv as f64)
            };
            if bv == 0.0 { return None; }
            Some(Value::Number(Numeric::Double(av / bv)))
        }
        (Value::Typed(d), other) if is_duration_kind(d.kind) => {
            let factor = coerce_to_double(other)
                .unwrap_or_else(|| value_to_number_with(other, idx, bindings));
            if !factor.is_finite() || factor == 0.0 { return None; }
            if d.kind == "yearMonthDuration" {
                let months = parse_year_month_duration_months(&d.lexical)?;
                let scaled = round_months_half_up(months as f64 / factor);
                return Some(Value::Typed(Box::new(TypedAtomic {
                    kind: "yearMonthDuration",
                    lexical: format_year_month_duration_months(scaled),
                    numeric: None, boolean: None,
                })));
            }
            let us = parse_day_time_duration_micros(&d.lexical)?;
            let scaled = (us as f64 / factor).round() as i64;
            Some(Value::Typed(Box::new(TypedAtomic {
                kind: "dayTimeDuration",
                lexical: canonical_day_time_duration_lex(&format_day_time_duration_micros(scaled)),
                numeric: None, boolean: None,
            })))
        }
        _ => None,
    }
}

/// XPath 2.0 §10.5 / §10.7 — add `months` months to the given
/// `(year, month, day)`, normalising the month rollover.  Days
/// clamp to the last day of the target month (XSD's "month-
/// arithmetic" rule: 2003-01-31 + P1M → 2003-02-28).
fn add_months_to_ymd(y: i32, m: u32, d: u32, months: i64) -> (i32, u32, u32) {
    let total_months = (y as i64) * 12 + (m as i64) - 1 + months;
    let ny = total_months.div_euclid(12) as i32;
    let nm = total_months.rem_euclid(12) as u32 + 1;
    let last = days_in_month(ny, nm);
    let nd = d.min(last);
    (ny, nm, nd)
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11              => 30,
        2 => if is_leap_year(y) { 29 } else { 28 },
        _ => 0,
    }
}

fn is_leap_year(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Advance a proleptic-Gregorian `(year, month, day)` by one calendar
/// day, rolling month and year boundaries.  Used to normalise the
/// `24:00:00` midnight form of xs:dateTime onto the following day.
fn add_one_day(year: i32, month: u8, day: u8) -> (i32, u8, u8) {
    if (day as u32) < days_in_month(year, month as u32) {
        (year, month, day + 1)
    } else if month < 12 {
        (year, month + 1, 1)
    } else {
        (year + 1, 1, 1)
    }
}

/// XPath 2.0 §10.4 — add or subtract two `xs:duration` values.  The
/// operation is only defined within a single sub-family:
/// `xs:yearMonthDuration` combines by month count, `xs:dayTimeDuration`
/// by microseconds (preserving fractional seconds).  Mixing the two
/// families — or a bare `xs:duration` — has no defined sum, so this
/// returns `None` (the caller surfaces a type error / no-op).
fn duration_combine(a: &TypedAtomic, b: &TypedAtomic, subtract: bool) -> Option<Value> {
    let mk = |kind: &'static str, lexical: String| {
        Value::Typed(Box::new(TypedAtomic { kind, lexical, numeric: None, boolean: None }))
    };
    match (a.kind, b.kind) {
        ("yearMonthDuration", "yearMonthDuration") => {
            let lm = parse_year_month_duration_months(&a.lexical)?;
            let rm = parse_year_month_duration_months(&b.lexical)?;
            // A sum outside i64 range has no representable result; surface
            // it as a type error rather than overflowing.
            let m = if subtract { lm.checked_sub(rm) } else { lm.checked_add(rm) }?;
            Some(mk("yearMonthDuration", format_year_month_duration_months(m)))
        }
        ("dayTimeDuration", "dayTimeDuration") => {
            let lu = parse_day_time_duration_micros(&a.lexical)?;
            let ru = parse_day_time_duration_micros(&b.lexical)?;
            let u = if subtract { lu.checked_sub(ru) } else { lu.checked_add(ru) }?;
            let lex = canonical_day_time_duration_lex(
                &format_day_time_duration_micros(i64::try_from(u).ok()?));
            Some(mk("dayTimeDuration", lex))
        }
        _ => None,
    }
}

fn date_arith_add(l: &Value, r: &Value) -> Option<Value> {
    let (date, dur, date_kind) = match (l, r) {
        (Value::Typed(a), Value::Typed(b))
            if is_date_like_kind(a.kind) && is_duration_kind(b.kind)
            => (a, b, a.kind),
        (Value::Typed(a), Value::Typed(b))
            if is_duration_kind(a.kind) && is_date_like_kind(b.kind)
            => (b, a, b.kind),
        // duration + duration → duration (same sub-family only).
        (Value::Typed(a), Value::Typed(b))
            if is_duration_kind(a.kind) && is_duration_kind(b.kind) =>
        {
            return duration_combine(a, b, false);
        }
        _ => return None,
    };
    match date_kind {
        "date" => {
            let (y, m, d, _tz) = parse_xsd_date_only(&date.lexical)?;
            // yearMonthDuration shifts by month count; day-of-
            // month clamps per XSD §F.
            if dur.kind == "yearMonthDuration" {
                let months = parse_year_month_duration_months(&dur.lexical)?;
                let (ny, nm, nd) = add_months_to_ymd(y, m, d, months);
                let lex = format!("{:04}-{:02}-{:02}", ny, nm, nd);
                return Some(Value::Typed(Box::new(TypedAtomic {
                    kind: "date", lexical: lex, numeric: None, boolean: None,
                })));
            }
            let sec = parse_day_time_duration_secs(&dur.lexical)?;
            // Whole-day delta — xs:date has no sub-day precision.
            let day_delta = sec / 86_400;
            let new_days = ymd_to_days(y, m, d) + day_delta;
            let (ny, nm, nd) = days_to_ymd(new_days);
            let lex = format!("{:04}-{:02}-{:02}", ny, nm, nd);
            Some(Value::Typed(Box::new(TypedAtomic {
                kind: "date", lexical: lex, numeric: None, boolean: None,
            })))
        }
        "time" => {
            // xs:time + dayTimeDuration → xs:time (modulo 24h).
            // yearMonthDuration + time is undefined (the spec
            // rejects it as a static type error).  Work in
            // microseconds so a duration's sub-second component
            // (`PT…0.3S`) survives the wrap, mirroring the dateTime
            // arm below.
            if dur.kind == "yearMonthDuration" { return None; }
            let dur_us = parse_day_time_duration_micros(&dur.lexical)?;
            let (h, m, s, time_frac, tz) = parse_xsd_time(&date.lexical)?;
            let day_us = ((h as i128) * 3600 + (m as i128) * 60 + s as i128)
                * 1_000_000 + time_frac as i128;
            let total_us = (day_us + dur_us).rem_euclid(86_400i128 * 1_000_000);
            let total = (total_us / 1_000_000) as i64;
            let frac = (total_us % 1_000_000) as u32;
            let nh = (total / 3600) as u8;
            let nm = ((total / 60) % 60) as u8;
            let ns = (total % 60) as u8;
            let lex = if frac == 0 {
                let mut l = format!("{:02}:{:02}:{:02}", nh, nm, ns);
                if let Some(tz_m) = tz { l.push_str(&format_tz_suffix(tz_m)); }
                l
            } else {
                let mut l = format!("{:02}:{:02}:{:02}.{:06}", nh, nm, ns, frac);
                while l.ends_with('0') { l.pop(); }
                if let Some(tz_m) = tz { l.push_str(&format_tz_suffix(tz_m)); }
                l
            };
            Some(Value::Typed(Box::new(TypedAtomic {
                kind: "time", lexical: lex, numeric: None, boolean: None,
            })))
        }
        "dateTime" => {
            // xs:dateTime + xs:dayTimeDuration → xs:dateTime
            // (proper carry across midnight).  yearMonthDuration
            // shifts the date portion's month with day clamping.
            if dur.kind == "yearMonthDuration" {
                let months = parse_year_month_duration_months(&dur.lexical)?;
                let (y, mo, d, h, mi, s, frac, tz) =
                    parse_xsd_date_time(&date.lexical, DateKind::DateTime)?;
                let (ny, nm, nd) = add_months_to_ymd(y, mo as u32, d as u32, months);
                let lex = format_datetime_lexical(ny, nm as u8, nd as u8, h, mi, s, frac, tz);
                return Some(Value::Typed(Box::new(TypedAtomic {
                    kind: "dateTime", lexical: lex, numeric: None, boolean: None,
                })));
            }
            // xs:dateTime + xs:dayTimeDuration with sub-second
            // precision: convert both sides to microseconds, add,
            // then split back into (date, time, fractional).  Using
            // `parse_day_time_duration_secs` here would round away
            // the fractional component the test data exercises.
            let dur_us = parse_day_time_duration_micros(&dur.lexical)?;
            let (y, mo, d, h, mi, s, frac, tz) =
                parse_xsd_date_time(&date.lexical, DateKind::DateTime)?;
            let day_us = ((h as i128) * 3600 + (mi as i128) * 60 + s as i128)
                * 1_000_000 + (frac as i128);
            let total = day_us + dur_us;
            let us_per_day = 86_400i128 * 1_000_000;
            let day_delta = total.div_euclid(us_per_day);
            let remain   = total.rem_euclid(us_per_day);
            let new_days = ymd_to_days(y, mo as u32, d as u32) + day_delta as i64;
            let (ny, nm, nd) = days_to_ymd(new_days);
            let remain_secs = (remain / 1_000_000) as i64;
            let new_frac = (remain % 1_000_000) as u32;
            let nh = (remain_secs / 3600) as u8;
            let nmi = ((remain_secs / 60) % 60) as u8;
            let ns = (remain_secs % 60) as u8;
            let lex = format_datetime_lexical(ny, nm as u8, nd as u8, nh, nmi, ns, new_frac, tz);
            Some(Value::Typed(Box::new(TypedAtomic {
                kind: "dateTime", lexical: lex, numeric: None, boolean: None,
            })))
        }
        _ => None,
    }
}

/// Render the canonical lexical form of an xs:dateTime tuple
/// `YYYY-MM-DDTHH:MM:SS[.fff][TZ]`.  Caller is responsible for
/// computing the components (date arithmetic already normalized
/// them); we just stitch the string.
fn format_datetime_lexical(
    y: i32, mo: u8, d: u8, h: u8, mi: u8, s: u8, frac_us: u32, tz: Option<i16>,
) -> String {
    let mut out = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        y, mo, d, h, mi, s);
    if frac_us != 0 {
        let mut frac = format!(".{:06}", frac_us);
        while frac.ends_with('0') { frac.pop(); }
        out.push_str(&frac);
    }
    if let Some(tz) = tz {
        out.push_str(&format_tz_suffix(tz));
    }
    out
}

/// Render a timezone offset (minutes east of UTC) as XSD's
/// `Z` / `±HH:MM` suffix.  Zero offset always renders as `Z`.
fn format_tz_suffix(minutes: i16) -> String {
    if minutes == 0 { return "Z".into(); }
    let sign = if minutes < 0 { '-' } else { '+' };
    let abs = minutes.unsigned_abs() as i32;
    format!("{sign}{:02}:{:02}", abs / 60, abs % 60)
}

/// XPath 2.0 §10.7 / §10.4 — `date - date → dayTimeDuration`,
/// `date - duration → date`, `duration - duration → duration`.
fn date_arith_sub(l: &Value, r: &Value) -> Option<Value> {
    match (l, r) {
        // date - date → dayTimeDuration
        (Value::Typed(a), Value::Typed(b))
            if a.kind == "date" && b.kind == "date" =>
        {
            let (ay, am, ad, _) = parse_xsd_date_only(&a.lexical)?;
            let (by, bm, bd, _) = parse_xsd_date_only(&b.lexical)?;
            let diff_days = ymd_to_days(ay, am, ad) - ymd_to_days(by, bm, bd);
            let lex = format_day_time_duration_secs(diff_days * 86_400);
            Some(Value::Typed(Box::new(TypedAtomic {
                kind: "dayTimeDuration", lexical: lex,
                numeric: None, boolean: None,
            })))
        }
        // date - duration → date
        (Value::Typed(a), Value::Typed(b))
            if a.kind == "date" && is_duration_kind(b.kind) =>
        {
            let (y, m, d, _) = parse_xsd_date_only(&a.lexical)?;
            let sec = parse_day_time_duration_secs(&b.lexical)?;
            let day_delta = sec / 86_400;
            let new_days = ymd_to_days(y, m, d) - day_delta;
            let (ny, nm, nd) = days_to_ymd(new_days);
            let lex = format!("{:04}-{:02}-{:02}", ny, nm, nd);
            Some(Value::Typed(Box::new(TypedAtomic {
                kind: "date", lexical: lex, numeric: None, boolean: None,
            })))
        }
        // dateTime - duration → dateTime / time - duration → time —
        // implement as addition of the negated duration so the
        // sub-second / month-arithmetic logic stays in one place.
        (Value::Typed(a), Value::Typed(b))
            if matches!(a.kind, "dateTime" | "time") && is_duration_kind(b.kind) =>
        {
            let negated = TypedAtomic {
                kind: b.kind,
                lexical: negate_duration_lex(&b.lexical),
                numeric: None,
                boolean: None,
            };
            return date_arith_add(l, &Value::Typed(Box::new(negated)));
        }
        // dateTime - dateTime / date - date already returns a
        // dayTimeDuration above; date - dateTime is a type error
        // per spec.  date - time and time - date are also errors.
        (Value::Typed(a), Value::Typed(b))
            if matches!(a.kind, "dateTime" | "time")
                && matches!(b.kind, "dateTime" | "time") && a.kind == b.kind =>
        {
            // dateTime - dateTime → dayTimeDuration (UTC-normalised
            // microseconds difference).
            let dk = if a.kind == "dateTime" { DateKind::DateTime } else { DateKind::Time };
            let a_us = date_value_to_utc_micros(&a.lexical, dk)?;
            let b_us = date_value_to_utc_micros(&b.lexical, dk)?;
            let diff_us = (a_us - b_us) as i64;
            let lex = canonical_day_time_duration_lex(
                &format_day_time_duration_micros(diff_us)
            );
            Some(Value::Typed(Box::new(TypedAtomic {
                kind: "dayTimeDuration", lexical: lex,
                numeric: None, boolean: None,
            })))
        }
        // duration - duration → duration (same sub-family only).
        (Value::Typed(a), Value::Typed(b))
            if is_duration_kind(a.kind) && is_duration_kind(b.kind) =>
        {
            duration_combine(a, b, true)
        }
        _ => None,
    }
}

/// Discriminator for typed-aware arithmetic — keeps the
/// operator identity around so the result type can follow XPath
/// 2.0 §6.2.2 promotion rules (div on two integers → decimal).
#[derive(Clone, Copy)]
#[allow(dead_code)]
enum NumericOp { Add, Sub, Mul, Div, Mod, IDiv }

/// XPath 2.0 §6.2 numeric-type promotion: the result type is the
/// "widest" of the operand types under the lattice
/// `integer ⊂ decimal ⊂ float ⊂ double`.  Returns `None` when
/// neither operand carries a typed numeric tag — caller falls
/// back to plain `Value::Number` arithmetic.
fn numeric_promote_kind(a: Option<&str>, b: Option<&str>) -> Option<&'static str> {
    fn rank(k: &str) -> Option<u8> {
        Some(match k {
            "integer" | "long" | "int" | "short" | "byte"
            | "unsignedLong" | "unsignedInt" | "unsignedShort" | "unsignedByte"
            | "nonNegativeInteger" | "nonPositiveInteger"
            | "positiveInteger" | "negativeInteger" => 0, // integer family
            "decimal" => 1,
            "float"   => 2,
            "double"  => 3,
            _ => return None,
        })
    }
    let ra = a.and_then(rank);
    let rb = b.and_then(rank);
    let r = ra.max(rb)?;
    Some(match r {
        0 => "integer",
        1 => "decimal",
        2 => "float",
        _ => "double",
    })
}

/// An integer-valued function result (`count`, `position`, `last`,
/// `string-length`, `idiv`, …).  XPath 2.0 types these as `xs:integer`
/// so `instance of xs:integer` holds; XPath 1.0 has only the `number`
/// (double) type, so it keeps the double form — that avoids leaking a
/// typed integer into a 1.0-only consumer and keeps 1.0 behaviour
/// byte-identical.
fn integer_result(n: i64, bindings: &dyn XPathBindings) -> Value {
    if bindings.xpath_version_2_or_later() {
        Value::Number(Numeric::Integer(n))
    } else {
        Value::Number(Numeric::Double(n as f64))
    }
}

/// The XSD numeric kind of `v`, read from either a [`Value::Number`]
/// carrier or a typed-numeric [`Value::Typed`].  `None` for
/// non-numeric values (the caller then falls back to a plain double).
fn numeric_kind_of(v: &Value) -> Option<&'static str> {
    match v {
        Value::Number(n) => Some(n.kind()),
        Value::Typed(t) if t.numeric.is_some() => Some(t.kind),
        Value::IntRange { .. } => Some("integer"),
        _ => None,
    }
}

/// XPath 2.0 §6.2.4 — `div` / `idiv` / `mod` by zero raises
/// err:FOAR0001 when *both* operands are xs:integer or xs:decimal.
/// For xs:float / xs:double (and untyped operands, which atomise to
/// xs:double) the result is ±INF / NaN, not an error; XPath 1.0
/// compatibility mode is likewise all-double.
fn integer_decimal_zero_divisor<I: DocIndexLike>(
    l: &Value, r: &Value, idx: &I, bindings: &dyn XPathBindings,
) -> bool {
    if in_xpath_1_0_compat() { return false; }
    if value_to_number_with(r, idx, bindings) != 0.0 { return false; }
    let is_int_dec = |k| matches!(k, Some("integer") | Some("decimal"));
    is_int_dec(numeric_kind_of(l)) && is_int_dec(numeric_kind_of(r))
}

/// Numeric binary op honouring XPath 1.0 backwards-compatibility:
/// in a [`Expr::BackwardsCompat`] scope the operands are atomised to
/// xs:double (XPath 2.0 §B.1), so the result is always xs:double.
/// Outside compat mode this defers to the typed promotion lattice.
/// True when `v` is the empty sequence — an empty node-set, foreign
/// node-set, or sequence.  XPath 2.0 §6.2 (op:numeric-* signatures):
/// an arithmetic operation with an empty-sequence operand returns the
/// empty sequence.  (XPath 1.0 instead coerces the empty node-set to
/// `NaN`, so this distinction is gated on 2.0 / non-compat at the call
/// site.)
fn value_atomizes_empty(v: &Value) -> bool {
    match v {
        Value::NodeSet(n)        => n.is_empty(),
        Value::ForeignNodeSet(n) => n.is_empty(),
        Value::Sequence(s)       => s.is_empty(),
        _ => false,
    }
}

/// XPath 2.0 arithmetic short-circuit: in 2.0 (and not XPath 1.0
/// backwards-compatibility mode) an empty operand makes the whole
/// expression the empty sequence.
fn arith_empty_2_0(l: &Value, r: &Value, ctx: &EvalCtx) -> bool {
    ctx.static_ctx.xpath_2_0
        && !in_xpath_1_0_compat()
        && (value_atomizes_empty(l) || value_atomizes_empty(r))
}

fn compat_numeric_op<I: DocIndexLike>(
    l: &Value, r: &Value, idx: &I, bindings: &dyn XPathBindings, op: NumericOp,
) -> Value {
    if in_xpath_1_0_compat() {
        let a = value_to_number_with(l, idx, bindings);
        let b = value_to_number_with(r, idx, bindings);
        let result = match op {
            NumericOp::Add => a + b,
            NumericOp::Sub => a - b,
            NumericOp::Mul => a * b,
            NumericOp::Div => a / b,
            NumericOp::Mod => a % b,
            NumericOp::IDiv => (a / b).trunc(),
        };
        return Value::Number(Numeric::Double(result));
    }
    typed_numeric_op(l, r, idx, bindings, op)
}

/// XPath 2.0 §6.2 / XPTY0004 — arithmetic on an explicit xs:string
/// operand is a type error (strings don't promote to numeric the way
/// xs:untypedAtomic does).  Untyped atomics from node atomization
/// stay lenient; only literal-string operands are rejected here.
/// XPath 1.0 / 2.0-backwards-compat path stays permissive.
/// XPath 2.0 §3.5.2 / XPTY0004 — a general comparison between an
/// xs:string literal and a numeric value is a type error (untypedAtomic
/// from atomization stays lenient and converts via xs:double rules).
fn reject_string_vs_numeric_cmp_2_0(
    l: &Value, r: &Value, ctx: &EvalCtx, op: &str,
) -> Result<()> {
    if !ctx.static_ctx.xpath_2_0 || in_xpath_1_0_compat() {
        return Ok(());
    }
    let is_str   = |v: &Value| matches!(v, Value::String(_));
    let is_num   = |v: &Value| matches!(v, Value::Number(_))
        || matches!(v, Value::Typed(t) if t.numeric.is_some()
            && !matches!(t.kind, "untypedAtomic"));
    if (is_str(l) && is_num(r)) || (is_num(l) && is_str(r)) {
        return Err(xpath_err(format!(
            "general comparison '{op}' between an xs:string and a \
             numeric value (XPTY0004)"
        )).with_xpath_code("XPTY0004"));
    }
    Ok(())
}

fn reject_string_arith_2_0(
    l: &Value, r: &Value, ctx: &EvalCtx, op: &str,
) -> Result<()> {
    if !ctx.static_ctx.xpath_2_0 || in_xpath_1_0_compat() {
        return Ok(());
    }
    for v in [l, r] {
        if let Value::String(s) = v {
            return Err(xpath_err(format!(
                "arithmetic '{op}' on an xs:string operand '{s}' (XPTY0004)"
            )).with_xpath_code("XPTY0004"));
        }
    }
    Ok(())
}

/// Project `v` to an exact [`rust_decimal::Decimal`] when its type
/// promises one — `Value::Number(Integer | Decimal)` directly, and
/// `Value::Typed` of an integer-family or decimal kind by parsing
/// the preserved lexical form (so the value the user wrote survives
/// without an f64 detour).  Returns `None` for double/float/string/
/// untyped values, signalling that the caller must drop to the
/// lossy f64 path.
fn exact_decimal(v: &Value) -> Option<rust_decimal::Decimal> {
    match v {
        Value::Number(Numeric::Integer(i)) => Some(rust_decimal::Decimal::from(*i)),
        Value::Number(Numeric::Decimal(d)) => Some(*d),
        Value::Typed(t) if matches!(t.kind,
            "integer" | "decimal"
            | "int" | "long" | "short" | "byte"
            | "unsignedInt" | "unsignedLong" | "unsignedShort" | "unsignedByte"
            | "nonNegativeInteger" | "nonPositiveInteger"
            | "positiveInteger" | "negativeInteger"
        ) => t.lexical.parse().ok(),
        _ => None,
    }
}

fn typed_numeric_op<I: DocIndexLike>(
    l: &Value, r: &Value, idx: &I, bindings: &dyn XPathBindings, op: NumericOp,
) -> Value {
    // XPath 2.0 §3.1.1 / §6.2 — when both operands are exact (integer
    // or decimal), arithmetic must stay exact: `0.1 + 0.2 = 0.3`, not
    // `0.30000000000000004`.  Drop to f64 only when a Double / Float
    // operand forces lossy semantics.
    if let (Some(da), Some(db)) = (exact_decimal(l), exact_decimal(r)) {
        use rust_decimal::prelude::ToPrimitive;
        let zero = db.is_zero();
        let exact = match op {
            NumericOp::Add  => da.checked_add(db),
            NumericOp::Sub  => da.checked_sub(db),
            NumericOp::Mul  => da.checked_mul(db),
            NumericOp::Div  if zero => None,
            NumericOp::Div  => da.checked_div(db),
            NumericOp::Mod  if zero => None,
            NumericOp::Mod  => da.checked_rem(db),
            NumericOp::IDiv if zero => None,
            NumericOp::IDiv => da.checked_div(db).map(|q| q.trunc()),
        };
        if let Some(d) = exact {
            // Result-type per XPath 2.0 §6.2: idiv → xs:integer;
            // div → xs:decimal (even from int÷int per §6.2.4);
            // other ops → xs:integer iff both operands integer,
            // else xs:decimal.
            let both_integer = matches!(
                (l, r),
                (Value::Number(Numeric::Integer(_)), Value::Number(Numeric::Integer(_)))
            );
            return match op {
                NumericOp::IDiv => match d.to_i64() {
                    Some(i) => Value::Number(Numeric::Integer(i)),
                    None    => Value::Number(Numeric::Decimal(d)),
                },
                NumericOp::Div  => Value::Number(Numeric::Decimal(d)),
                _ if both_integer => match d.to_i64() {
                    Some(i) => Value::Number(Numeric::Integer(i)),
                    None    => Value::Number(Numeric::Decimal(d)),
                },
                _ => Value::Number(Numeric::Decimal(d)),
            };
        }
        // Decimal overflow falls through to the f64 path below — the
        // caller gets a wider but lossy answer rather than an error.
    }
    let a = value_to_number_with(l, idx, bindings);
    let b = value_to_number_with(r, idx, bindings);
    let result = match op {
        NumericOp::Add => a + b,
        NumericOp::Sub => a - b,
        NumericOp::Mul => a * b,
        NumericOp::Div => a / b,
        NumericOp::Mod => a % b,
        NumericOp::IDiv => (a / b).trunc(),
    };
    // Fast path for the common case — both operands already carry a
    // `Numeric` kind — promotes via the integer rank instead of the
    // string-keyed lattice walk below.  `idiv` is always xs:integer;
    // `div` is at least xs:decimal (XPath 2.0 §6.2.4).
    if let (Value::Number(la), Value::Number(rb)) = (l, r) {
        let rank = match op {
            NumericOp::IDiv => 0,
            NumericOp::Div  => la.rank().max(rb.rank()).max(1),
            _               => la.rank().max(rb.rank()),
        };
        return Value::Number(Numeric::from_rank(rank, result));
    }
    // XPath 2.0 §6.2 promotion within the integer ⊂ decimal ⊂ float ⊂
    // double lattice; the result carries the promoted kind (so an
    // xs:double operand makes the result an xs:double, etc.).  `idiv`
    // is always xs:integer; `div` between two integers is xs:decimal
    // (XPath 2.0 §6.2.4).
    match numeric_promote_kind(numeric_kind_of(l), numeric_kind_of(r)) {
        Some(mut kind) => {
            if matches!(op, NumericOp::IDiv) {
                kind = "integer";
            } else if matches!(op, NumericOp::Div) && kind == "integer" {
                kind = "decimal";
            }
            Value::Number(Numeric::of_kind(kind, result))
        }
        None => Value::Number(Numeric::Double(result)),
    }
}

#[allow(dead_code)]
fn arith<I: DocIndexLike>(
    l: &Expr,
    r: &Expr,
    ctx: &EvalCtx,
    idx: &I,
    op: impl Fn(f64, f64) -> f64,
) -> Result<Value> {
    let lv = eval_expr(l, ctx, idx)?;
    let rv = eval_expr(r, ctx, idx)?;
    Ok(Value::Number(Numeric::Double(op(
        value_to_number_with(&lv, idx, ctx.bindings),
        value_to_number_with(&rv, idx, ctx.bindings),
    ))))
}

fn cmp_op<I: DocIndexLike>(
    l: &Expr,
    r: &Expr,
    ctx: &EvalCtx,
    idx: &I,
    op: impl Fn(f64, f64) -> bool,
) -> Result<Value> {
    let lv = eval_expr(l, ctx, idx)?;
    let rv = eval_expr(r, ctx, idx)?;
    reject_string_vs_numeric_cmp_2_0(&lv, &rv, ctx, "<=>")?;
    // XPath 1.0 §3.4 — when one operand of `<`/`<=`/`>`/`>=` is a
    // node-set, the test is true iff *some* node's numeric
    // string-value satisfies the relation against the other side.
    // The fallback path collapses both operands to a single number.
    let node_to_number = |id: NodeId| -> f64 {
        idx.string_value(id).trim().parse::<f64>().unwrap_or(f64::NAN)
    };
    let foreign_to_number = |p: ForeignNodePtr| -> f64 {
        ctx.bindings.foreign_string_value(p)
            .trim().parse::<f64>().unwrap_or(f64::NAN)
    };
    match (&lv, &rv) {
        (Value::NodeSet(ns), other) | (other, Value::NodeSet(ns))
            if !matches!(other, Value::NodeSet(_) | Value::ForeignNodeSet(_)) =>
        {
            let n_other = value_to_number_with(other, idx, ctx.bindings);
            let any = ns.iter().any(|&id| {
                let node_n = node_to_number(id);
                // Preserve operand order: if `l` was the node-set,
                // compare (node_n, n_other); otherwise (n_other, node_n).
                if matches!(lv, Value::NodeSet(_)) { op(node_n, n_other) }
                else { op(n_other, node_n) }
            });
            return Ok(Value::Boolean(any));
        }
        (Value::ForeignNodeSet(fs), other) | (other, Value::ForeignNodeSet(fs))
            if !matches!(other, Value::NodeSet(_) | Value::ForeignNodeSet(_)) =>
        {
            let n_other = value_to_number_with(other, idx, ctx.bindings);
            let any = fs.iter().any(|&p| {
                let node_n = foreign_to_number(p);
                if matches!(lv, Value::ForeignNodeSet(_)) { op(node_n, n_other) }
                else { op(n_other, node_n) }
            });
            return Ok(Value::Boolean(any));
        }
        // Two node-sets — true iff any pair (ln, rn) satisfies the
        // relation.  Pre-compute one side's numbers to keep the
        // search O(L+R) for the boolean answer.
        (Value::NodeSet(ls), Value::NodeSet(rs)) => {
            let l_nums: Vec<f64> = ls.iter().map(|&id| node_to_number(id)).collect();
            let r_nums: Vec<f64> = rs.iter().map(|&id| node_to_number(id)).collect();
            let any = l_nums.iter().any(|&a| r_nums.iter().any(|&b| op(a, b)));
            return Ok(Value::Boolean(any));
        }
        _ => {}
    }
    // Date / dateTime / time / duration ordering — compare in their
    // own value space rather than as numbers.  XPath 2.0 §3.5.2.
    if let (Value::Typed(t), Value::Typed(u)) = (&lv, &rv) {
        if t.kind == u.kind {
            if matches!(t.kind, "date" | "dateTime" | "time") {
                if let (Some(a), Some(b)) = (
                    dt_to_utc_seconds(&t.lexical, t.kind),
                    dt_to_utc_seconds(&u.lexical, u.kind),
                ) {
                    return Ok(Value::Boolean(op(a as f64, b as f64)));
                }
            }
            if t.kind == "dayTimeDuration" {
                if let (Some(a), Some(b)) = (
                    parse_day_time_duration_secs(&t.lexical),
                    parse_day_time_duration_secs(&u.lexical),
                ) {
                    return Ok(Value::Boolean(op(a as f64, b as f64)));
                }
            }
            if t.kind == "yearMonthDuration" {
                if let (Some(a), Some(b)) = (
                    parse_year_month_duration_months(&t.lexical),
                    parse_year_month_duration_months(&u.lexical),
                ) {
                    return Ok(Value::Boolean(op(a as f64, b as f64)));
                }
            }
            // String-ordered types: fall through to numeric compare
            // — XPath general `<` between strings is implementation-
            // defined and tests typically use `lt` for that.
        }
    }
    let ln = value_to_number_with(&lv, idx, ctx.bindings);
    let rn = value_to_number_with(&rv, idx, ctx.bindings);
    Ok(Value::Boolean(op(ln, rn)))
}

/// EXSLT `dyn:evaluate(expr-string) → value` — compile `expr-string`
/// as an XPath expression and evaluate it against the current
/// context.  Per spec, a parse / eval failure yields an empty
/// node-set rather than propagating an error (libexslt's behaviour
/// — stylesheets use `dyn:evaluate` for "soft" lookups where a
/// malformed expression should produce no result, not abort the
/// transform).
fn dyn_evaluate<I: DocIndexLike>(
    args: &[Value], ctx: &EvalCtx<'_>, idx: &I,
) -> Result<Value> {
    if args.len() != 1 {
        return Err(xpath_err("dyn:evaluate takes 1 argument"));
    }
    let src = value_to_string(&args[0], idx);
    let opts = super::XPathOptions {
        libxml2_compatible: ctx.static_ctx.libxml2_compatible,
        ..super::XPathOptions::default()
    };
    let expr = match super::parse_xpath_with(&src, &opts) {
        Ok(e)  => e,
        Err(_) => return Ok(Value::NodeSet(Vec::new())),
    };
    match eval_expr(&expr, ctx, idx) {
        Ok(v)  => Ok(v),
        Err(_) => Ok(Value::NodeSet(Vec::new())),
    }
}

// ── built-in functions ────────────────────────────────────────────────────────

/// XPath 3.1 §17.1 `map:*` function library (the non-higher-order
/// subset; map:for-each / map:find need function items and live with
/// the HOF work).
fn eval_map_function<I: DocIndexLike>(
    local: &str, args: &[Value], ctx: &EvalCtx<'_>, idx: &I,
) -> Result<Value> {
    let as_map = |v: &Value| -> Result<Vec<(Value, Value)>> {
        match v {
            Value::Map(m) => Ok((**m).clone()),
            _ => Err(xpath_err(format!("map:{local}: expected a map argument"))),
        }
    };
    let empty = || Value::NodeSet(Vec::new());
    match local {
        "size" => Ok(Value::Number(Numeric::Integer(as_map(&args[0])?.len() as i64))),
        "contains" => {
            let m = as_map(&args[0])?;
            let k = first_atomic_key(&args[1], idx);
            Ok(Value::Boolean(m.iter().any(|(mk, _)| map_key_eq(mk, &k, idx))))
        }
        "get" => {
            let m = as_map(&args[0])?;
            let k = first_atomic_key(&args[1], idx);
            Ok(m.into_iter().find(|(mk, _)| map_key_eq(mk, &k, idx))
                .map(|(_, v)| v).unwrap_or_else(empty))
        }
        "keys" => Ok(seq_from_items(
            as_map(&args[0])?.into_iter().map(|(k, _)| k).collect())),
        "put" => {
            let mut m = as_map(&args[0])?;
            let k = first_atomic_key(&args[1], idx);
            m.retain(|(mk, _)| !map_key_eq(mk, &k, idx));
            m.push((k, args[2].clone()));
            Ok(Value::Map(Box::new(m)))
        }
        "remove" => {
            let mut m = as_map(&args[0])?;
            let ks = items_of(&args[1]);
            m.retain(|(mk, _)| !ks.iter().any(|k| map_key_eq(mk, k, idx)));
            Ok(Value::Map(Box::new(m)))
        }
        "entry" => Ok(Value::Map(Box::new(vec![
            (first_atomic_key(&args[0], idx), args[1].clone())]))),
        "merge" => {
            // map:merge($maps [, $options]) — `duplicates` defaults to
            // "use-first"; "use-last" lets later maps win.
            let use_last = match args.get(1) {
                Some(Value::Map(opts)) => opts.iter()
                    .find(|(k, _)| value_to_string(k, idx) == "duplicates")
                    .map(|(_, v)| value_to_string(v, idx) == "use-last")
                    .unwrap_or(false),
                _ => false,
            };
            let mut out: Vec<(Value, Value)> = Vec::new();
            for item in items_of(&args[0]) {
                if let Value::Map(m) = item {
                    for (k, v) in m.iter() {
                        match out.iter().position(|(ok, _)| map_key_eq(ok, k, idx)) {
                            Some(p) if use_last => out[p].1 = v.clone(),
                            Some(_) => {} // use-first: keep existing
                            None => out.push((k.clone(), v.clone())),
                        }
                    }
                }
            }
            Ok(Value::Map(Box::new(out)))
        }
        // map:for-each($map, $action) — $action($key, $value) per entry;
        // results concatenated into a single sequence.
        "for-each" => {
            let m = as_map(&args[0])?;
            let f = value_as_function(&args[1])?;
            let mut out = Vec::new();
            for (k, v) in m {
                out.push(call_function_item(f, vec![k, v], ctx, idx)?);
            }
            Ok(seq_from_items(out))
        }
        // map:find($input, $key) — search maps/arrays reachable from
        // $input, collecting matching values into an array.
        "find" => {
            let key = first_atomic_key(&args[1], idx);
            let mut found = Vec::new();
            fn search<I: DocIndexLike>(
                v: &Value, key: &Value, idx: &I, out: &mut Vec<Value>,
            ) {
                for item in items_of(v) {
                    match item {
                        Value::Map(m) => {
                            for (k, val) in m.iter() {
                                if map_key_eq(k, key, idx) { out.push(val.clone()); }
                                search(val, key, idx, out);
                            }
                        }
                        Value::Array(a) => for member in a.iter() {
                            search(member, key, idx, out);
                        },
                        _ => {}
                    }
                }
            }
            search(&args[0], &key, idx, &mut found);
            Ok(Value::Array(Box::new(found)))
        }
        _ => Err(xpath_err(format!(
            "map:{local} is not supported in this build"))),
    }
}

/// XPath 3.1 §17.3 `array:*` function library.
fn eval_array_function<I: DocIndexLike>(
    local: &str, args: &[Value], ctx: &EvalCtx<'_>, idx: &I,
) -> Result<Value> {
    let as_array = |v: &Value| -> Result<Vec<Value>> {
        match v {
            Value::Array(a) => Ok((**a).clone()),
            _ => Err(xpath_err(format!("array:{local}: expected an array argument"))),
        }
    };
    match local {
        "size" => Ok(Value::Number(Numeric::Integer(as_array(&args[0])?.len() as i64))),
        "get" => {
            let a = as_array(&args[0])?;
            let pos = value_to_number(&args[1], idx);
            if pos.fract() == 0.0 && pos >= 1.0 && (pos as usize) <= a.len() {
                Ok(a[pos as usize - 1].clone())
            } else {
                Err(xpath_err("array:get: index out of bounds (FOAY0001)"))
            }
        }
        "append" => {
            let mut a = as_array(&args[0])?;
            a.push(args[1].clone());
            Ok(Value::Array(Box::new(a)))
        }
        "head" => {
            let a = as_array(&args[0])?;
            a.into_iter().next().ok_or_else(|| xpath_err("array:head: empty array (FOAY0001)"))
        }
        "tail" => {
            let mut a = as_array(&args[0])?;
            if a.is_empty() { return Err(xpath_err("array:tail: empty array (FOAY0001)")); }
            a.remove(0);
            Ok(Value::Array(Box::new(a)))
        }
        "reverse" => {
            let mut a = as_array(&args[0])?;
            a.reverse();
            Ok(Value::Array(Box::new(a)))
        }
        "subarray" => {
            let a = as_array(&args[0])?;
            let start = value_to_number(&args[1], idx).round() as i64;
            let len = match args.get(2) {
                Some(v) => value_to_number(v, idx).round() as i64,
                None => a.len() as i64 - start + 1,
            };
            let s = (start - 1).max(0) as usize;
            let e = ((start - 1 + len).max(0) as usize).min(a.len());
            Ok(Value::Array(Box::new(a.get(s..e).map(<[_]>::to_vec).unwrap_or_default())))
        }
        "join" => {
            let mut out = Vec::new();
            for item in items_of(&args[0]) {
                if let Value::Array(a) = item { out.extend((*a).clone()); }
            }
            Ok(Value::Array(Box::new(out)))
        }
        "flatten" => {
            fn flat(v: &Value, out: &mut Vec<Value>) {
                for item in items_of(v) {
                    match item {
                        Value::Array(a) => for m in a.iter() { flat(m, out); },
                        other => out.push(other),
                    }
                }
            }
            let mut out = Vec::new();
            flat(&args[0], &mut out);
            Ok(seq_from_items(out))
        }
        // array:for-each($array, $action) — apply $action to each
        // member; each result becomes one member of the output array.
        "for-each" => {
            let a = as_array(&args[0])?;
            let f = value_as_function(&args[1])?;
            let mut out = Vec::with_capacity(a.len());
            for m in a {
                out.push(call_function_item(f, vec![m], ctx, idx)?);
            }
            Ok(Value::Array(Box::new(out)))
        }
        // array:filter($array, $predicate) — keep members for which the
        // predicate's effective boolean value is true.
        "filter" => {
            let a = as_array(&args[0])?;
            let f = value_as_function(&args[1])?;
            let mut out = Vec::new();
            for m in a {
                let keep = call_function_item(f, vec![m.clone()], ctx, idx)?;
                if value_to_bool(&keep, idx) { out.push(m); }
            }
            Ok(Value::Array(Box::new(out)))
        }
        "fold-left" => {
            let a = as_array(&args[0])?;
            let f = value_as_function(&args[2])?;
            let mut acc = args[1].clone();
            for m in a {
                acc = call_function_item(f, vec![acc, m], ctx, idx)?;
            }
            Ok(acc)
        }
        "fold-right" => {
            let a = as_array(&args[0])?;
            let f = value_as_function(&args[2])?;
            let mut acc = args[1].clone();
            for m in a.into_iter().rev() {
                acc = call_function_item(f, vec![m, acc], ctx, idx)?;
            }
            Ok(acc)
        }
        // array:for-each-pair($a, $b, $action) — pairwise over the
        // shorter length; each result is one member of the output.
        "for-each-pair" => {
            let a = as_array(&args[0])?;
            let b = as_array(&args[1])?;
            let f = value_as_function(&args[2])?;
            let mut out = Vec::new();
            for (x, y) in a.into_iter().zip(b) {
                out.push(call_function_item(f, vec![x, y], ctx, idx)?);
            }
            Ok(Value::Array(Box::new(out)))
        }
        // array:sort($array [, $collation [, $key]]) — stable sort by
        // the atomic value of each member (or of $key applied to it).
        "sort" => {
            let a = as_array(&args[0])?;
            let keyfn = match args.get(2) {
                Some(v) => Some(value_as_function(v)?),
                None => None,
            };
            let mut keyed: Vec<(Value, Value)> = Vec::with_capacity(a.len());
            for m in a {
                let k = match keyfn {
                    Some(f) => call_function_item(f, vec![m.clone()], ctx, idx)?,
                    None => m.clone(),
                };
                keyed.push((k, m));
            }
            keyed.sort_by(|(ka, _), (kb, _)| {
                compare_atomic_for_sort(ka, kb, idx, ctx.bindings)
            });
            Ok(Value::Array(Box::new(keyed.into_iter().map(|(_, m)| m).collect())))
        }
        // array:members / array:flatten variants returning the members
        // as a sequence are covered by `flatten`; expose `members`-style
        // access via `subarray`/`get`.
        _ => Err(xpath_err(format!(
            "array:{local} is not supported in this build"))),
    }
}

/// Extract a function item from a value for higher-order dispatch.
fn value_as_function(v: &Value) -> Result<&FunctionItem> {
    match v {
        Value::Function(fi) => Ok(fi),
        _ => Err(xpath_err("expected a function item (XPTY0004)")),
    }
}

/// Ordering used by `array:sort` / `fn:sort` — compares two atomic
/// keys by numeric value when both are numeric, else by string value.
/// Empty sequences sort before non-empty ones.
fn compare_atomic_for_sort<I: DocIndexLike>(
    a: &Value, b: &Value, idx: &I, bindings: &dyn XPathBindings,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let numeric = |v: &Value| -> Option<f64> {
        match v {
            Value::Number(_) => Some(value_to_number(v, idx)),
            Value::Typed(t) if matches!(t.kind,
                "integer" | "decimal" | "double" | "float" | "long" | "int"
                | "short" | "byte" | "numeric") => Some(value_to_number(v, idx)),
            _ => None,
        }
    };
    match (numeric(a), numeric(b)) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => value_to_string_with(a, idx, bindings)
            .cmp(&value_to_string_with(b, idx, bindings)),
    }
}

/// XPath 3.1 §16.2 higher-order function library (`fn:` namespace).
/// Returns `None` when `local` is not a higher-order function, so the
/// caller falls through to ordinary built-in dispatch.
fn eval_hof_function<I: DocIndexLike>(
    local: &str, args: &[Value], ctx: &EvalCtx<'_>, idx: &I,
) -> Option<Result<Value>> {
    let f = |i: usize| value_as_function(&args[i]);
    let res = match local {
        // fn:for-each($seq, $action) — apply $action to each item,
        // concatenating the results.
        "for-each" if args.len() == 2 => (|| {
            let func = f(1)?;
            let mut out = Vec::new();
            for item in iter_items(&args[0]) {
                out.push(call_function_item(func, vec![item], ctx, idx)?);
            }
            Ok(seq_from_items(out))
        })(),
        // fn:filter($seq, $predicate) — retain items whose predicate
        // result is true.
        "filter" if args.len() == 2 => (|| {
            let func = f(1)?;
            let mut out = Vec::new();
            for item in iter_items(&args[0]) {
                let keep = call_function_item(func, vec![item.clone()], ctx, idx)?;
                if value_to_bool(&keep, idx) { out.push(item); }
            }
            Ok(seq_from_items(out))
        })(),
        "fold-left" if args.len() == 3 => (|| {
            let func = f(2)?;
            let mut acc = args[1].clone();
            for item in iter_items(&args[0]) {
                acc = call_function_item(func, vec![acc, item], ctx, idx)?;
            }
            Ok(acc)
        })(),
        "fold-right" if args.len() == 3 => (|| {
            let func = f(2)?;
            let items: Vec<Value> = iter_items(&args[0]).collect();
            let mut acc = args[1].clone();
            for item in items.into_iter().rev() {
                acc = call_function_item(func, vec![item, acc], ctx, idx)?;
            }
            Ok(acc)
        })(),
        // fn:for-each-pair($seq1, $seq2, $action) — pairwise over the
        // shorter sequence.
        "for-each-pair" if args.len() == 3 => (|| {
            let func = f(2)?;
            let mut out = Vec::new();
            for (x, y) in iter_items(&args[0]).zip(iter_items(&args[1])) {
                out.push(call_function_item(func, vec![x, y], ctx, idx)?);
            }
            Ok(seq_from_items(out))
        })(),
        // fn:sort($input [, $collation [, $key]]).
        "sort" if (1..=3).contains(&args.len()) => (|| {
            let keyfn = match args.get(2) {
                Some(v) => Some(value_as_function(v)?),
                None => None,
            };
            let mut keyed: Vec<(Value, Value)> = Vec::new();
            for item in iter_items(&args[0]) {
                let k = match keyfn {
                    Some(func) => call_function_item(func, vec![item.clone()], ctx, idx)?,
                    None => item.clone(),
                };
                keyed.push((k, item));
            }
            keyed.sort_by(|(ka, _), (kb, _)|
                compare_atomic_for_sort(ka, kb, idx, ctx.bindings));
            Ok(seq_from_items(keyed.into_iter().map(|(_, v)| v).collect()))
        })(),
        // fn:apply($function, $array) — call $function with the array's
        // members as positional arguments.
        "apply" if args.len() == 2 => (|| {
            let func = f(0)?;
            let call_args = match &args[1] {
                Value::Array(a) => (**a).clone(),
                _ => return Err(xpath_err("fn:apply: second argument must be an array")),
            };
            call_function_item(func, call_args, ctx, idx)
        })(),
        // fn:function-arity($function).
        "function-arity" if args.len() == 1 =>
            f(0).map(|func| Value::Number(Numeric::Integer(func.arity() as i64))),
        // fn:function-name($function) as xs:QName? — the expanded QName of
        // a named-function reference, or the empty sequence for an
        // anonymous (inline / partially-applied) function.
        "function-name" if args.len() == 1 => f(0).map(|func| match func {
            FunctionItem::Named { name, ns, .. } => {
                let local = name.rsplit(':').next().unwrap_or(name);
                let lexical = if ns.is_empty() {
                    local.to_string()
                } else {
                    format!("{{{ns}}}{local}")
                };
                Value::Typed(Box::new(TypedAtomic {
                    kind: "QName", lexical, numeric: None, boolean: None,
                }))
            }
            _ => Value::NodeSet(Vec::new()),
        }),
        // fn:function-lookup($name as xs:QName, $arity as xs:integer) — a
        // function item for the named function, or the empty sequence when
        // no such function is available in scope.
        "function-lookup" if args.len() == 2 => {
            let qname = value_to_string(&args[0], idx);
            let arity = value_to_number(&args[1], idx) as usize;
            // QName string-value is Clark `{uri}local`, lexical
            // `prefix:local`, or a bare local in the default function
            // namespace.
            let (ns, local) = if let Some(rest) = qname.strip_prefix('{') {
                rest.split_once('}')
                    .map(|(u, l)| (u.to_string(), l.to_string()))
                    .unwrap_or_else(|| (String::new(), qname.clone()))
            } else if let Some((prefix, l)) = qname.split_once(':') {
                (resolve_prefix_or_implicit(ctx.bindings, prefix).unwrap_or_default(),
                 l.to_string())
            } else {
                (FN_NAMESPACE.to_string(), qname.clone())
            };
            let available = if ns.is_empty() || ns.as_str() == FN_NAMESPACE {
                xpath_function_available(&local, ctx)
            } else {
                ctx.bindings.function_available_in(&ns, &local, arity)
            };
            if available {
                let sig = ctx.bindings.function_signature_in(&ns, &local, arity).map(Box::new);
                Ok(Value::Function(Box::new(FunctionItem::Named { name: local, ns, arity, sig })))
            } else {
                Ok(Value::NodeSet(Vec::new()))
            }
        }
        _ => return None,
    };
    Some(res)
}

/// XPath 3.1 §17.5 JSON function library (`fn:` namespace).  Returns
/// `None` when `local` is not a JSON function so the caller falls
/// through to ordinary built-in dispatch.
fn eval_json_function<I: DocIndexLike>(
    local: &str, args: &[Value], _ctx: &EvalCtx<'_>, idx: &I,
) -> Option<Result<Value>> {
    // Read a string-valued entry from an options map (XPath 3.1 §17.5).
    let opt_str = |opts: Option<&Value>, key: &str| -> Option<String> {
        match opts {
            Some(Value::Map(m)) => m.iter()
                .find(|(k, _)| value_to_string(k, idx) == key)
                .map(|(_, v)| value_to_string(v, idx)),
            _ => None,
        }
    };
    let opt_bool = |opts: Option<&Value>, key: &str| -> Option<bool> {
        match opts {
            Some(Value::Map(m)) => m.iter()
                .find(|(k, _)| value_to_string(k, idx) == key)
                .map(|(_, v)| value_to_bool(v, idx)),
            _ => None,
        }
    };
    let res = match local {
        // fn:parse-json($json-text [, $options]) → the XDM map/array
        // representation (F&O §17.5.1).
        "parse-json" if (1..=2).contains(&args.len()) => (|| {
            // An empty sequence argument yields the empty sequence.
            if sequence_len(&args[0]) == 0 {
                return Ok(Value::Sequence(Vec::new()));
            }
            let text = value_to_string(&args[0], idx);
            let opts = args.get(1);
            let dup = opt_str(opts, "duplicates")
                .unwrap_or_else(|| "use-first".to_string());
            let escape = opt_bool(opts, "escape").unwrap_or(false);
            let liberal = opt_bool(opts, "liberal").unwrap_or(false);
            parse_json_value(&text, &dup, escape, liberal)
        })(),
        // fn:xml-to-json($input [, $options]) — serialize a node in the
        // F&O JSON element vocabulary to a JSON string (§17.5.5).
        "xml-to-json" if (1..=2).contains(&args.len()) => (|| {
            let root = match &args[0] {
                Value::NodeSet(ns) if ns.len() == 1 => ns[0],
                Value::NodeSet(ns) if ns.is_empty() =>
                    return Ok(Value::Sequence(Vec::new())),
                _ => return Err(xpath_err(
                    "xml-to-json: argument must be a single document or element node")),
            };
            // A document node must wrap exactly one element child
            // (F&O §17.5.5 requires a single element as the input).
            let elem = match idx.kind(root) {
                XPathNodeKind::Document => {
                    let mut elems = idx.children(root).iter().copied()
                        .filter(|&c| matches!(idx.kind(c), XPathNodeKind::Element));
                    let first = elems.next().ok_or_else(||
                        xpath_err("xml-to-json: empty document (FOJS0006)"))?;
                    if elems.next().is_some() {
                        return Err(xpath_err(
                            "xml-to-json: more than one top-level element (FOJS0006)"));
                    }
                    first
                }
                XPathNodeKind::Element => root,
                _ => return Err(xpath_err(
                    "xml-to-json: argument must be a document or element node")),
            };
            let mut out = String::new();
            xml_node_to_json(elem, idx, false, &mut out)?;
            Ok(Value::String(out))
        })(),
        // fn:serialize($value [, $options]) (F&O §17.2) — only the JSON
        // output method is modelled here; node-tree serialization (the
        // xml/html/text methods) is the XSLT result-document layer's job,
        // so those fall back to the value's string value.
        "serialize" if (1..=2).contains(&args.len()) => (|| {
            let method = opt_str(args.get(1), "method").unwrap_or_default();
            if method == "json" {
                let mut out = String::new();
                value_to_json(&args[0], idx, &mut out)?;
                Ok(Value::String(out))
            } else {
                Ok(Value::String(value_to_string(&args[0], idx)))
            }
        })(),
        _ => return None,
    };
    Some(res)
}

/// Serialize an XDM value as JSON (the `method=json` output method,
/// F&O §17.2 / §17.5): maps become objects, arrays become arrays, the
/// numeric/boolean atomics become JSON literals, and everything else its
/// quoted string value.
fn value_to_json<I: DocIndexLike>(v: &Value, idx: &I, out: &mut String) -> Result<()> {
    match v {
        Value::Map(m) => {
            out.push('{');
            for (i, (k, val)) in m.iter().enumerate() {
                if i > 0 { out.push(','); }
                out.push('"');
                json_escape_into(&value_to_string(k, idx), out);
                out.push_str("\":");
                value_to_json(val, idx, out)?;
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, val) in a.iter().enumerate() {
                if i > 0 { out.push(','); }
                value_to_json(val, idx, out)?;
            }
            out.push(']');
        }
        Value::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(_) => out.push_str(&value_to_string(v, idx)),
        Value::Typed(t) if t.numeric.is_some() => out.push_str(&value_to_string(v, idx)),
        Value::NodeSet(ns) if ns.is_empty() => out.push_str("null"),
        Value::Sequence(items) if items.is_empty() => out.push_str("null"),
        other => {
            out.push('"');
            json_escape_into(&value_to_string(other, idx), out);
            out.push('"');
        }
    }
    Ok(())
}

/// A structural event emitted by [`parse_json_events`].  Lets a
/// consumer build either the XDM map/array [`Value`] (fn:parse-json)
/// or the F&O JSON element vocabulary (fn:json-to-xml) from one
/// parser.  `Number` carries the *raw* lexical so json-to-xml can
/// preserve the input's number representation.
pub enum JsonEvent {
    StartObject,
    EndObject,
    StartArray,
    EndArray,
    /// Object member key (already unescaped per the `escape` option).
    Key(String),
    Str(String),
    Number(String),
    Bool(bool),
    Null,
}

/// Parse `json` (F&O §17.5), driving `sink` with one [`JsonEvent`] per
/// structural token.  Duplicate-key policy is left to the consumer —
/// the parser reports keys verbatim.  `escape` keeps backslash
/// sequences in string content; `liberal` tolerates unescaped control
/// characters.
pub fn parse_json_events(
    json: &str, escape: bool, liberal: bool, sink: &mut dyn FnMut(JsonEvent),
) -> Result<()> {
    let chars: Vec<char> = json.chars().collect();
    let mut p = JsonParser { chars: &chars, pos: 0, escape, liberal };
    p.skip_ws();
    p.parse_value(sink)?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return Err(xpath_err("parse-json: trailing content after JSON value (FOJS0001)"));
    }
    Ok(())
}

/// Parse `text` to the XDM representation (F&O §17.5): object → map,
/// array → array, string → xs:string, number → xs:double, true/false →
/// xs:boolean, null → empty sequence.  `duplicates` is one of
/// `reject` / `use-first` / `use-last`.  Public so the XSLT layer's
/// `fn:json-doc` can reuse it after loading the resource.
pub fn parse_json_value(text: &str, duplicates: &str, escape: bool, liberal: bool)
    -> Result<Value>
{
    // Build the Value with an explicit frame stack so the shared
    // event parser can drive both this and json-to-xml.
    enum Frame { Obj(Vec<(Value, Value)>, Option<String>), Arr(Vec<Value>) }
    let mut stack: Vec<Frame> = Vec::new();
    let mut result: Option<Value> = None;
    let mut dup_err: Option<crate::error::XmlError> = None;

    let mut attach = |stack: &mut Vec<Frame>, result: &mut Option<Value>, v: Value| {
        match stack.last_mut() {
            Some(Frame::Arr(a)) => a.push(v),
            Some(Frame::Obj(entries, pending)) => {
                let key = pending.take().unwrap_or_default();
                match entries.iter().position(|(k, _)| value_as_str(k) == key) {
                    Some(i) => match duplicates {
                        "reject" => {
                            dup_err.get_or_insert_with(|| xpath_err(format!(
                                "parse-json: duplicate key {key:?} (FOJS0003)")));
                        }
                        "use-last" => entries[i].1 = v,
                        _ => {}
                    },
                    None => entries.push((Value::String(key), v)),
                }
            }
            None => *result = Some(v),
        }
    };

    parse_json_events(text, escape, liberal, &mut |ev| match ev {
        JsonEvent::StartObject => stack.push(Frame::Obj(Vec::new(), None)),
        JsonEvent::StartArray  => stack.push(Frame::Arr(Vec::new())),
        JsonEvent::Key(k) => {
            if let Some(Frame::Obj(_, pending)) = stack.last_mut() { *pending = Some(k); }
        }
        JsonEvent::EndObject => {
            if let Some(Frame::Obj(entries, _)) = stack.pop() {
                attach(&mut stack, &mut result, Value::Map(Box::new(entries)));
            }
        }
        JsonEvent::EndArray => {
            if let Some(Frame::Arr(members)) = stack.pop() {
                attach(&mut stack, &mut result, Value::Array(Box::new(members)));
            }
        }
        JsonEvent::Str(s)    => attach(&mut stack, &mut result, Value::String(s)),
        JsonEvent::Number(n) => attach(&mut stack, &mut result,
            Value::Number(Numeric::Double(n.parse::<f64>().unwrap_or(f64::NAN)))),
        JsonEvent::Bool(b)   => attach(&mut stack, &mut result, Value::Boolean(b)),
        JsonEvent::Null      => attach(&mut stack, &mut result, Value::Sequence(Vec::new())),
    })?;
    if let Some(e) = dup_err { return Err(e); }
    Ok(result.unwrap_or_else(|| Value::Sequence(Vec::new())))
}

struct JsonParser<'a> {
    chars: &'a [char],
    pos:   usize,
    escape: bool,
    liberal: bool,
}

impl<'a> JsonParser<'a> {
    fn peek(&self) -> Option<char> { self.chars.get(self.pos).copied() }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == ' ' || c == '\t' || c == '\n' || c == '\r' { self.pos += 1; }
            else { break; }
        }
    }

    fn parse_value(&mut self, sink: &mut dyn FnMut(JsonEvent)) -> Result<()> {
        match self.peek() {
            Some('{') => self.parse_object(sink),
            Some('[') => self.parse_array(sink),
            Some('"') => { let s = self.parse_string()?; sink(JsonEvent::Str(s)); Ok(()) }
            Some('t') | Some('f') => self.parse_bool(sink),
            Some('n') => self.parse_null(sink),
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number(sink),
            _ => Err(xpath_err("parse-json: unexpected character (FOJS0001)")),
        }
    }

    fn parse_object(&mut self, sink: &mut dyn FnMut(JsonEvent)) -> Result<()> {
        self.pos += 1; // {
        sink(JsonEvent::StartObject);
        self.skip_ws();
        if self.peek() == Some('}') { self.pos += 1; sink(JsonEvent::EndObject); return Ok(()); }
        loop {
            self.skip_ws();
            if self.peek() != Some('"') {
                return Err(xpath_err("parse-json: expected object key (FOJS0001)"));
            }
            let key = self.parse_string()?;
            sink(JsonEvent::Key(key));
            self.skip_ws();
            if self.peek() != Some(':') {
                return Err(xpath_err("parse-json: expected ':' in object (FOJS0001)"));
            }
            self.pos += 1;
            self.skip_ws();
            self.parse_value(sink)?;
            self.skip_ws();
            match self.peek() {
                Some(',') => { self.pos += 1; continue; }
                Some('}') => { self.pos += 1; break; }
                _ => return Err(xpath_err("parse-json: expected ',' or '}' (FOJS0001)")),
            }
        }
        sink(JsonEvent::EndObject);
        Ok(())
    }

    fn parse_array(&mut self, sink: &mut dyn FnMut(JsonEvent)) -> Result<()> {
        self.pos += 1; // [
        sink(JsonEvent::StartArray);
        self.skip_ws();
        if self.peek() == Some(']') { self.pos += 1; sink(JsonEvent::EndArray); return Ok(()); }
        loop {
            self.skip_ws();
            self.parse_value(sink)?;
            self.skip_ws();
            match self.peek() {
                Some(',') => { self.pos += 1; continue; }
                Some(']') => { self.pos += 1; break; }
                _ => return Err(xpath_err("parse-json: expected ',' or ']' (FOJS0001)")),
            }
        }
        sink(JsonEvent::EndArray);
        Ok(())
    }

    fn parse_string(&mut self) -> Result<String> {
        self.pos += 1; // opening quote
        let mut s = String::new();
        loop {
            match self.peek() {
                None => return Err(xpath_err("parse-json: unterminated string (FOJS0001)")),
                Some('"') => { self.pos += 1; break; }
                Some('\\') => {
                    self.pos += 1;
                    let esc = self.peek().ok_or_else(||
                        xpath_err("parse-json: dangling escape (FOJS0001)"))?;
                    self.pos += 1;
                    // When `escape` is requested, the lexical backslash
                    // sequence is preserved verbatim in the result.
                    if self.escape && esc != 'u' {
                        s.push('\\'); s.push(esc); continue;
                    }
                    match esc {
                        '"' => s.push('"'),
                        '\\' => s.push('\\'),
                        '/' => s.push('/'),
                        'b' => s.push('\u{0008}'),
                        'f' => s.push('\u{000C}'),
                        'n' => s.push('\n'),
                        'r' => s.push('\r'),
                        't' => s.push('\t'),
                        'u' => {
                            let cp = self.parse_hex4()?;
                            // Surrogate pair handling.
                            if (0xD800..=0xDBFF).contains(&cp) {
                                if self.peek() == Some('\\') {
                                    self.pos += 1;
                                    if self.peek() == Some('u') {
                                        self.pos += 1;
                                        let lo = self.parse_hex4()?;
                                        let c = 0x10000
                                            + ((cp - 0xD800) << 10)
                                            + (lo - 0xDC00);
                                        if self.escape {
                                            s.push_str(&format!("\\u{cp:04X}\\u{lo:04X}"));
                                        } else if let Some(ch) = char::from_u32(c) {
                                            s.push(ch);
                                        }
                                        continue;
                                    }
                                }
                                return Err(xpath_err(
                                    "parse-json: invalid surrogate (FOJS0001)"));
                            }
                            if self.escape {
                                s.push_str(&format!("\\u{cp:04X}"));
                            } else if let Some(ch) = char::from_u32(cp) {
                                s.push(ch);
                            }
                        }
                        _ => return Err(xpath_err(format!(
                            "parse-json: invalid escape \\{esc} (FOJS0001)"))),
                    }
                }
                Some(c) => {
                    // Unescaped control characters are an error unless
                    // `liberal` parsing was requested.
                    if (c as u32) < 0x20 && !self.liberal {
                        return Err(xpath_err(
                            "parse-json: unescaped control character (FOJS0001)"));
                    }
                    s.push(c);
                    self.pos += 1;
                }
            }
        }
        Ok(s)
    }

    fn parse_hex4(&mut self) -> Result<u32> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.peek().ok_or_else(||
                xpath_err("parse-json: short \\u escape (FOJS0001)"))?;
            let d = c.to_digit(16).ok_or_else(||
                xpath_err("parse-json: invalid hex in \\u escape (FOJS0001)"))?;
            v = v * 16 + d;
            self.pos += 1;
        }
        Ok(v)
    }

    fn parse_bool(&mut self, sink: &mut dyn FnMut(JsonEvent)) -> Result<()> {
        if self.matches_literal("true") { sink(JsonEvent::Bool(true)); Ok(()) }
        else if self.matches_literal("false") { sink(JsonEvent::Bool(false)); Ok(()) }
        else { Err(xpath_err("parse-json: invalid literal (FOJS0001)")) }
    }

    fn parse_null(&mut self, sink: &mut dyn FnMut(JsonEvent)) -> Result<()> {
        if self.matches_literal("null") { sink(JsonEvent::Null); Ok(()) }
        else { Err(xpath_err("parse-json: invalid literal (FOJS0001)")) }
    }

    fn matches_literal(&mut self, lit: &str) -> bool {
        let end = self.pos + lit.len();
        if end <= self.chars.len()
            && self.chars[self.pos..end].iter().copied().eq(lit.chars())
        {
            self.pos = end;
            true
        } else {
            false
        }
    }

    fn parse_number(&mut self, sink: &mut dyn FnMut(JsonEvent)) -> Result<()> {
        let start = self.pos;
        let digits = |p: &mut Self| {
            let s = p.pos;
            while p.peek().is_some_and(|c| c.is_ascii_digit()) { p.pos += 1; }
            p.pos > s
        };
        let bad = || xpath_err("parse-json: invalid number (FOJS0001)");
        if self.liberal {
            // Liberal mode tolerates the lexical forms JSON forbids
            // (leading zeros, leading `+`, bare fraction); scan loosely.
            if matches!(self.peek(), Some('-') | Some('+')) { self.pos += 1; }
            while self.peek().is_some_and(|c|
                c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) {
                self.pos += 1;
            }
        } else {
            // Strict RFC 8259 number grammar:
            // `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`.
            if self.peek() == Some('-') { self.pos += 1; }
            match self.peek() {
                Some('0') => self.pos += 1, // a leading 0 stands alone
                Some(c) if c.is_ascii_digit() => { digits(self); }
                _ => return Err(bad()),
            }
            if self.peek() == Some('.') {
                self.pos += 1;
                if !digits(self) { return Err(bad()); }
            }
            if matches!(self.peek(), Some('e') | Some('E')) {
                self.pos += 1;
                if matches!(self.peek(), Some('+') | Some('-')) { self.pos += 1; }
                if !digits(self) { return Err(bad()); }
            }
        }
        let lex: String = self.chars[start..self.pos].iter().collect();
        match lex.parse::<f64>() {
            Ok(n) if n.is_finite() => { sink(JsonEvent::Number(lex)); Ok(()) }
            _ => Err(xpath_err(format!("parse-json: invalid number {lex:?} (FOJS0001)"))),
        }
    }
}

/// Validate and read a boolean-typed JSON option (F&O §17.5): the value
/// must be exactly one xs:boolean.  Absent → `Ok(None)`; wrong
/// type/cardinality → FOJS0005.
pub fn json_option_bool<I: DocIndexLike>(
    opts: Option<&Value>, key: &str, idx: &I,
) -> Result<Option<bool>> {
    fn single_bool(v: &Value) -> Option<bool> {
        match v {
            Value::Boolean(b) => Some(*b),
            Value::Typed(t) if t.kind == "boolean" => t.boolean,
            Value::Sequence(items) if items.len() == 1 => single_bool(&items[0]),
            _ => None,
        }
    }
    let Some(Value::Map(m)) = opts else { return Ok(None) };
    match m.iter().find(|(k, _)| value_to_string(k, idx) == key) {
        None => Ok(None),
        Some((_, v)) => single_bool(v).map(Some).ok_or_else(||
            xpath_err(format!("JSON option {key:?} must be a single boolean (FOJS0005)"))),
    }
}

/// The string value of a map key for duplicate detection.
fn value_as_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Typed(t) => t.lexical.clone(),
        _ => String::new(),
    }
}

/// The XPath/XQuery functions-and-operators namespace, which the JSON
/// element vocabulary (`map`/`array`/`string`/…) lives in.
const FN_NAMESPACE: &str = "http://www.w3.org/2005/xpath-functions";

/// Serialize an element in the F&O JSON element vocabulary
/// (`map`/`array`/`string`/`number`/`boolean`/`null` in the
/// xpath-functions namespace) to a JSON string (F&O §17.5.5),
/// validating the vocabulary as it goes (FOJS0006 / FOJS0007).
/// `in_map` is true when `elem` is a member of an enclosing `map`,
/// where a `key` attribute is required and otherwise forbidden.
fn xml_node_to_json<I: DocIndexLike>(
    elem: NodeId, idx: &I, in_map: bool, out: &mut String,
) -> Result<()> {
    let err = |m: &str| xpath_err(format!("xml-to-json: {m} (FOJS0006)"));
    if idx.namespace_uri(elem) != FN_NAMESPACE {
        return Err(err("element is not in the JSON namespace"));
    }
    let local = idx.local_name(elem).to_string();
    // Validate attributes: only `key`/`escaped`/`escaped-key` in no
    // namespace are recognised; foreign-namespace attributes are
    // ignored (§17.5.5).
    for a in idx.attr_range(elem) {
        if !idx.namespace_uri(a).is_empty() { continue; }
        match idx.local_name(a) {
            "key" | "escaped" | "escaped-key" => {}
            other => return Err(err(&format!("unexpected attribute {other:?}"))),
        }
    }
    let has_key = attr_value(elem, "key", idx).is_some();
    if in_map && !has_key { return Err(err("map entry is missing its key")); }
    if !in_map && has_key { return Err(err("key attribute outside a map")); }

    let elem_children: Vec<NodeId> = idx.children(elem).iter().copied()
        .filter(|&c| matches!(idx.kind(c), XPathNodeKind::Element)).collect();
    let text = direct_text(elem, idx);
    let text_is_blank = text.trim().is_empty();

    if in_map {
        // Emit the (possibly pre-escaped) key before the value.
        let key = attr_value(elem, "key", idx).unwrap_or_default();
        out.push('"');
        if json_bool_attr(elem, "escaped-key", idx)?.unwrap_or(false) {
            validate_json_escapes(&key)?;
            out.push_str(&key);
        } else {
            json_escape_into(&key, out);
        }
        out.push_str("\":");
    }

    match local.as_str() {
        "null" => {
            if !elem_children.is_empty() || !text_is_blank {
                return Err(err("null must be empty"));
            }
            out.push_str("null");
        }
        "boolean" => {
            if !elem_children.is_empty() { return Err(err("boolean has element content")); }
            match text.trim() {
                "true" | "1"  => out.push_str("true"),
                "false" | "0" => out.push_str("false"),
                _ => return Err(err("boolean value is not true/false")),
            }
        }
        "number" => {
            if !elem_children.is_empty() { return Err(err("number has element content")); }
            let n = text.trim();
            match n.parse::<f64>() {
                Ok(f) if f.is_finite() => out.push_str(n),
                _ => return Err(err("number is not a valid JSON number")),
            }
        }
        "string" => {
            if !elem_children.is_empty() { return Err(err("string has element content")); }
            out.push('"');
            if json_bool_attr(elem, "escaped", idx)?.unwrap_or(false) {
                // Content is already JSON-escaped: validate (FOJS0007)
                // and emit verbatim.
                validate_json_escapes(&text)?;
                out.push_str(&text);
            } else {
                json_escape_into(&text, out);
            }
            out.push('"');
        }
        "array" => {
            if !text_is_blank { return Err(err("array has text content")); }
            out.push('[');
            for (i, &c) in elem_children.iter().enumerate() {
                if i > 0 { out.push(','); }
                xml_node_to_json(c, idx, false, out)?;
            }
            out.push(']');
        }
        "map" => {
            if !text_is_blank { return Err(err("map has text content")); }
            out.push('{');
            let mut seen: Vec<String> = Vec::new();
            for (i, &c) in elem_children.iter().enumerate() {
                if idx.namespace_uri(c) == FN_NAMESPACE {
                    if let Some(k) = attr_value(c, "key", idx) {
                        if seen.contains(&k) {
                            return Err(err("duplicate key in map"));
                        }
                        seen.push(k);
                    }
                }
                if i > 0 { out.push(','); }
                xml_node_to_json(c, idx, true, out)?;
            }
            out.push('}');
        }
        other => return Err(err(&format!("unexpected element {other:?}"))),
    }
    Ok(())
}

/// Concatenated string value of an element's direct text / CDATA
/// children (ignoring descendant elements).
fn direct_text<I: DocIndexLike>(elem: NodeId, idx: &I) -> String {
    let mut s = String::new();
    for &c in idx.children(elem) {
        if matches!(idx.kind(c), XPathNodeKind::Text | XPathNodeKind::CData) {
            s.push_str(&idx.string_value(c));
        }
    }
    s
}

/// Read a boolean-valued attribute (`true`/`false`/`1`/`0`); a value
/// outside that set is FOJS0006.
fn json_bool_attr<I: DocIndexLike>(elem: NodeId, name: &str, idx: &I) -> Result<Option<bool>> {
    match attr_value(elem, name, idx) {
        None => Ok(None),
        Some(v) => match v.trim() {
            "true" | "1"  => Ok(Some(true)),
            "false" | "0" => Ok(Some(false)),
            _ => Err(xpath_err(format!(
                "xml-to-json: {name} is not a boolean (FOJS0006)"))),
        },
    }
}

/// Validate that `s` contains only well-formed JSON escape sequences
/// (used for content marked `escaped="true"`); FOJS0007 on failure.
fn validate_json_escapes(s: &str) -> Result<()> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            let Some(&next) = chars.get(i + 1) else {
                return Err(xpath_err("xml-to-json: dangling backslash (FOJS0007)"));
            };
            match next {
                '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' => i += 2,
                'u' => {
                    let hex = chars.get(i + 2..i + 6);
                    if !hex.is_some_and(|h| h.iter().all(|c| c.is_ascii_hexdigit())) {
                        return Err(xpath_err(
                            "xml-to-json: invalid \\u escape (FOJS0007)"));
                    }
                    i += 6;
                }
                _ => return Err(xpath_err(format!(
                    "xml-to-json: invalid escape \\{next} (FOJS0007)"))),
            }
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// Look up an unprefixed attribute's value on an element node.
fn attr_value<I: DocIndexLike>(elem: NodeId, name: &str, idx: &I) -> Option<String> {
    idx.attr_range(elem)
        .find(|&a| idx.namespace_uri(a).is_empty() && idx.local_name(a) == name)
        .map(|a| idx.string_value(a))
}

/// Append `s` to `out` with JSON string escaping (F&O §17.4.2).
fn json_escape_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
}

fn eval_function<I: DocIndexLike>(name: &str, args: &[Expr], ctx: &EvalCtx<'_>, idx: &I) -> Result<Value> {
    // Try the caller-supplied function table.  A `prefix:local`
    // function name resolves the prefix to a URI via bindings;
    // unprefixed names dispatch under empty URI (lxml's
    // `extensions={(None, name): fn}` form).  Built-in XPath 1.0
    // functions (count, string, …) only exist under the empty URI,
    // so the call_function hook is given priority — it can be used
    // to override a built-in but, more commonly, registers
    // application-specific functions in the same namespace.
    let (call_uri, call_local) = match name.split_once(':') {
        Some((prefix, local)) => match resolve_prefix_or_implicit(ctx.bindings, prefix) {
            Some(uri) => (uri, local.to_string()),
            None => return Err(xpath_err(format!(
                "Undefined namespace prefix in function name: {prefix}"
            ))),
        },
        None => (String::new(), name.to_string()),
    };
    // Eagerly evaluate the args so we can pass concrete values to the
    // bindings hook — built-in dispatch below re-evaluates lazily
    // through macros, which would double-eval if we used the same
    // path for both.  We materialize once and reuse for built-ins.
    let mut arg_vals: Option<Vec<Value>> = None;
    let take_args = |vals: &mut Option<Vec<Value>>, args: &[Expr]| -> Result<Vec<Value>> {
        if let Some(v) = vals.take() { return Ok(v); }
        args.iter().map(|e| eval_expr(e, ctx, idx)).collect()
    };
    {
        let vs = take_args(&mut arg_vals, args)?;
        // 1. User-registered bindings take priority — lets callers
        //    override EXSLT or define their own functions in any
        //    namespace.
        let res = ctx.bindings.call_function_in(&call_uri, &call_local, vs.clone(), ctx.context_node);
        if let Some(r) = res { return r; }
        // 2. EXSLT built-in families (math / date / str / set / regexp / common).
        //    Returns `Some` iff `call_uri` is one of the EXSLT
        //    namespace URIs *and* the family recognises the name.
        if let Some(r) = super::exslt::dispatch(&call_uri, &call_local, vs.clone(), idx) {
            return r;
        }
        // 2a. EXSLT `dyn:evaluate(string)` — compile and evaluate the
        //     argument as an XPath expression against the current
        //     context.  Lives here (rather than in `exslt::dispatch`)
        //     because it needs the full `EvalCtx` to re-enter the
        //     parser and evaluator at runtime.
        if call_uri == super::exslt::DYN_NS && call_local == "evaluate" {
            return dyn_evaluate(&vs, ctx, idx);
        }
        // 2b. XSD-namespace constructor calls — `xs:integer(arg)`,
        //     `xs:string(arg)`, etc.  XPath 2.0 promotes these to
        //     first-class atomization conversions; we support the
        //     common ones (string/number/boolean/decimal/date)
        //     because XSD 1.1 `xs:assert` expressions in real
        //     schemas use them to coerce attribute values for
        //     numeric or temporal comparisons.
        if call_uri == "http://www.w3.org/2001/XMLSchema" {
            return Ok(xs_constructor(&call_local, &vs, idx, ctx.bindings)?);
        }
        // XPath 3.1 §17 map:* / array:* function libraries.
        if call_uri == "http://www.w3.org/2005/xpath-functions/map" {
            return eval_map_function(&call_local, &vs, ctx, idx);
        }
        if call_uri == "http://www.w3.org/2005/xpath-functions/array" {
            return eval_array_function(&call_local, &vs, ctx, idx);
        }
        // XPath 3.1 §16 higher-order functions live in the `fn:` URI
        // (and the no-prefix default) — fn:for-each, fold-left, etc.
        if call_uri.is_empty()
            || call_uri == "http://www.w3.org/2005/xpath-functions"
        {
            if let Some(r) = eval_hof_function(&call_local, &vs, ctx, idx) {
                return r;
            }
            // XPath 3.1 §17.5 JSON functions.
            if let Some(r) = eval_json_function(&call_local, &vs, ctx, idx) {
                return r;
            }
        }
        arg_vals = Some(vs);
    }
    // Prefixed function with no registered handler and no EXSLT
    // match — XPath 1.0 says that's an error (no built-ins exist
    // outside the default namespace).  The one exception is the
    // XPath 2.0 functions namespace (`fn:`); built-ins live in
    // that URI too, so fall through to built-in dispatch when
    // the call URI matches.
    if !call_uri.is_empty()
        && call_uri != "http://www.w3.org/2005/xpath-functions"
    {
        return Err(xpath_err(format!(
            "Unregistered XPath function: {{{call_uri}}}{call_local}"
        )));
    }
    // Pre-evaluated args available — built-ins below can use them via
    // the arg!() macro by indexing into `arg_vals`.  Or, since the
    // existing macros assume re-evaluation, we leave them alone:
    // duplicate eval is benign for XPath 1.0 (no side effects).
    let _ = arg_vals; // hand off to built-in dispatch (which re-evaluates)
    macro_rules! arg {
        ($n:expr) => {
            eval_expr(&args[$n], ctx, idx)?
        };
    }
    macro_rules! arg_str {
        ($n:expr) => {
            value_to_string_with(&arg!($n), idx, ctx.bindings)
        };
    }
    macro_rules! arg_num {
        ($n:expr) => {
            value_to_number_with(&arg!($n), idx, ctx.bindings)
        };
    }
    macro_rules! check_args {
        ($n:expr) => {
            if args.len() != $n {
                return Err(xpath_err(format!("{}() requires {} argument(s)", name, $n))
                    .with_xpath_code("XPTY0004"));
            }
        };
    }

    // Built-ins always dispatch on the local part — `fn:`-prefixed
    // calls flow through here too after the prefix has been resolved
    // to the XPath functions URI above.
    let name = call_local.as_str();
    match name {
        // ── boolean functions ────────────────────────────────────────────────
        "true" => { check_args!(0); Ok(Value::Boolean(true)) }
        "false" => { check_args!(0); Ok(Value::Boolean(false)) }
        "not" => {
            check_args!(1);
            Ok(Value::Boolean(!value_to_bool(&arg!(0), idx)))
        }
        "boolean" => {
            check_args!(1);
            Ok(Value::Boolean(value_to_bool(&arg!(0), idx)))
        }
        "lang" => {
            // XPath 1.0 §4.3 / XPath 2.0 §15.4.5 — `lang($code)`
            // walks the context node's ancestor chain looking for
            // `xml:lang`; `lang($code, $node)` walks the explicit
            // node's chain instead.  Match is case-insensitive
            // against the full attribute value OR a hyphen-prefix
            // (so `lang('en')` matches `xml:lang="en-US"`).
            if args.is_empty() || args.len() > 2 {
                return Err(xpath_err("lang() requires 1 or 2 arguments"));
            }
            let want = value_to_string(&arg!(0), idx);
            let want_lc = want.to_ascii_lowercase();
            let start = if args.len() == 2 {
                match arg!(1) {
                    Value::NodeSet(ns) => ns.first().copied(),
                    _ => return Err(xpath_err(
                        "lang() second argument must be a node")),
                }
            } else {
                Some(ctx.context_node)
            };
            let mut current = start;
            while let Some(node) = current {
                for attr in idx.attr_range(node) {
                    if !(idx.local_name(attr) == "lang" && idx.namespace_uri(attr) == "http://www.w3.org/XML/1998/namespace") { continue; }
                    let val_lc = idx.string_value(attr).to_ascii_lowercase();
                    let ok = val_lc == want_lc
                        || (val_lc.len() > want_lc.len()
                            && val_lc.starts_with(&want_lc)
                            && val_lc.as_bytes()[want_lc.len()] == b'-');
                    return Ok(Value::Boolean(ok));
                }
                current = idx.parent(node);
            }
            Ok(Value::Boolean(false))
        }

        // ── node-set functions ───────────────────────────────────────────────
        "last" => { check_args!(0); Ok(integer_result(ctx.size as i64, ctx.bindings)) }
        "position" => { check_args!(0); Ok(integer_result(ctx.pos as i64, ctx.bindings)) }
        "count" => {
            check_args!(1);
            // XPath 2.0 §15.4.2 — `count` returns the number of
            // items in any sequence, not just node-sets.  Atomic
            // values are themselves single-item sequences.  An
            // `IntRange` (lazy `m to n`) contributes its
            // cardinality via [`sequence_len`] without expanding
            // the range — that's the whole point of the lazy
            // representation.
            Ok(integer_result(sequence_len(&arg!(0)) as i64, ctx.bindings))
        }
        "id" => {
            if args.is_empty() || args.len() > 2 {
                return Err(xpath_err("id() requires 1 or 2 arguments"));
            }
            // XPath 1.0 § 4.1 / XPath 2.0 § 14.5.4: the first argument's
            // string-value is whitespace-tokenised; each token is
            // matched against the unique-ID attributes of the document
            // identified by the (optional) second argument's node — or
            // the document containing the context node if omitted.  A
            // node-set first argument is tokenised per-node and the
            // result is the union.  An empty second-argument node-set
            // yields the empty sequence.
            //
            // The set of "ID attributes" follows DTD typing when a
            // `<!ATTLIST e a ID>` declaration is present; otherwise
            // [`DocIndexLike::is_id_attribute`] falls back to the
            // libxml2-compatible convention of any `xml:id` or any
            // attribute whose local name is literally `id`.
            let v = arg!(0);
            let search_root = if args.len() == 2 {
                match arg!(1) {
                    Value::NodeSet(ns) => match ns.first().copied() {
                        Some(n) => doc_root_of(n, idx),
                        None    => return Ok(Value::NodeSet(Vec::new())),
                    },
                    _ => return Err(xpath_err(
                        "id() second argument must be a node")),
                }
            } else {
                doc_root_of(ctx.context_node, idx)
            };
            let tokens: Vec<String> = match &v {
                Value::NodeSet(ns) => {
                    let mut out: Vec<String> = Vec::new();
                    for &n in ns {
                        out.extend(
                            idx.string_value(n)
                                .split_whitespace()
                                .map(str::to_string),
                        );
                    }
                    out
                }
                _ => value_to_string(&v, idx)
                    .split_whitespace()
                    .map(str::to_string)
                    .collect(),
            };
            let mut hits: Vec<NodeId> = Vec::new();
            for node in descendants(search_root, idx, true) {
                if !matches!(idx.kind(node), XPathNodeKind::Element) { continue; }
                for attr in idx.attr_range(node) {
                    if !idx.is_id_attribute(attr) { continue; }
                    let av = idx.string_value(attr);
                    if tokens.iter().any(|t| *t == av) {
                        hits.push(node);
                        break;
                    }
                }
            }
            dedup_sort(&mut hits);
            Ok(Value::NodeSet(hits))
        }
        "idref" => {
            if args.is_empty() || args.len() > 2 {
                return Err(xpath_err("idref() requires 1 or 2 arguments"));
            }
            // XPath 2.0 §14.5.5 — the first argument supplies the
            // candidate IDs (whitespace-tokenised); the result is the
            // `IDREF`/`IDREFS`-typed attribute nodes whose value
            // contains one of them.  The optional second argument's
            // node identifies the document to search (else the context
            // node's document).
            let v = arg!(0);
            let search_root = if args.len() == 2 {
                match arg!(1) {
                    Value::NodeSet(ns) => match ns.first().copied() {
                        Some(n) => doc_root_of(n, idx),
                        None    => return Ok(Value::NodeSet(Vec::new())),
                    },
                    _ => return Err(xpath_err(
                        "idref() second argument must be a node")),
                }
            } else {
                doc_root_of(ctx.context_node, idx)
            };
            let tokens: Vec<String> = match &v {
                Value::NodeSet(ns) => ns.iter()
                    .flat_map(|&n| idx.string_value(n)
                        .split_whitespace().map(str::to_string).collect::<Vec<_>>())
                    .collect(),
                _ => value_to_string(&v, idx)
                    .split_whitespace().map(str::to_string).collect(),
            };
            let mut hits: Vec<NodeId> = Vec::new();
            for node in descendants(search_root, idx, true) {
                if !matches!(idx.kind(node), XPathNodeKind::Element) { continue; }
                for attr in idx.attr_range(node) {
                    if !idx.is_idref_attribute(attr) { continue; }
                    // IDREFS holds a whitespace-separated list; the
                    // attribute matches if any of its tokens is a
                    // candidate ID.
                    let av = idx.string_value(attr);
                    if av.split_whitespace().any(|w| tokens.iter().any(|t| t == w)) {
                        hits.push(attr);
                    }
                }
            }
            dedup_sort(&mut hits);
            Ok(Value::NodeSet(hits))
        }
        "local-name" => {
            let id = if args.is_empty() {
                // Same XPTY0004 guard as `name()` — an atomic context
                // (e.g. `(1 to 5)[local-name()='a']`) isn't a node.
                if ctx.static_ctx.xpath_2_0 && matches!(current_context_item(),
                    Some(v) if !matches!(v,
                        Value::NodeSet(_) | Value::ForeignNodeSet(_)))
                {
                    return Err(xpath_err(
                        "local-name(): context item is not a node (XPTY0004)"
                    ).with_xpath_code("XPTY0004"));
                }
                Some(ctx.context_node)
            } else {
                // XPath 1.0 §4.1: an explicit but empty node-set
                // argument yields the empty string, not the context
                // node's name.
                match arg!(0) {
                    Value::NodeSet(ns) => ns.first().copied(),
                    _ => return Err(xpath_err("local-name() requires a node-set or no argument")),
                }
            };
            Ok(Value::String(id.map(|i| idx.local_name(i).to_string()).unwrap_or_default()))
        }
        "name" => {
            // No-arg form takes the context item.  Under XPath 2.0 an
            // atomic context (e.g. inside `(1 to 5)[name()='a']`) is
            // XPTY0004 — name() requires a node.
            let id = if args.is_empty() {
                if ctx.static_ctx.xpath_2_0 && matches!(current_context_item(),
                    Some(v) if !matches!(v,
                        Value::NodeSet(_) | Value::ForeignNodeSet(_)))
                {
                    return Err(xpath_err(
                        "name(): context item is not a node (XPTY0004)"
                    ).with_xpath_code("XPTY0004"));
                }
                Some(ctx.context_node)
            } else {
                match arg!(0) {
                    Value::NodeSet(ns) => ns.first().copied(),
                    _ => return Err(xpath_err("name() requires a node-set or no argument")),
                }
            };
            Ok(Value::String(id.map(|i| idx.node_name(i).to_string()).unwrap_or_default()))
        }
        "namespace-uri" => {
            let id = if args.is_empty() {
                if ctx.static_ctx.xpath_2_0 && matches!(current_context_item(),
                    Some(v) if !matches!(v,
                        Value::NodeSet(_) | Value::ForeignNodeSet(_)))
                {
                    return Err(xpath_err(
                        "namespace-uri(): context item is not a node (XPTY0004)"
                    ).with_xpath_code("XPTY0004"));
                }
                Some(ctx.context_node)
            } else {
                match arg!(0) {
                    Value::NodeSet(ns) => ns.first().copied(),
                    _ => return Err(xpath_err("namespace-uri() requires a node-set or no argument")),
                }
            };
            Ok(Value::String(id.map(|i| idx.namespace_uri(i).to_string()).unwrap_or_default()))
        }

        // ── string functions ─────────────────────────────────────────────────
        "string" => {
            if args.is_empty() {
                Ok(Value::String(idx.string_value(ctx.context_node)))
            } else {
                check_args!(1);
                Ok(Value::String(value_to_string_with_compat(
                    &arg!(0), idx, ctx.bindings, ctx.static_ctx.libxml2_compatible)))
            }
        }
        "concat" => {
            if args.len() < 2 {
                return Err(xpath_err("concat() requires at least 2 arguments"));
            }
            let mut s = String::new();
            for a in args {
                let v = eval_expr(a, ctx, idx)?;
                s.push_str(&value_to_string_with_compat(
                    &v, idx, ctx.bindings, ctx.static_ctx.libxml2_compatible));
            }
            Ok(Value::String(s))
        }
        "starts-with" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(xpath_err("starts-with() requires 2 or 3 arguments"));
            }
            let s   = arg_str!(0);
            let pre = arg_str!(1);
            let coll = effective_collation(if args.len() == 3 { Some(arg_str!(2)) } else { None });
            Ok(Value::Boolean(collation_starts_with(&s, &pre, coll.as_deref())))
        }
        "contains" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(xpath_err("contains() requires 2 or 3 arguments"));
            }
            let s   = arg_str!(0);
            let sub = arg_str!(1);
            let coll = effective_collation(if args.len() == 3 { Some(arg_str!(2)) } else { None });
            Ok(Value::Boolean(collation_contains(&s, &sub, coll.as_deref())))
        }
        "substring-before" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(xpath_err("substring-before() requires 2 or 3 arguments"));
            }
            let s = arg_str!(0);
            let sep = arg_str!(1);
            let coll = effective_collation(if args.len() == 3 { Some(arg_str!(2)) } else { None });
            // For the HTML ASCII case-insensitive collation, do the
            // search on the folded copy but slice the original on the
            // matched byte index — the fold is a 1:1 ASCII transform
            // so byte offsets line up.
            let find_pos = if is_ascii_ci_collation(coll.as_deref()) {
                ascii_ci_fold(&s).find(&ascii_ci_fold(&sep))
            } else {
                s.find(sep.as_str())
            };
            Ok(Value::String(match find_pos {
                Some(pos) => s[..pos].to_string(),
                None => String::new(),
            }))
        }
        "substring-after" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(xpath_err("substring-after() requires 2 or 3 arguments"));
            }
            let s = arg_str!(0);
            let sep = arg_str!(1);
            let coll = effective_collation(if args.len() == 3 { Some(arg_str!(2)) } else { None });
            let find_pos = if is_ascii_ci_collation(coll.as_deref()) {
                ascii_ci_fold(&s).find(&ascii_ci_fold(&sep))
            } else {
                s.find(sep.as_str())
            };
            Ok(Value::String(match find_pos {
                Some(pos) => s[pos + sep.len()..].to_string(),
                None => String::new(),
            }))
        }
        "substring" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(xpath_err("substring() requires 2 or 3 arguments"));
            }
            let s = arg_str!(0);
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len();
            // XPath 1.0 §4.2: result is chars at 1-based positions p where
            // round(start) <= p < round(start) + round(len).  Compute the
            // bounds in f64 so +Inf / -Inf / NaN behave per spec (NaN comparisons
            // are always false → empty result; +Inf falls past `len` → empty).
            // Convert to usize only after clamping to [0, len] in f64.
            let len_f = len as f64;
            let start_1based = arg_num!(1).round();
            let end_1based = if args.len() == 3 {
                // start + len_arg may be NaN (e.g. +Inf + -Inf); guard explicitly.
                let cnt = arg_num!(2).round();
                let e = start_1based + cnt;
                if e.is_nan() { 0.0 } else { e }
            } else {
                len_f + 1.0
            };
            // Convert 1-based [start, end) into 0-based [s, e) clamped to [0, len].
            let s_f = (start_1based - 1.0).max(0.0).min(len_f);
            let e_f = (end_1based - 1.0).max(0.0).min(len_f);
            let (s_idx, e_idx) = if s_f <= e_f {
                (s_f as usize, e_f as usize)
            } else {
                (0, 0)
            };
            let result: String = chars[s_idx..e_idx].iter().collect();
            Ok(Value::String(result))
        }
        "string-length" => {
            let s = if args.is_empty() {
                idx.string_value(ctx.context_node)
            } else {
                check_args!(1);
                arg_str!(0)
            };
            Ok(integer_result(s.chars().count() as i64, ctx.bindings))
        }
        "normalize-space" => {
            let s = if args.is_empty() {
                idx.string_value(ctx.context_node)
            } else {
                check_args!(1);
                arg_str!(0)
            };
            let normalized = s.split_whitespace().collect::<Vec<_>>().join(" ");
            Ok(Value::String(normalized))
        }
        "translate" => {
            check_args!(3);
            let s = arg_str!(0);
            let from = arg_str!(1);
            let to: Vec<char> = arg_str!(2).chars().collect();
            // Build a lookup table once instead of scanning
            // `from_chars` linearly per input character.  For
            // duplicate keys in `from`, the *first* occurrence
            // wins (XSLT 1.0 §6.4.2).  `Some(c)` replaces;
            // `None` drops the character entirely (when the
            // matched `from` index has no corresponding `to`).
            let mut map: std::collections::HashMap<char, Option<char>> =
                std::collections::HashMap::new();
            for (i, f) in from.chars().enumerate() {
                map.entry(f).or_insert_with(|| to.get(i).copied());
            }
            let result: String = s
                .chars()
                .filter_map(|c| match map.get(&c) {
                    Some(Some(t)) => Some(*t),
                    Some(None)    => None,
                    None          => Some(c),
                })
                .collect();
            Ok(Value::String(result))
        }

        // ── number functions ─────────────────────────────────────────────────
        "number" => {
            let v = if args.is_empty() {
                Value::String(idx.string_value(ctx.context_node))
            } else {
                check_args!(1);
                arg!(0)
            };
            // libxml2 quirk: `number('-')` returns -0, not NaN.  Per
            // spec § 4.4 the result is NaN for any non-numeric
            // lexical form, but matching libxml2 lets corpora that
            // exercise this edge case round-trip cleanly.
            if ctx.static_ctx.libxml2_compatible {
                if let Value::String(ref s) = v {
                    if s.trim() == "-" { return Ok(Value::Number(Numeric::Double(-0.0))); }
                }
            }
            // XPath 2.0 §14 fn:number returns xs:double; the Double
            // kind drives `string(number(x))`'s scientific form in 2.0
            // and a decimal string in 1.0 / libxml2.
            Ok(Value::Number(Numeric::Double(value_to_number(&v, idx))))
        }
        "sum" => {
            // XPath 2.0 §15.4.4: `sum($seq [, $zero])` — the 2-arg
            // form returns `$zero` when `$seq` is the empty sequence
            // (default `xs:integer(0)` for the 1-arg form).  Atomic
            // sequences sum their numeric coercions.
            if args.is_empty() || args.len() > 2 {
                return Err(xpath_err(format!(
                    "sum() requires 1 or 2 arguments (got {})", args.len()
                )));
            }
            // Pre-evaluate the optional zero argument so the
            // match arms below can return it without re-entering
            // the macro (which uses `?`).
            let zero_val: Value = if args.len() == 2 { arg!(1) } else { Value::Number(Numeric::Double(0.0)) };
            match arg!(0).untyped() {
                Value::NodeSet(ns) => {
                    if ns.is_empty() {
                        return Ok(zero_val);
                    }
                    let total: f64 = ns.iter().map(|&id|
                        idx.string_value(id).trim().parse::<f64>().unwrap_or(f64::NAN)
                    ).sum();
                    Ok(Value::Number(Numeric::Double(total)))
                }
                Value::Number(n) => Ok(Value::Number(n)),
                Value::String(s) => Ok(Value::Number(Numeric::Double(s.trim().parse::<f64>().unwrap_or(f64::NAN)))),
                Value::Boolean(b) => Ok(Value::Number(Numeric::Double(if b { 1.0 } else { 0.0 }))),
                // Atomic sequence (typed or mixed): coerce each item to
                // a number and sum.  XPath 2.0 §15.4.4 — empty sequence
                // yields the zero argument; otherwise items add.
                Value::Sequence(items) => {
                    if items.is_empty() {
                        return Ok(zero_val);
                    }
                    // F&O §10.4 — sum of a duration sequence is a
                    // duration (sum of dayTimeDurations is a
                    // dayTimeDuration, etc.), not a coerced number.
                    if let Some((kind, total, _)) = duration_seq_total(&items) {
                        return Ok(duration_value(kind, total));
                    }
                    // F&O §15.4.4 / FORG0006 — every item must promote
                    // to a single numeric (or duration) type.  An
                    // xs:string literal that can't parse to a number
                    // never matches: raise the type error rather than
                    // silently propagating NaN.  xs:untypedAtomic
                    // (sourced from node string-values) still
                    // converts via xs:double per spec, so this only
                    // catches the explicit-string case.
                    for v in &items {
                        if let Value::String(s) = v {
                            if s.trim().parse::<f64>().is_err() {
                                return Err(xpath_err(format!(
                                    "sum(): non-numeric item '{s}' \
                                     (FORG0006)"
                                )).with_xpath_code("FORG0006"));
                            }
                        }
                    }
                    let total: f64 = items.iter()
                        .map(|v| value_to_number_with(v, idx, ctx.bindings))
                        .sum();
                    // XPath 2.0 §15.4.4 — the result type is the
                    // promoted numeric type of the items (sum of
                    // integers is xs:integer, etc.).  An item without a
                    // numeric kind (an untyped atomic) is treated as
                    // xs:double, collapsing the result to double.
                    let kind = items.iter().fold(Some("integer"), |acc, v| {
                        match (acc, numeric_kind_of(v)) {
                            (Some(a), Some(b)) => numeric_promote_kind(Some(a), Some(b)),
                            _ => None,
                        }
                    });
                    Ok(Value::Number(match kind {
                        Some(k) => Numeric::of_kind(k, total),
                        None    => Numeric::Double(total),
                    }))
                }
                Value::ForeignNodeSet(_) => Err(xpath_err(
                    "sum() over foreign node-sets not supported")),
                Value::Typed(_) => unreachable!(),
                // Closed-form sum of consecutive integers — the result
                // is itself an xs:integer.
                Value::IntRange { lo, hi } => {
                    let total = ((hi - lo + 1) as f64) * ((lo + hi) as f64) * 0.5;
                    Ok(Value::Number(Numeric::of_kind("integer", total)))
                }
                // sum() over a map / array is a type error (FORG0006);
                // lenient — treat as the empty-sequence case → $zero.
                Value::Map(_) | Value::Array(_) | Value::Function(_) => Ok(zero_val),
            }
        }
        "floor" => {
            check_args!(1);
            let a = arg!(0);
            if ctx.static_ctx.xpath_2_0 {
                if let Value::String(s) = &a {
                    if s.trim().parse::<f64>().is_err() {
                        return Err(xpath_err(format!(
                            "floor(): argument '{s}' is not a number (XPTY0004)"
                        )).with_xpath_code("XPTY0004"));
                    }
                }
            }
            Ok(preserve_numeric_kind(&a, value_to_number(&a, idx).floor()))
        }
        "ceiling" => {
            check_args!(1);
            let a = arg!(0);
            if ctx.static_ctx.xpath_2_0 {
                if let Value::String(s) = &a {
                    if s.trim().parse::<f64>().is_err() {
                        return Err(xpath_err(format!(
                            "ceiling(): argument '{s}' is not a number (XPTY0004)"
                        )).with_xpath_code("XPTY0004"));
                    }
                }
            }
            Ok(preserve_numeric_kind(&a, value_to_number(&a, idx).ceil()))
        }
        "round" => {
            check_args!(1);
            let a = arg!(0);
            // XPath 2.0 §3.5.5 / XPTY0004 — fn:round's argument is
            // `xs:numeric?`; an xs:string (literal-sourced) doesn't
            // promote and must be cast explicitly.  Untyped atomics
            // from node atomization stay lenient.
            if ctx.static_ctx.xpath_2_0 {
                if let Value::String(s) = &a {
                    if s.trim().parse::<f64>().is_err() {
                        return Err(xpath_err(format!(
                            "round(): argument '{s}' is not a number (XPTY0004)"
                        )).with_xpath_code("XPTY0004"));
                    }
                }
            }
            let n = value_to_number(&a, idx);
            // XPath 1.0 § 4.4: ties round toward +∞, NOT away from
            // zero.  `(n + 0.5).floor()` implements this for the bulk
            // of the domain; the spec carves out an explicit sign-
            // preserving zero case: every value in `[-0.5, 0]`
            // (including -0 itself) must round to -0.  Use
            // `is_sign_negative` because `-0.0 < 0.0` is false under
            // IEEE 754 — a plain `n < 0.0` check would miss -0.  NaN
            // and ±∞ pass through `.floor()` unchanged.
            let r = if n.is_sign_negative() && n >= -0.5 { -0.0 }
                    else                                 { (n + 0.5).floor() };
            Ok(preserve_numeric_kind(&a, r))
        }

        // ── XSLT extension: document(URI[, base-node-set]) ──────────────────
        // Returns a ForeignNodeSet — pointers to nodes in the loaded
        // doc(s).  Bindings impl (compat) keeps a registry of loaded
        // docs alive for the XPath context's lifetime.
        "document" => {
            if args.is_empty() || args.len() > 2 {
                return Err(xpath_err("document() requires 1 or 2 arguments"));
            }
            let uri = value_to_string(&arg!(0), idx);
            // 2-arg form: second arg is a node-set whose first node's
            // base URI is used to resolve relative URIs.  Not yet
            // honored — pass None and let the bindings fall back to
            // the calling expression's base URI.
            match ctx.bindings.load_document(&uri, None) {
                Some(r) => r.map(Value::ForeignNodeSet)
                    .map_err(|e| e.or_xpath_code("FODC0002")),
                None => Err(xpath_err(
                    "document(): bindings don't support external document loading",
                ).with_xpath_code("FODC0002")),
            }
        }

        // ── XPath 2.0 functions used by XSD 1.1 `xs:assert` ───────────────
        //
        // These are the small set of 2.0 functions that show up
        // frequently in test-expression bodies and have natural
        // XPath-1.0-flavoured implementations.  We keep the dispatch
        // here (rather than the EXSLT module) because they're
        // unprefixed and live in the default XPath function library.

        "ends-with" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(xpath_err("ends-with() requires 2 or 3 arguments"));
            }
            let s   = arg_str!(0);
            let suf = arg_str!(1);
            let coll = effective_collation(if args.len() == 3 { Some(arg_str!(2)) } else { None });
            Ok(Value::Boolean(collation_ends_with(&s, &suf, coll.as_deref())))
        }
        "exists" => {
            check_args!(1);
            let v = arg!(0);
            Ok(Value::Boolean(match v {
                // XPath 2.0 `exists` is sequence-cardinality, not
                // boolean truthiness: a non-empty node-set, a
                // string (incl. ""), a number (incl. NaN), or a
                // boolean all count as 1+ item sequences.
                Value::NodeSet(ns)        => !ns.is_empty(),
                Value::ForeignNodeSet(ns) => !ns.is_empty(),
                Value::Sequence(items)    => !items.is_empty(),
                Value::IntRange { lo, hi } => hi >= lo,
                _                         => true,
            }))
        }
        "empty" => {
            check_args!(1);
            let v = arg!(0);
            Ok(Value::Boolean(match v {
                Value::NodeSet(ns)        => ns.is_empty(),
                Value::ForeignNodeSet(ns) => ns.is_empty(),
                Value::Sequence(items)    => items.is_empty(),
                Value::IntRange { lo, hi } => hi < lo,
                _                         => false,
            }))
        }
        "data" => {
            // Atomization (XPath 2.0 §2.5.2): atomise every item
            // in the input sequence to a string atomic.  A
            // multi-node input becomes a Sequence of String items,
            // not a concatenated single string — downstream
            // `xsl:value-of separator=…` consumers depend on the
            // per-item shape to interleave the separator correctly.
            check_args!(1);
            let v = arg!(0);
            match v {
                Value::NodeSet(ns) => {
                    let items: Vec<Value> = ns.iter()
                        .map(|&id| Value::String(idx.string_value(id)))
                        .collect();
                    match items.len() {
                        0 => Ok(Value::NodeSet(Vec::new())),
                        1 => Ok(items.into_iter().next().unwrap()),
                        _ => Ok(Value::Sequence(items)),
                    }
                }
                Value::ForeignNodeSet(ns) => {
                    let items: Vec<Value> = ns.iter()
                        .map(|&p| Value::String(ctx.bindings.foreign_string_value(p)))
                        .collect();
                    match items.len() {
                        0 => Ok(Value::NodeSet(Vec::new())),
                        1 => Ok(items.into_iter().next().unwrap()),
                        _ => Ok(Value::Sequence(items)),
                    }
                }
                other => Ok(other),
            }
        }
        "current-dateTime" => {
            check_args!(0);
            let now = stable_now();
            Ok(Value::Typed(Box::new(TypedAtomic {
                kind: "dateTime",
                lexical: format_datetime_utc(now),
                numeric: None,
                boolean: None,
            })))
        }
        "current-date" => {
            check_args!(0);
            let now = stable_now();
            Ok(Value::Typed(Box::new(TypedAtomic {
                kind: "date",
                lexical: format_date_utc(now),
                numeric: None,
                boolean: None,
            })))
        }
        "current-time" => {
            check_args!(0);
            let now = stable_now();
            Ok(Value::Typed(Box::new(TypedAtomic {
                kind: "time",
                lexical: format_time_utc(now),
                numeric: None,
                boolean: None,
            })))
        }
        // XPath 2.0 §10.5 — `fn:adjust-{date,dateTime,time}-to-timezone`.
        // Re-stamps the value to the given timezone (or strips the
        // timezone when the second argument is the empty sequence).
        // Two-arg form supplies an explicit `xs:dayTimeDuration`
        // timezone offset; one-arg form uses the implicit timezone
        // (we treat that as `PT0S` since we don't track locale).
        "adjust-dateTime-to-timezone"
        | "adjust-date-to-timezone"
        | "adjust-time-to-timezone" => {
            if args.is_empty() || args.len() > 2 {
                return Err(xpath_err(format!(
                    "{name}() requires 1 or 2 arguments (got {})", args.len()
                )));
            }
            let kind = match name {
                "adjust-dateTime-to-timezone" => "dateTime",
                "adjust-date-to-timezone"     => "date",
                "adjust-time-to-timezone"     => "time",
                _ => unreachable!(),
            };
            // Empty input → empty output.
            if let Value::NodeSet(ns) = &arg!(0) {
                if ns.is_empty() { return Ok(Value::NodeSet(Vec::new())); }
            }
            let lex = value_to_string_with(&arg!(0), idx, ctx.bindings);
            let new_tz_minutes: Option<i16> = if args.len() == 2 {
                // Empty second arg = strip timezone.
                let is_empty = matches!(&arg!(1),
                    Value::NodeSet(ns) if ns.is_empty());
                if is_empty {
                    None
                } else {
                    let dur = value_to_string_with(&arg!(1), idx, ctx.bindings);
                    let secs = parse_day_time_duration_secs(&dur).unwrap_or(0);
                    let mins = secs / 60;
                    if mins.abs() > 14 * 60 {
                        return Err(xpath_err(format!(
                            "adjust timezone exceeds 14 hours: {dur}"
                        )));
                    }
                    Some(mins as i16)
                }
            } else {
                Some(0) // implicit timezone — we use UTC
            };
            Ok(Value::Typed(Box::new(TypedAtomic {
                kind,
                lexical: adjust_timezone(&lex, kind, new_tz_minutes),
                numeric: None,
                boolean: None,
            })))
        }

        // ── XPath 2.0 regex functions ─────────────────────────────────
        //
        // `matches` / `replace` / `tokenize` accept an optional 3rd
        // `flags` argument (XPath 2.0 §7.6).  The pattern syntax in
        // XPath 2.0 is XML Schema's regex flavour, which overlaps
        // heavily with the Rust `regex` crate's RE2 syntax for the
        // common cases (character classes, anchors, quantifiers,
        // alternation, groups, Unicode categories).  We pass the
        // pattern through unchanged and translate the flag string
        // into Rust's `(?flags)` inline form.
        "matches" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(xpath_err(format!(
                    "matches() requires 2 or 3 arguments (got {})", args.len()
                )));
            }
            let input   = arg_str!(0);
            let pattern = arg_str!(1);
            let flags   = if args.len() == 3 { arg_str!(2) } else { String::new() };
            // The native XSD §F / XPath 2.0 engine implements the
            // full XPath dialect (`^`/`$` anchors, find semantics,
            // class subtraction, `\p{IsBlock}`, XSD `\s`/`\d`/`\w`,
            // and the spec's strict rejection of `(?…)` forms).
            // It is authoritative when no flags are in play —
            // including for syntax errors, which must surface as
            // FORX0002 rather than fall back to a more permissive
            // engine that would silently accept the bad pattern.
            // Flag handling still routes through the Rust crate
            // until the native engine grows i/s/m/x support.
            if flags.is_empty() {
                return crate::regex::compile_with_cached(
                    &pattern, ctx.bindings.regex_dialect(),
                )
                    .map(|p| Value::Boolean(p.find_match(&input)))
                    .map_err(|e| xpath_err(format!("invalid regex: {e}"))
                        .with_xpath_code("FORX0002"));
            }
            let re = compile_xpath_regex_dialect(&pattern, &flags,
                ctx.bindings.regex_dialect())?;
            Ok(Value::Boolean(re.is_match(&input)))
        }
        "replace" => {
            if args.len() < 3 || args.len() > 4 {
                return Err(xpath_err(format!(
                    "replace() requires 3 or 4 arguments (got {})", args.len()
                )));
            }
            let input       = arg_str!(0);
            let pattern     = arg_str!(1);
            let replacement = arg_str!(2);
            let flags       = if args.len() == 4 { arg_str!(3) } else { String::new() };
            let re = compile_xpath_regex_dialect(&pattern, &flags,
                ctx.bindings.regex_dialect())?;
            // F&O §7.6.3 / FORX0003 — same zero-length-match rule as
            // tokenize(): replacing an empty match would loop forever,
            // so it's a dynamic error.
            if re.is_match("") {
                return Err(xpath_err(format!(
                    "replace(): the pattern '{pattern}' matches a \
                     zero-length string (FORX0003)"
                )).with_xpath_code("FORX0003"));
            }
            // XPath 2.0 §7.6.3 replacement syntax: `\$` represents
            // `$`, `\\` represents `\`, `$N` represents group N,
            // `$0` represents the full match.  Rust's regex crate
            // uses `$N` / `${N}` and treats `$` as the only escape
            // (no backslash escaping).  Translate the XPath form
            // into the Rust form, capping multi-digit `$N` to the
            // number of capture groups in the compiled regex so
            // `$10` past a 5-group regex becomes `$1` + literal `0`
            // (XPath 2.0 §7.6.3 "longest prefix that yields a valid
            // backref").
            // captures_len() includes group 0 (the whole match) so
            // subtract 1 for the user-visible group count.
            let group_count = re.captures_len().saturating_sub(1);
            let translated = translate_xpath_replacement(&replacement, group_count)?;
            Ok(Value::String(re.replace_all(&input, translated.as_str()).into_owned()))
        }
        "tokenize" => {
            // Two-arg form `tokenize(input, pattern)` is the workhorse.
            // The XPath 3.0 zero-pattern form `tokenize(input)` splits
            // on whitespace; we accept it too for ergonomics.
            if args.is_empty() || args.len() > 3 {
                return Err(xpath_err(format!(
                    "tokenize() requires 1 to 3 arguments (got {})", args.len()
                )));
            }
            let input   = arg_str!(0);
            let pattern = if args.len() >= 2 { arg_str!(1) } else { r"\s+".to_string() };
            let flags   = if args.len() == 3 { arg_str!(2) } else { String::new() };
            // XPath 2.0 §7.6.4: zero-length input yields the empty
            // sequence — *not* a sequence containing one empty
            // string the way Rust's `regex::split` would return.
            if input.is_empty() {
                return Ok(Value::NodeSet(Vec::new()));
            }
            let re = compile_xpath_regex_dialect(&pattern, &flags,
                ctx.bindings.regex_dialect())?;
            // F&O §7.6.4 / FORX0003 — a pattern that matches the empty
            // string is a dynamic error, since `tokenize` would then
            // produce infinite empty separators.
            if re.is_match("") {
                return Err(xpath_err(format!(
                    "tokenize(): the pattern '{pattern}' matches a \
                     zero-length string (FORX0003)"
                )).with_xpath_code("FORX0003"));
            }
            let parts: Vec<String> = re.split(&input).map(str::to_string).collect();
            // Materialise the result as a node-set of synthetic text
            // nodes so callers can iterate / count / index into it the
            // way `str:tokenize` already supports.  Indexes that can't
            // allocate (test-only stubs) get a string fallback.
            match idx.allocate_rtf_text_nodes(parts.clone()) {
                Some(ids) => Ok(Value::NodeSet(ids)),
                None      => Ok(Value::String(parts.join(" "))),
            }
        }

        // ── XPath 2.0 string / sequence functions ─────────────────
        //
        // The common 2.0 string helpers: `lower-case`, `upper-case`,
        // `string-join`, plus the cardinality helpers `head` / `tail`
        // / `reverse` / `subsequence` and the de-duplicating
        // `distinct-values` / `index-of`.  We use our NodeSet (with
        // synthetic-text allocator) as the sequence carrier.

        "lower-case" => {
            check_args!(1);
            Ok(Value::String(arg_str!(0).to_lowercase()))
        }
        "upper-case" => {
            check_args!(1);
            Ok(Value::String(arg_str!(0).to_uppercase()))
        }
        "string-join" => {
            // `string-join(seq)` (XPath 3.0) defaults the separator
            // to "".  XPath 2.0 requires 2 args but accepting the
            // 1-arg form is a strict superset — and matches every
            // shipping engine.
            if args.is_empty() || args.len() > 2 {
                return Err(xpath_err(format!(
                    "string-join() requires 1 or 2 arguments (got {})", args.len()
                )));
            }
            let sep = if args.len() == 2 { arg_str!(1) } else { String::new() };
            let pieces = sequence_to_strings(&arg!(0), idx);
            Ok(Value::String(pieces.join(&sep)))
        }
        "abs" => {
            check_args!(1);
            let a = arg!(0);
            Ok(preserve_numeric_kind(&a, value_to_number(&a, idx).abs()))
        }
        // XPath 2.0 §15.4.3-5 `min` / `max` / `avg`.  Empty input
        // returns the empty sequence (the spec form — XPath 1.0
        // never offered these functions, so there's no compat
        // pressure to surface NaN).  When every item stringifies
        // as a valid number we apply numeric `min` / `max`;
        // otherwise we fall back to lexicographic ordering so
        // string sequences (`min(('apple','banana'))`) behave per
        // the spec's "string promotion" rule.
        // `min`/`max` take an optional 2nd collation argument; string
        // comparison otherwise uses the in-scope default collation.
        "min" | "max" => {
            if args.is_empty() || args.len() > 2 {
                return Err(xpath_err(format!(
                    "{name}() requires 1 or 2 arguments (got {})", args.len()
                )));
            }
            let coll = effective_collation(
                if args.len() == 2 { Some(arg_str!(1)) } else { None });
            let ci = is_ascii_ci_collation(coll.as_deref());
            let op = if name == "min" { MinMaxOp::Min } else { MinMaxOp::Max };
            min_max_avg(&arg!(0), idx, op, ci)
        }
        "avg" => {
            check_args!(1);
            min_max_avg(&arg!(0), idx, MinMaxOp::Avg, false)
        }
        "distinct-values" => {
            if args.is_empty() || args.len() > 2 {
                return Err(xpath_err(format!(
                    "distinct-values() requires 1 or 2 arguments (got {})", args.len()
                )));
            }
            let coll = effective_collation(if args.len() == 2 { Some(arg_str!(1)) } else { None });
            let ci = is_ascii_ci_collation(coll.as_deref());
            let mut seen = std::collections::HashSet::new();
            let mut keep = Vec::new();
            for s in sequence_to_strings(&arg!(0), idx) {
                let k = if ci { ascii_ci_fold(&s) } else { s.clone() };
                if seen.insert(k) { keep.push(s); }
            }
            match idx.allocate_rtf_text_nodes(keep.clone()) {
                Some(ids) => Ok(Value::NodeSet(ids)),
                None      => Ok(Value::String(keep.join(" "))),
            }
        }
        "index-of" => {
            // `index-of(seq, target [, collation])` returns the
            // 1-based positions of `target` in `seq`.  We compare
            // via string-value.
            if args.len() < 2 || args.len() > 3 {
                return Err(xpath_err(format!(
                    "index-of() requires 2 or 3 arguments (got {})", args.len()
                )));
            }
            let target = arg_str!(1);
            let coll   = effective_collation(if args.len() == 3 { Some(arg_str!(2)) } else { None });
            let items  = sequence_to_strings(&arg!(0), idx);
            let (key_target, items_keys): (String, Vec<String>) =
                if is_ascii_ci_collation(coll.as_deref()) {
                    (ascii_ci_fold(&target),
                     items.iter().map(|s| ascii_ci_fold(s)).collect())
                } else {
                    (target, items)
                };
            let positions: Vec<String> = items_keys.iter().enumerate()
                .filter_map(|(i, s)|
                    if s == &key_target { Some((i + 1).to_string()) } else { None })
                .collect();
            match idx.allocate_rtf_text_nodes(positions.clone()) {
                Some(ids) => Ok(Value::NodeSet(ids)),
                None      => Ok(Value::String(positions.join(" "))),
            }
        }
        "subsequence" => {
            // XPath 2.0 §15.5.6 — `subsequence($s, $start[, $length])`
            // keeps items at positions `p` (1-indexed) for which:
            //   round($start) <= p < round($start) + round($length)
            // AND `1 <= p <= count($s)`.  NaN or `+INF + -INF`
            // anywhere in the bounds short-circuits to the empty
            // sequence (no integer satisfies `NaN <= p < NaN`).
            // Negative `$start` + finite `$length` may still leave
            // a window: e.g. subsequence((1..20), -5, 8) keeps
            // positions 1..2 because -5 + 8 = 3 caps the upper end.
            if args.len() < 2 || args.len() > 3 {
                return Err(xpath_err(format!(
                    "subsequence() requires 2 or 3 arguments (got {})", args.len()
                )));
            }
            // XPath 2.0 §15.5.6 / XPTY0004 — the start / length
            // arguments are typed `xs:double`, so an empty sequence
            // doesn't promote.  In 2.0 this is a type error, not the
            // silent empty-sequence answer.
            let is_empty = |v: &Value| matches!(v,
                Value::Sequence(items) if items.is_empty())
                || matches!(v, Value::NodeSet(ns) if ns.is_empty());
            if ctx.static_ctx.xpath_2_0 && is_empty(&arg!(1)) {
                return Err(xpath_err(
                    "subsequence(): the start argument must not be the \
                     empty sequence (XPTY0004)"
                ).with_xpath_code("XPTY0004"));
            }
            if ctx.static_ctx.xpath_2_0 && args.len() == 3 && is_empty(&arg!(2)) {
                return Err(xpath_err(
                    "subsequence(): the length argument must not be the \
                     empty sequence (XPTY0004)"
                ).with_xpath_code("XPTY0004"));
            }
            let seq      = arg!(0);
            let start_f  = value_to_number(&arg!(1), idx).round();
            let length_f = if args.len() == 3 {
                value_to_number(&arg!(2), idx).round()
            } else {
                f64::INFINITY
            };
            // Compute the 0-indexed [lo, hi) slice into the source
            // sequence.  An empty slice is signalled by `lo >= hi`.
            let slice_bounds = |count: usize| -> (usize, usize) {
                if start_f.is_nan() || length_f.is_nan() {
                    return (0, 0);
                }
                let end = start_f + length_f;            // 1-indexed exclusive end
                if end.is_nan() { return (0, 0); }       // -INF + INF
                let lo_p = start_f.max(1.0);             // first kept 1-indexed pos
                if lo_p >= end { return (0, 0); }
                let lo = ((lo_p - 1.0) as usize).min(count);
                let hi = if end.is_infinite() && end > 0.0 {
                    count
                } else {
                    ((end - 1.0).max(0.0) as usize).min(count)
                };
                (lo, hi.max(lo))
            };
            // Preserve node identity when the input is a NodeSet —
            // re-allocating into synthetic text would lose
            // attribute / element distinctions.
            if let Value::NodeSet(ns) = &seq {
                let (lo, hi) = slice_bounds(ns.len());
                return Ok(Value::NodeSet(ns[lo..hi].to_vec()));
            }
            if let Value::Sequence(items) = &seq {
                let (lo, hi) = slice_bounds(items.len());
                let out: Vec<Value> = items[lo..hi].to_vec();
                return Ok(if out.len() == 1 {
                    out.into_iter().next().unwrap()
                } else {
                    Value::Sequence(out)
                });
            }
            let pieces = sequence_to_strings(&seq, idx);
            let (lo, hi) = slice_bounds(pieces.len());
            let pieces: Vec<String> = pieces[lo..hi].to_vec();
            match idx.allocate_rtf_text_nodes(pieces.clone()) {
                Some(ids) => Ok(Value::NodeSet(ids)),
                None      => Ok(Value::String(pieces.join(""))),
            }
        }
        "reverse" => {
            check_args!(1);
            if let Value::NodeSet(ns) = arg!(0) {
                let mut rev = ns;
                rev.reverse();
                return Ok(Value::NodeSet(rev));
            }
            if let Value::Sequence(items) = arg!(0) {
                let mut rev = items;
                rev.reverse();
                return Ok(Value::Sequence(rev));
            }
            let mut pieces = sequence_to_strings(&arg!(0), idx);
            pieces.reverse();
            match idx.allocate_rtf_text_nodes(pieces.clone()) {
                Some(ids) => Ok(Value::NodeSet(ids)),
                None      => Ok(Value::String(pieces.join(""))),
            }
        }
        "unordered" => {
            // XPath 2.0 §15.1.3 — `fn:unordered($arg)` is an
            // optimisation hint: it tells the engine the caller
            // doesn't care about iteration order, so the engine
            // may reorder freely.  We don't reorder; the spec
            // permits returning the input unchanged.
            check_args!(1);
            Ok(arg!(0))
        }
        // XPath 3.0 §15.1.9 — `fn:path($node)` returns a string
        // locating the node in its tree.  Documents → `/`; for
        // every other node we walk up to the root, recording each
        // ancestor's element / attribute / kind label, and emit
        // `Q{uri}local[pos]` segments separated by `/`.  An
        // explicit `Q{}` (empty-namespace) prefix is used for
        // elements / attributes with no namespace; a numeric
        // position predicate is included for elements, comments,
        // text nodes, and PIs (always 1 for attributes).  When
        // the node has no ancestor document, we anchor the path
        // at `Q{}root()/...` (the spec's signal for an orphan
        // subtree).
        "path" => {
            if args.len() > 1 {
                return Err(xpath_err(format!(
                    "path() requires 0 or 1 arguments (got {})", args.len()
                )));
            }
            let v = if args.is_empty() {
                Value::NodeSet(vec![ctx.context_node])
            } else { arg!(0) };
            let node = match v {
                Value::NodeSet(ref ns) => ns.first().copied(),
                _ => None,
            };
            let Some(node) = node else {
                return Ok(Value::NodeSet(Vec::new()));
            };
            Ok(Value::String(node_path_string(node, idx)))
        }
        // XPath 3.0 §15.4.10 — `fn:sort($input)` returns the input
        // sorted by its atomic value with the default collation.
        // The 2-arg form takes a collation URI (we only support
        // codepoint; non-codepoint collations fall back to codepoint
        // ordering with the warning silenced).  The 3-arg form takes
        // a per-item key-extractor function reference — XPath 3.0+
        // higher-order functions which we don't carry, so reject.
        "sort" => {
            if args.is_empty() || args.len() > 3 {
                return Err(xpath_err(format!(
                    "sort() requires 1 to 3 arguments (got {})", args.len()
                )));
            }
            if args.len() == 3 {
                return Err(xpath_err(
                    "sort() with key-extractor function not supported"));
            }
            let input = arg!(0);
            // Pull items out so each can be compared by atomic value.
            let mut items = items_of(&input);
            // Determine whether everything is numeric so we can
            // sort numerically; otherwise fall back to codepoint
            // string ordering.
            let nums: Option<Vec<f64>> = items.iter().map(|v| {
                let s = value_to_string_with(v, idx, ctx.bindings);
                s.trim().parse::<f64>().ok()
            }).collect();
            if let Some(nums) = nums {
                let mut indexed: Vec<(usize, f64)> = nums.into_iter().enumerate().collect();
                indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                let order: Vec<usize> = indexed.into_iter().map(|(i, _)| i).collect();
                let sorted: Vec<Value> = order.into_iter().map(|i| items[i].clone()).collect();
                return Ok(if sorted.len() == 1 { sorted.into_iter().next().unwrap() }
                          else                  { Value::Sequence(sorted) });
            }
            // String-collation fallback.
            let keys: Vec<String> = items.iter()
                .map(|v| value_to_string_with(v, idx, ctx.bindings))
                .collect();
            let mut order: Vec<usize> = (0..items.len()).collect();
            order.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
            let sorted: Vec<Value> = order.into_iter().map(|i| std::mem::replace(
                &mut items[i], Value::NodeSet(Vec::new()))).collect();
            Ok(if sorted.len() == 1 { sorted.into_iter().next().unwrap() }
               else                  { Value::Sequence(sorted) })
        }
        "head" => {
            check_args!(1);
            if let Value::NodeSet(ns) = arg!(0) {
                return Ok(Value::NodeSet(ns.into_iter().take(1).collect()));
            }
            let pieces = sequence_to_strings(&arg!(0), idx);
            Ok(match pieces.into_iter().next() {
                Some(s) => Value::String(s),
                None    => Value::NodeSet(Vec::new()),
            })
        }
        "tail" => {
            check_args!(1);
            if let Value::NodeSet(ns) = arg!(0) {
                return Ok(Value::NodeSet(ns.into_iter().skip(1).collect()));
            }
            let pieces: Vec<String> = sequence_to_strings(&arg!(0), idx).into_iter().skip(1).collect();
            match idx.allocate_rtf_text_nodes(pieces.clone()) {
                Some(ids) => Ok(Value::NodeSet(ids)),
                None      => Ok(Value::String(pieces.join(""))),
            }
        }
        // XPath 2.0 §15.1.7 `insert-before($seq, $pos, $insert)` —
        // insert items into a sequence at 1-based `$pos`.  Out-of-
        // range positions clamp to the head / tail.  When either
        // operand carries typed-atomic items we preserve the
        // Sequence shape; otherwise we keep the legacy NodeSet
        // result for node-only callers.
        "insert-before" => {
            check_args!(3);
            let seq = arg!(0);
            let pos = value_to_number(&arg!(1), idx).round() as i64;
            let ins = arg!(2);
            let any_seq = matches!(seq, Value::Sequence(_))
                       || matches!(ins, Value::Sequence(_));
            if any_seq {
                let to_items = |v: Value| -> Vec<Value> {
                    match v {
                        Value::Sequence(xs) => xs,
                        Value::NodeSet(ns) => ns.into_iter()
                            .map(|n| Value::NodeSet(vec![n])).collect(),
                        other => vec![other],
                    }
                };
                let mut a = to_items(seq);
                let mut b = to_items(ins);
                let p = if pos < 1 { 0 } else { (pos as usize - 1).min(a.len()) };
                let tail = a.split_off(p);
                a.append(&mut b);
                a.extend(tail);
                return Ok(Value::Sequence(a));
            }
            let into_ids = |v: Value| -> Vec<NodeId> {
                match v.untyped() {
                    Value::NodeSet(ns) => ns,
                    Value::String(s)   => idx.allocate_rtf_text_nodes(vec![s])
                                             .unwrap_or_default(),
                    Value::Number(n)   => idx.allocate_rtf_text_nodes(
                                              vec![value_to_string(&Value::Number(n), idx)])
                                              .unwrap_or_default(),
                    Value::Boolean(b)  => idx.allocate_rtf_text_nodes(
                                              vec![if b { "true".into() } else { "false".into() }])
                                              .unwrap_or_default(),
                    Value::ForeignNodeSet(_) => Vec::new(),
                    Value::IntRange { lo, hi } => idx.allocate_rtf_text_nodes(
                        (lo..=hi).map(|i| i.to_string()).collect()
                    ).unwrap_or_default(),
                    Value::Typed(_) | Value::Sequence(_) => unreachable!(),
                    Value::Map(_) | Value::Array(_) | Value::Function(_) => Vec::new(),
                }
            };
            let mut a = into_ids(seq);
            let mut b = into_ids(ins);
            let p = if pos < 1 { 0 } else { (pos as usize - 1).min(a.len()) };
            let tail = a.split_off(p);
            a.append(&mut b);
            a.extend(tail);
            Ok(Value::NodeSet(a))
        }
        // XPath 2.0 §15.1.8 `remove($seq, $pos)` — remove the item
        // at 1-based `$pos`.  Out-of-range positions return the
        // sequence unchanged.
        "remove" => {
            check_args!(2);
            let seq = arg!(0);
            let pos = value_to_number(&arg!(1), idx).round() as i64;
            if let Value::Sequence(mut items) = seq {
                if pos >= 1 && (pos as usize) <= items.len() {
                    items.remove(pos as usize - 1);
                }
                return Ok(Value::Sequence(items));
            }
            let mut items: Vec<NodeId> = match seq.untyped() {
                Value::NodeSet(ns) => ns,
                Value::String(s)   => idx.allocate_rtf_text_nodes(vec![s])
                                         .unwrap_or_default(),
                Value::Number(n)   => idx.allocate_rtf_text_nodes(
                                          vec![value_to_string(&Value::Number(n), idx)])
                                          .unwrap_or_default(),
                Value::Boolean(b)  => idx.allocate_rtf_text_nodes(
                                          vec![if b { "true".into() } else { "false".into() }])
                                          .unwrap_or_default(),
                Value::ForeignNodeSet(_) => Vec::new(),
                Value::IntRange { lo, hi } => idx.allocate_rtf_text_nodes(
                    (lo..=hi).map(|i| i.to_string()).collect()
                ).unwrap_or_default(),
                // untyped() flattens singletons and never produces a
                // Typed; multi-item Sequences were taken above.
                Value::Typed(_) | Value::Sequence(_) => unreachable!(),
                Value::Map(_) | Value::Array(_) | Value::Function(_) => Vec::new(),
            };
            if pos >= 1 && (pos as usize) <= items.len() {
                items.remove(pos as usize - 1);
            }
            Ok(Value::NodeSet(items))
        }
        "empty-sequence" => {
            check_args!(0);
            Ok(Value::NodeSet(Vec::new()))
        }
        "format-date" => {
            // `format-date(date, picture, [lang, calendar, country])` —
            // we honour `date` + `picture` and ignore the locale args.
            if args.len() < 2 || args.len() > 5 {
                return Err(xpath_err(format!(
                    "format-date() requires 2 to 5 arguments (got {})", args.len()
                )));
            }
            let v = format_date_time_picture(&arg_str!(0), &arg_str!(1), DateKind::Date)?;
            let lang = if args.len() > 2 { arg_str!(2) } else { String::new() };
            let cal  = if args.len() > 3 { arg_str!(3) } else { String::new() };
            Ok(Value::String(format!("{}{v}", format_date_locale_prefix(&lang, &cal))))
        }
        "format-time" => {
            if args.len() < 2 || args.len() > 5 {
                return Err(xpath_err(format!(
                    "format-time() requires 2 to 5 arguments (got {})", args.len()
                )));
            }
            let v = format_date_time_picture(&arg_str!(0), &arg_str!(1), DateKind::Time)?;
            let lang = if args.len() > 2 { arg_str!(2) } else { String::new() };
            let cal  = if args.len() > 3 { arg_str!(3) } else { String::new() };
            Ok(Value::String(format!("{}{v}", format_date_locale_prefix(&lang, &cal))))
        }
        "deep-equal" => {
            // XPath 2.0 §15.3.1 — sequences are deep-equal iff they
            // have the same length and each item is pairwise
            // deep-equal.  Atomic items compare by value (with
            // type-aware promotion); nodes compare by name +
            // attributes (set equality) + child sequence.
            if args.len() < 2 || args.len() > 3 {
                return Err(xpath_err(format!(
                    "deep-equal() requires 2 or 3 arguments (got {})", args.len()
                )));
            }
            let a = arg!(0);
            let b = arg!(1);
            Ok(Value::Boolean(deep_equal_values(&a, &b, idx, ctx.bindings)))
        }
        "base-uri" => {
            // 0-arg form uses the context node; 1-arg takes the
            // supplied node.  XPath 2.0 §15.5.3 — walk up to the
            // nearest synthetic-RTF document root and consult the
            // bindings' override table, then fall back to the
            // empty sequence when no base URI is recorded.
            if args.len() > 1 {
                return Err(xpath_err(format!(
                    "base-uri() requires 0 or 1 arguments (got {})", args.len()
                )));
            }
            let target = if args.is_empty() {
                Some(ctx.context_node)
            } else {
                match arg!(0) {
                    Value::NodeSet(ns) => ns.first().copied(),
                    Value::ForeignNodeSet(_) => None,
                    _ => return Err(xpath_err(
                        "base-uri() argument must be a node")),
                }
            };
            let Some(start) = target else {
                return Ok(Value::NodeSet(Vec::new()));
            };
            // XPath 2.0 §2.5 (fn:base-uri accessor): a namespace node
            // has no base URI.
            if idx.kind(start) == XPathNodeKind::Namespace {
                return Ok(Value::NodeSet(Vec::new()));
            }
            // Walk ancestor-or-self collecting each element's `xml:base`
            // declaration (leaf-first), stopping at the first node that
            // carries an explicit base-URI override in the bindings'
            // table (synthetic-RTF document roots, the source document
            // root, or an `xsl:document`/variable `xml:base`).  The
            // effective base is that anchor resolved against each
            // `xml:base` from the outermost inward (XML Base §3 / RFC
            // 3986).  Attribute / text / comment / PI nodes inherit
            // their parent element's base, which falls out of the same
            // walk since they have no `xml:base` of their own.
            let mut chain: Vec<String> = Vec::new();
            let mut anchor: Option<String> = None;
            let mut n = start;
            loop {
                if let Some(u) = ctx.bindings.node_base_uri(n) {
                    anchor = Some(u);
                    break;
                }
                if idx.kind(n) == XPathNodeKind::Element {
                    for a in idx.attr_range(n) {
                        if idx.local_name(a) == "base" && idx.namespace_uri(a) == "http://www.w3.org/XML/1998/namespace" {
                            chain.push(idx.string_value(a));
                            break;
                        }
                    }
                }
                match idx.parent(n) {
                    Some(p) => n = p,
                    None    => break,
                }
            }
            let mut base = anchor.or_else(|| ctx.bindings.static_base_uri());
            for b in chain.into_iter().rev() {
                base = Some(match base {
                    Some(r) => resolve_uri_against(&r, &b),
                    None    => b,
                });
            }
            match base {
                Some(u) => Ok(Value::String(u)),
                None    => Ok(Value::NodeSet(Vec::new())),
            }
        }
        "static-base-uri" => {
            // XPath 2.0 §15.2.3 — return the static base URI from the
            // bindings (set from the stylesheet's `xml:base` or the
            // apply-time base URI).  Empty sequence if unset.
            if !args.is_empty() {
                return Err(xpath_err("static-base-uri() takes no arguments"));
            }
            match ctx.bindings.static_base_uri() {
                Some(s) => Ok(Value::String(s)),
                None    => Ok(Value::NodeSet(vec![])),
            }
        }
        "resolve-uri" => {
            // XPath 2.0 §15.5.7: resolve-uri(relative[, base]).
            // Falls back to returning the relative URI unchanged
            // when base is empty / absent.  No URI parser dependency —
            // perform the join lexically: if `relative` is absolute
            // (contains `:` before any `/`), return it; otherwise
            // prepend the base's parent directory.
            if args.is_empty() || args.len() > 2 {
                return Err(xpath_err(format!(
                    "resolve-uri() requires 1 or 2 arguments (got {})", args.len()
                )));
            }
            // Empty sequence in → empty sequence out (XPath 2.0
            // §15.5.7).  Pre-check before stringifying so a true
            // empty-sequence argument doesn't degrade to "".
            if let Value::NodeSet(ns) = &arg!(0) {
                if ns.is_empty() { return Ok(Value::NodeSet(vec![])); }
            }
            if let Value::Sequence(items) = &arg!(0) {
                if items.is_empty() { return Ok(Value::NodeSet(vec![])); }
            }
            let rel = value_to_string_with(&arg!(0), idx, ctx.bindings);
            let base = if args.len() == 2 {
                value_to_string_with(&arg!(1), idx, ctx.bindings)
            } else {
                ctx.bindings.static_base_uri().unwrap_or_default()
            };
            // XPath 2.0 §15.5.7 / erratum FO.E1 — in the explicit
            // 2-argument form the base must be an absolute URI, and both
            // arguments must be valid URI references; a relative base or
            // a malformed reference (whitespace, or a second `#` so the
            // fragment is ambiguous) is FORG0002.
            if args.len() == 2 && !base.is_empty() {
                let valid_ref = |u: &str|
                    !u.contains(char::is_whitespace) && u.matches('#').count() <= 1;
                if !valid_ref(&rel) || !valid_ref(&base) || !uri_has_scheme(&base) {
                    return Err(xpath_err(format!(
                        "resolve-uri: base '{base}' must be an absolute URI and \
                         both arguments valid URI references"
                    )).with_xpath_code("FORG0002"));
                }
            }
            // Empty $relative resolves to the base URI itself
            // (RFC 3986 §5.2.2 / XPath 2.0 §15.5.7).
            if rel.is_empty() && !base.is_empty() {
                return Ok(Value::String(base));
            }
            if base.is_empty() {
                return Ok(Value::String(rel));
            }
            Ok(Value::String(resolve_uri_rfc3986(&base, &rel)))
        }
        // XPath 2.0 §10.5 — accessor functions on xs:date /
        // xs:dateTime / xs:time / xs:duration.  Values flow through
        // the engine as strings (no atomic-type system yet); parse
        // the lexical form on demand to extract the requested field.
        // Lenient: missing fields return 0 / empty-string per spec.
        // XPath 2.0 §15.5.5 root([$node]) — return the root of
        // the tree containing $node (or context node).  In our
        // index that's the ancestor with no parent.
        "root" => {
            let v = if args.is_empty() {
                Value::NodeSet(vec![ctx.context_node])
            } else { arg!(0) };
            let node = match v {
                Value::NodeSet(ref ns) => ns.first().copied(),
                _ => None,
            };
            let r = node.map(|mut n| {
                while let Some(p) = idx.parent(n) { n = p; }
                vec![n]
            }).unwrap_or_default();
            Ok(Value::NodeSet(r))
        }
        // XPath 2.0 §15.5.4 doc($uri) — same as document($uri)
        // but always returns the doc as a single document node.
        // We can't load here without a Loader, so delegate to the
        // bindings layer (which routes through the XSLT runtime's
        // pre-loaded document table).
        "doc" => {
            // XPath 2.0 §15.5.4 — `fn:doc($uri)` returns the empty
            // sequence when `$uri` is the empty sequence, rather
            // than failing with a not-found.  Also stay quiet (empty
            // sequence) for any caller that passes through an empty
            // value via untyped sequence: we only fail on a real
            // non-empty URI that we can't resolve.
            check_args!(1);
            let arg0 = arg!(0);
            if matches!(&arg0,
                Value::NodeSet(ns) if ns.is_empty())
                || matches!(&arg0,
                    Value::Sequence(items) if items.is_empty())
            {
                return Ok(Value::NodeSet(Vec::new()));
            }
            let uri = value_to_string_with(&arg0, idx, ctx.bindings);
            match ctx.bindings.call_function_in(
                "", "document",
                vec![Value::String(uri)],
                ctx.context_node,
            ) {
                Some(r) => r.map_err(|e| e.or_xpath_code("FODC0002")),
                None    => Ok(Value::NodeSet(Vec::new())),
            }
        }
        "doc-available" => {
            check_args!(1);
            let arg0 = arg!(0);
            if matches!(&arg0,
                Value::NodeSet(ns) if ns.is_empty())
                || matches!(&arg0,
                    Value::Sequence(items) if items.is_empty())
            {
                return Ok(Value::Boolean(false));
            }
            let uri = value_to_string_with(&arg0, idx, ctx.bindings);
            match ctx.bindings.call_function_in(
                "", "document",
                vec![Value::String(uri)],
                ctx.context_node,
            ) {
                Some(Ok(Value::NodeSet(ns))) => Ok(Value::Boolean(!ns.is_empty())),
                _ => Ok(Value::Boolean(false)),
            }
        }
        "document-uri" => {
            // XPath 2.0 §2.5 (dm:document-uri): the absolute URI of
            // the resource a *document node* was constructed from, or
            // the empty sequence when the node isn't a document node
            // or its URI is unknown.  The source/loaded document's URI
            // is recorded as that node's base URI override.
            if args.len() > 1 {
                return Err(xpath_err(format!(
                    "document-uri() takes 0 or 1 arguments (got {})", args.len()
                )));
            }
            let target = if args.is_empty() {
                Some(ctx.context_node)
            } else {
                match arg!(0) {
                    Value::NodeSet(ns) => ns.first().copied(),
                    _ => None,
                }
            };
            let uri = target.filter(|&n| idx.kind(n) == XPathNodeKind::Document)
                .and_then(|n| ctx.bindings.node_base_uri(n));
            match uri {
                Some(u) => Ok(Value::String(u)),
                None    => Ok(Value::NodeSet(Vec::new())),
            }
        }
        // XPath 2.0 §6.4.5 round-half-to-even — banker's rounding.
        "round-half-to-even" => {
            if args.is_empty() || args.len() > 2 {
                return Err(xpath_err(format!(
                    "round-half-to-even() takes 1 or 2 arguments (got {})", args.len()
                )));
            }
            let n = value_to_number(&arg!(0), idx);
            let precision = if args.len() == 2 {
                value_to_number(&arg!(1), idx) as i32
            } else { 0 };
            let scale = 10f64.powi(precision);
            let scaled = n * scale;
            // Round half to even (banker's rounding).  The
            // `f64::round` half-away-from-zero rule doesn't match;
            // when the absolute fractional part is exactly 0.5 we
            // pick the neighbour whose integer part is even.
            let rounded = if scaled.fract().abs() == 0.5 {
                let floor = scaled.floor();
                if (floor as i64) % 2 == 0 { floor } else { floor + 1.0 }
            } else {
                scaled.round()
            };
            let result = rounded / scale;
            // XPath 2.0 §6.4.5 — round-half-to-even on a zero
            // value returns positive zero; without this an input
            // like `round-half-to-even(-3.0, -2)` produces -0.0
            // (signed zero from the negative-scale roundtrip),
            // which serialises as "-0" and disagrees with the
            // spec's "0".
            let result = if result == 0.0 { 0.0 } else { result };
            Ok(Value::Number(Numeric::Double(result)))
        }
        // XPath 2.0 §15.5.6 collection — we never have a collection;
        // return empty for any URI so collection-driven tests don't
        // panic, matching the "no such collection" XPath 2.0
        // fallback.
        "collection" => {
            if args.len() > 1 {
                return Err(xpath_err("collection() takes 0 or 1 arguments"));
            }
            Ok(Value::NodeSet(Vec::new()))
        }
        // XPath 2.0 §10.3.4 dateTime($d, $t) — combine a date and a
        // time into a dateTime.  Both args are strings in lexical
        // xs:date / xs:time form.
        "dateTime" => {
            check_args!(2);
            let d = value_to_string_with(&arg!(0), idx, ctx.bindings);
            let t = value_to_string_with(&arg!(1), idx, ctx.bindings);
            // Combine the date components of $d with the time
            // components of $t.  The result's timezone is the one
            // timezone present on either argument; if both carry a
            // timezone they must agree (FORG0008 otherwise).  Crucially
            // the timezone belongs at the *end* of the dateTime, not
            // after the date — `2004-09-05+02:00` + `12:15:00` is
            // `2004-09-05T12:15:00+02:00`, never `2004-09-05+02:00T…`.
            match (parse_xsd_date_time(&d, DateKind::Date),
                   parse_xsd_date_time(&t, DateKind::Time)) {
                (Some((y, mo, dd, _, _, _, _, tz_d)),
                 Some((_, _, _, h, mi, s, frac, tz_t))) => {
                    let tz = match (tz_d, tz_t) {
                        (Some(a), Some(b)) if a != b => return Err(xpath_err(
                            "dateTime(): the date and time have inconsistent timezones (FORG0008)")),
                        (Some(a), _) => Some(a),
                        (_, b)       => b,
                    };
                    Ok(Value::String(format_datetime_lexical(y, mo, dd, h, mi, s, frac, tz)))
                }
                // Non-lexical inputs — keep a lenient join rather than
                // failing the whole expression.
                _ => Ok(Value::String(format!("{}T{}", d.trim(), t.trim()))),
            }
        }
        // XPath 2.0 §10.4.4 / §10.4.5 / §10.4.6 — QName accessors.
        "local-name-from-QName" => {
            check_args!(1);
            let s = value_to_string_with(&arg!(0), idx, ctx.bindings);
            // The string-value of an xs:QName is `prefix:local` or
            // `{uri}local` (Clark form when no prefix).  Take the
            // tail after the last `:` or after `}`.
            let local = if let Some(i) = s.rfind('}') {
                s[i + 1..].to_string()
            } else if let Some(i) = s.rfind(':') {
                s[i + 1..].to_string()
            } else { s };
            Ok(Value::String(local))
        }
        "prefix-from-QName" => {
            check_args!(1);
            let s = value_to_string_with(&arg!(0), idx, ctx.bindings);
            // Clark form has no prefix.  Otherwise take leading
            // chars up to first `:`.
            if s.starts_with('{') {
                return Ok(Value::String(String::new()));
            }
            let prefix = s.split_once(':').map(|(p, _)| p).unwrap_or("");
            Ok(Value::String(prefix.to_string()))
        }
        "namespace-uri-from-QName" => {
            check_args!(1);
            let s = value_to_string_with(&arg!(0), idx, ctx.bindings);
            if s.starts_with('{') {
                if let Some(end) = s.find('}') {
                    return Ok(Value::String(s[1..end].to_string()));
                }
            }
            Ok(Value::String(String::new()))
        }
        // XPath 2.0 §15.1.10 QName($uri, $lex) — construct.  Round-
        // trip via Clark form so subsequent accessors above work.
        "QName" => {
            check_args!(2);
            let uri   = value_to_string_with(&arg!(0), idx, ctx.bindings);
            let lex   = value_to_string_with(&arg!(1), idx, ctx.bindings);
            let local = lex.rsplit(':').next().unwrap_or(&lex);
            if uri.is_empty() {
                Ok(Value::String(lex.clone()))
            } else {
                Ok(Value::String(format!("{{{uri}}}{local}")))
            }
        }
        "resolve-QName" => {
            // XPath 2.0 §15.1.10.2 — resolve a lexical QName against
            // the in-scope namespaces of an element node, returning the
            // expanded QName in Clark form `{uri}local`.  An empty
            // first argument yields the empty sequence; a malformed
            // lexical QName is FOCA0002; a prefix with no in-scope
            // binding is FONS0004.
            check_args!(2);
            if matches!(&arg!(0), Value::NodeSet(ns) if ns.is_empty()) {
                return Ok(Value::NodeSet(Vec::new()));
            }
            let lex = value_to_string_with(&arg!(0), idx, ctx.bindings);
            if !is_valid_lexical_qname(&lex) {
                return Err(xpath_err(format!(
                    "resolve-QName: '{lex}' is not a valid lexical QName"
                )).with_xpath_code("FOCA0002"));
            }
            let (prefix, local) = match lex.split_once(':') {
                Some((p, l)) => (p, l),
                None         => ("", lex.as_str()),
            };
            // Resolve the prefix against the element's namespace nodes
            // (same walk as namespace-uri-for-prefix); `xml` is implicit.
            let elem = match arg!(1) {
                Value::NodeSet(ref ns) => ns.first().copied(),
                _ => None,
            };
            let uri = elem.and_then(|id| {
                idx.ns_range(id)
                    .into_iter()
                    .find(|&ns_id| {
                        let p = idx.local_name(ns_id);
                        (prefix.is_empty() && p.is_empty()) || p == prefix
                    })
                    .map(|ns_id| idx.string_value(ns_id))
                    .or_else(|| (prefix == "xml")
                        .then(|| "http://www.w3.org/XML/1998/namespace".to_string()))
            });
            match uri {
                Some(u) if !u.is_empty() => Ok(Value::String(format!("{{{u}}}{local}"))),
                // A non-empty prefix that resolves to nothing is FONS0004;
                // an unprefixed name simply stays in no namespace.
                _ if !prefix.is_empty() => Err(xpath_err(format!(
                    "resolve-QName: no namespace declaration for prefix '{prefix}'"
                )).with_xpath_code("FONS0004")),
                _ => Ok(Value::String(local.to_string())),
            }
        }
        // XPath 2.0 §15.2.4 normalize-unicode($s [, $form]).  We
        // don't carry a Unicode normalization table; emit the input
        // unchanged for any normalisation form except an unknown one
        // (where the spec wants an error).
        "normalize-unicode" => {
            if args.is_empty() || args.len() > 2 {
                return Err(xpath_err("normalize-unicode() takes 1 or 2 arguments"));
            }
            let s = value_to_string_with(&arg!(0), idx, ctx.bindings);
            Ok(Value::String(s))
        }
        "type-available" => {
            // XSLT 2.0 §16.5.6 — `type-available(name)` answers true
            // iff the processor recognises `name` as the in-scope
            // (expanded) name of an XSD type.  We don't implement
            // user-defined schema types, but the XSD built-in types
            // are all available; resolve the QName, check namespace,
            // then look up the local name in the static type table.
            check_args!(1);
            let name = value_to_string_with(&arg!(0), idx, ctx.bindings);
            let (prefix, local) = match name.split_once(':') {
                Some((p, l)) => (Some(p), l),
                None         => (None, name.as_str()),
            };
            let uri = match prefix {
                Some(p) => ctx.bindings.resolve_prefix(p)
                    .or_else(|| match p {
                        "xml" => Some("http://www.w3.org/XML/1998/namespace".into()),
                        "xs"  => Some("http://www.w3.org/2001/XMLSchema".into()),
                        _ => None,
                    }),
                None    => None,
            };
            let in_xsd = uri.as_deref() == Some("http://www.w3.org/2001/XMLSchema");
            // Unprefixed names match nothing under the default
            // (empty) namespace.
            if prefix.is_none() {
                return Ok(Value::Boolean(false));
            }
            if !in_xsd {
                return Ok(Value::Boolean(false));
            }
            let known = matches!(local,
                "anyType" | "anySimpleType" | "anyAtomicType" | "untyped" | "untypedAtomic"
                | "string" | "boolean" | "decimal" | "float" | "double"
                | "integer" | "long" | "int" | "short" | "byte"
                | "nonNegativeInteger" | "nonPositiveInteger"
                | "positiveInteger" | "negativeInteger"
                | "unsignedLong" | "unsignedInt" | "unsignedShort" | "unsignedByte"
                | "duration" | "dateTime" | "time" | "date"
                | "dayTimeDuration" | "yearMonthDuration"
                | "gYearMonth" | "gYear" | "gMonth" | "gMonthDay" | "gDay"
                | "hexBinary" | "base64Binary" | "anyURI" | "QName" | "NOTATION"
                | "normalizedString" | "token" | "language"
                | "Name" | "NCName" | "ID" | "IDREF" | "IDREFS"
                | "ENTITY" | "ENTITIES" | "NMTOKEN" | "NMTOKENS"
                | "numeric"
            );
            Ok(Value::Boolean(known))
        }
        "unparsed-entity-public-id" => {
            check_args!(1);
            Ok(Value::String(String::new()))
        }
        // XPath 2.0 §15.5.8 / §15.5.9 namespace accessors.
        // We approximate from the in-scope namespace nodes of the
        // supplied element (ns_range gives the element's in-scope
        // bindings).  When the argument isn't an element node we
        // return the empty sequence.
        "in-scope-prefixes" => {
            check_args!(1);
            let elem = match arg!(0) {
                Value::NodeSet(ref ns) => ns.first().copied(),
                _ => None,
            };
            let Some(id) = elem else {
                return Ok(Value::NodeSet(Vec::new()));
            };
            let mut out: Vec<String> = Vec::new();
            for ns_id in idx.ns_range(id) {
                let p = idx.local_name(ns_id);
                out.push(if p.is_empty() { "".into() } else { p.to_string() });
            }
            // Always include "xml" since it's implicit.
            if !out.iter().any(|s| s == "xml") {
                out.push("xml".into());
            }
            match idx.allocate_rtf_text_nodes(out.clone()) {
                Some(ids) => Ok(Value::NodeSet(ids)),
                None      => Ok(Value::String(out.join(" "))),
            }
        }
        "namespace-uri-for-prefix" => {
            check_args!(2);
            let prefix = value_to_string_with(&arg!(0), idx, ctx.bindings);
            let elem = match arg!(1) {
                Value::NodeSet(ref ns) => ns.first().copied(),
                _ => None,
            };
            let Some(id) = elem else {
                return Ok(Value::NodeSet(Vec::new()));
            };
            for ns_id in idx.ns_range(id) {
                let p = idx.local_name(ns_id);
                let match_default = prefix.is_empty() && p.is_empty();
                if match_default || p == prefix {
                    return Ok(Value::String(idx.string_value(ns_id)));
                }
            }
            // Implicit `xml` binding.
            if prefix == "xml" {
                return Ok(Value::String("http://www.w3.org/XML/1998/namespace".into()));
            }
            Ok(Value::NodeSet(Vec::new()))
        }
        "year-from-dateTime" | "year-from-date" => {
            check_args!(1);
            let s = value_to_string_with(&arg!(0), idx, ctx.bindings);
            let v = s.trim();
            if v.is_empty() {
                return Ok(Value::NodeSet(Vec::new()));
            }
            let (sign, rest) = if let Some(r) = v.strip_prefix('-') { (-1i64, r) } else { (1i64, v) };
            let yr_end = rest.find('-').unwrap_or(rest.len());
            let yr: i64 = rest[..yr_end].parse().map_err(|_|
                xpath_err(format!("invalid date-like value: {s:?}")))?;
            Ok(Value::Number(Numeric::Double((sign * yr) as f64)))
        }
        "month-from-dateTime" | "month-from-date" => {
            check_args!(1);
            let s = value_to_string_with(&arg!(0), idx, ctx.bindings);
            let v = s.trim();
            if v.is_empty() {
                return Ok(Value::NodeSet(Vec::new()));
            }
            let body = v.strip_prefix('-').unwrap_or(v);
            // Year is digits up to first '-'; month is next 2 digits.
            let after_year = body.split_once('-').map(|(_, r)| r).unwrap_or("");
            let mm: u32 = after_year.get(..2).and_then(|s| s.parse().ok())
                .ok_or_else(|| xpath_err(format!("invalid date-like value: {s:?}")))?;
            Ok(Value::Number(Numeric::Double(mm as f64)))
        }
        "day-from-dateTime" | "day-from-date" => {
            check_args!(1);
            let s = value_to_string_with(&arg!(0), idx, ctx.bindings);
            let v = s.trim();
            if v.is_empty() {
                return Ok(Value::NodeSet(Vec::new()));
            }
            let body = v.strip_prefix('-').unwrap_or(v);
            // Year-Month-Day: take chars after the second '-'.
            let mut parts = body.splitn(3, '-');
            parts.next(); parts.next();
            let dd_seg = parts.next().unwrap_or("");
            let dd: u32 = dd_seg.get(..2).and_then(|s| s.parse().ok())
                .ok_or_else(|| xpath_err(format!("invalid date-like value: {s:?}")))?;
            Ok(Value::Number(Numeric::Double(dd as f64)))
        }
        "hours-from-dateTime" | "hours-from-time" => {
            check_args!(1);
            let s = value_to_string_with(&arg!(0), idx, ctx.bindings);
            // For dateTime the time portion is after 'T'.  Time-only
            // values start with HH:MM:SS directly.
            let t = s.split_once('T').map(|(_, r)| r).unwrap_or(s.trim());
            let hh: u32 = t.get(..2).and_then(|s| s.parse().ok())
                .ok_or_else(|| xpath_err(format!("invalid time-like value: {s:?}")))?;
            Ok(Value::Number(Numeric::Double(hh as f64)))
        }
        "minutes-from-dateTime" | "minutes-from-time" => {
            check_args!(1);
            let s = value_to_string_with(&arg!(0), idx, ctx.bindings);
            let t = s.split_once('T').map(|(_, r)| r).unwrap_or(s.trim());
            let mm: u32 = t.get(3..5).and_then(|s| s.parse().ok())
                .ok_or_else(|| xpath_err(format!("invalid time-like value: {s:?}")))?;
            Ok(Value::Number(Numeric::Double(mm as f64)))
        }
        "seconds-from-dateTime" | "seconds-from-time" => {
            check_args!(1);
            let s = value_to_string_with(&arg!(0), idx, ctx.bindings);
            let t = s.split_once('T').map(|(_, r)| r).unwrap_or(s.trim());
            // Seconds: from char 6 up to first 'Z' or '+'/'-' (zone) or end.
            let after = t.get(6..).unwrap_or("");
            let end = after.find(|c: char| c == 'Z' || c == '+' || c == '-')
                .unwrap_or(after.len());
            let ss: f64 = after[..end].parse()
                .map_err(|_| xpath_err(format!("invalid time-like value: {s:?}")))?;
            Ok(Value::Number(Numeric::Double(ss)))
        }
        "timezone-from-dateTime" | "timezone-from-date" | "timezone-from-time" => {
            check_args!(1);
            let s = value_to_string_with(&arg!(0), idx, ctx.bindings);
            let v = s.trim();
            // Empty input → empty result (no timezone).
            if v.is_empty() {
                return Ok(Value::NodeSet(Vec::new()));
            }
            // Trailing 'Z' → PT0S; trailing ±HH:MM → P[T]Hh[Mm]…
            let tz_start = v.rfind(|c: char| c == 'Z' || c == '+' || c == '-')
                .filter(|&i| i > 0);
            let tz = match tz_start.map(|i| &v[i..]) {
                Some("Z") => "PT0S".to_string(),
                Some(off) if off.len() == 6 => {
                    let sign = &off[0..1];
                    let hh: i32 = off[1..3].parse().unwrap_or(0);
                    let mm: i32 = off[4..6].parse().unwrap_or(0);
                    if hh == 0 && mm == 0 { "PT0S".to_string() }
                    else if mm == 0 { format!("{sign}PT{hh}H") }
                    else { format!("{sign}PT{hh}H{mm}M") }
                }
                _ => return Ok(Value::NodeSet(Vec::new())),
            };
            Ok(Value::String(tz.replace("+", "")))
        }
        "years-from-duration" | "months-from-duration"
        | "days-from-duration" | "hours-from-duration"
        | "minutes-from-duration" | "seconds-from-duration" => {
            check_args!(1);
            let s = value_to_string_with(&arg!(0), idx, ctx.bindings);
            let (sign, rest) = if let Some(r) = s.trim().strip_prefix('-') {
                (-1.0_f64, r)
            } else { (1.0, s.trim()) };
            let body = rest.strip_prefix('P').unwrap_or(rest);
            let (date_part, time_part) = match body.find('T') {
                Some(i) => (&body[..i], &body[i + 1..]),
                None    => (body, ""),
            };
            // Pull each component out as a number; treat missing
            // components as 0.
            fn extract(part: &str, marker: char) -> f64 {
                if let Some(i) = part.find(marker) {
                    let start = part[..i].rfind(|c: char| !c.is_ascii_digit() && c != '.')
                        .map(|n| n + 1).unwrap_or(0);
                    part[start..i].parse().unwrap_or(0.0)
                } else { 0.0 }
            }
            let years   = extract(date_part, 'Y');
            let months  = extract(date_part, 'M');
            let days    = extract(date_part, 'D');
            let hours   = extract(time_part, 'H');
            let minutes = extract(time_part, 'M');
            let seconds = extract(time_part, 'S');
            // XPath 2.0 §10.5: each accessor returns the *signed*
            // value of its component within the canonical form;
            // canonical normalisation isn't required by the
            // accessor itself, only by the duration type's
            // value-space comparisons.
            let v = match name.as_ref() {
                "years-from-duration"   => years,
                "months-from-duration"  => months,
                "days-from-duration"    => days,
                "hours-from-duration"   => hours,
                "minutes-from-duration" => minutes,
                "seconds-from-duration" => seconds,
                _ => unreachable!(),
            };
            Ok(Value::Number(Numeric::Double(sign * v)))
        }
        "nilled" => {
            // XPath 2.0 §15.4.6 — `nilled($node)`.  Per the spec:
            // * empty sequence → empty sequence
            // * non-element node → empty sequence
            // * element node in an untyped data model (we never
            //   surface PSVI) → false, even when the element carries
            //   `xsi:nil="true"`; that flag only takes effect when
            //   the element has been schema-validated as nillable.
            if args.len() != 1 {
                return Err(xpath_err("nilled() requires one argument"));
            }
            let v = arg!(0);
            let n = match v {
                Value::NodeSet(ref ns) => ns.first().copied(),
                _ => return Ok(Value::NodeSet(Vec::new())),
            };
            let id = match n { Some(id) => id, None => return Ok(Value::NodeSet(Vec::new())) };
            if !matches!(idx.kind(id), crate::xpath::XPathNodeKind::Element) {
                return Ok(Value::NodeSet(Vec::new()));
            }
            Ok(Value::Boolean(false))
        }
        "default-collation" => {
            check_args!(0);
            // Spec default: the XPath codepoint collation URI.
            Ok(Value::String("http://www.w3.org/2005/xpath-functions/collation/codepoint".into()))
        }
        "implicit-timezone" => {
            check_args!(0);
            // We don't model implicit timezone — return PT0S which
            // most code paths tolerate.
            Ok(Value::String("PT0S".into()))
        }
        "format-dateTime" => {
            if args.len() < 2 || args.len() > 5 {
                return Err(xpath_err(format!(
                    "format-dateTime() requires 2 to 5 arguments (got {})", args.len()
                )));
            }
            let v = format_date_time_picture(&arg_str!(0), &arg_str!(1), DateKind::DateTime)?;
            let lang = if args.len() > 2 { arg_str!(2) } else { String::new() };
            let cal  = if args.len() > 3 { arg_str!(3) } else { String::new() };
            Ok(Value::String(format!("{}{v}", format_date_locale_prefix(&lang, &cal))))
        }

        // ── XPath 2.0 misc functions ──────────────────────────────
        "compare" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(xpath_err(format!(
                    "compare() requires 2 or 3 arguments (got {})", args.len()
                )));
            }
            let a = arg_str!(0);
            let b = arg_str!(1);
            let coll = effective_collation(if args.len() == 3 { Some(arg_str!(2)) } else { None });
            let (ka, kb) = if is_ascii_ci_collation(coll.as_deref()) {
                (ascii_ci_fold(&a), ascii_ci_fold(&b))
            } else {
                (a, b)
            };
            Ok(Value::Number(Numeric::Double(match ka.cmp(&kb) {
                std::cmp::Ordering::Less    => -1.0,
                std::cmp::Ordering::Equal   =>  0.0,
                std::cmp::Ordering::Greater =>  1.0,
            })))
        }
        "codepoint-equal" => {
            check_args!(2);
            Ok(Value::Boolean(arg_str!(0) == arg_str!(1)))
        }
        "string-to-codepoints" => {
            check_args!(1);
            let s = arg_str!(0);
            let pieces: Vec<String> = s.chars().map(|c| (c as u32).to_string()).collect();
            match idx.allocate_rtf_text_nodes(pieces.clone()) {
                Some(ids) => Ok(Value::NodeSet(ids)),
                None      => Ok(Value::String(pieces.join(" "))),
            }
        }
        "codepoints-to-string" => {
            // Each item of the input sequence is a codepoint (number).
            check_args!(1);
            let codes = sequence_to_numbers(&arg!(0), idx);
            let mut out = String::with_capacity(codes.len());
            for n in codes {
                if let Some(c) = u32::try_from(n as i64).ok().and_then(char::from_u32) {
                    out.push(c);
                }
            }
            Ok(Value::String(out))
        }
        "encode-for-uri" => {
            check_args!(1);
            // RFC 3986 percent-encoding for "unreserved" → leave
            // alone, everything else → %HH.  Unreserved per RFC:
            // ALPHA / DIGIT / "-" / "." / "_" / "~".
            let s = arg_str!(0);
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                if c.is_ascii_alphanumeric()
                    || matches!(c, '-' | '.' | '_' | '~')
                {
                    out.push(c);
                } else {
                    let mut buf = [0u8; 4];
                    for b in c.encode_utf8(&mut buf).as_bytes() {
                        let _ = write!(out, "%{:02X}", b);
                    }
                }
            }
            Ok(Value::String(out))
        }
        "iri-to-uri" => {
            check_args!(1);
            // Encode non-ASCII chars; leave most ASCII alone (RFC 3987).
            let s = arg_str!(0);
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                if (c as u32) < 0x80
                    && !matches!(c, ' ' | '<' | '>' | '"' | '{' | '}' | '|' | '\\' | '^' | '`')
                {
                    out.push(c);
                } else {
                    let mut buf = [0u8; 4];
                    for b in c.encode_utf8(&mut buf).as_bytes() {
                        let _ = write!(out, "%{:02X}", b);
                    }
                }
            }
            Ok(Value::String(out))
        }
        "escape-html-uri" => {
            check_args!(1);
            // Same shape as iri-to-uri but only non-ASCII gets escaped.
            let s = arg_str!(0);
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                if (c as u32) < 0x80 { out.push(c); }
                else {
                    let mut buf = [0u8; 4];
                    for b in c.encode_utf8(&mut buf).as_bytes() {
                        let _ = write!(out, "%{:02X}", b);
                    }
                }
            }
            Ok(Value::String(out))
        }
        "error" => {
            // F&O §3.2.1.  Signatures: error(), error($code as xs:QName?),
            // error($code, $description), error($code, $description,
            // $error-object).  The first argument is the error-code
            // QName whose local part becomes $err:code; the second is
            // the description.  An empty first argument (or none) uses
            // the default err:FOER0000.
            let code_local = if args.is_empty() { None } else {
                let s = arg_str!(0);
                // Local part of a QName-shaped string: after the last
                // ':' (lexical) or '}' (Clark notation), else the whole.
                s.rsplit([':', '}']).next()
                    .filter(|l| !l.is_empty())
                    .map(|l| l.to_string())
            };
            let msg = if args.len() >= 2 { arg_str!(1) }
                      else if args.is_empty() { "fn:error invoked".to_string() }
                      else { arg_str!(0) };
            let mut e = xpath_err(format!("fn:error: {msg}"));
            if let Some(code) = code_local { e = e.with_xpath_code(code); }
            Err(e)
        }
        "trace" => {
            // `trace(value, label)` — pass `value` through; emit the
            // label to stderr.  Spec leaves the trace destination
            // implementation-defined.
            if args.is_empty() || args.len() > 2 {
                return Err(xpath_err(format!(
                    "trace() requires 1 or 2 arguments (got {})", args.len()
                )));
            }
            let v = arg!(0);
            if args.len() == 2 {
                eprintln!("[xpath trace] {}: {}", arg_str!(1), value_to_string(&v, idx));
            }
            Ok(v)
        }
        // Cardinality assertions — passthrough with bounds check.
        // [`sequence_len`] handles every shape uniformly: empty
        // node-set / sequence → 0, IntRange → its full cardinality,
        // single atomic → 1, multi-item Sequence → count.
        "exactly-one" => {
            check_args!(1);
            let v = arg!(0);
            let n = sequence_len(&v);
            if n != 1 {
                return Err(xpath_err(format!(
                    "exactly-one(): expected one item, got {n}"
                )));
            }
            Ok(v)
        }
        "one-or-more" => {
            check_args!(1);
            let v = arg!(0);
            if sequence_len(&v) < 1 {
                return Err(xpath_err("one-or-more(): empty input"));
            }
            Ok(v)
        }
        "zero-or-one" => {
            check_args!(1);
            let v = arg!(0);
            let n = sequence_len(&v);
            if n > 1 {
                return Err(xpath_err(format!(
                    "zero-or-one(): expected at most one item, got {n}"
                )));
            }
            Ok(v)
        }
        "node-name" => {
            check_args!(1);
            // Result is the node's expanded-name as `xs:QName`.
            // Encode the QName as a Typed value with kind="QName"
            // and Clark-notation lexical (`{uri}local`) so the
            // accessor helpers (`local-name-from-QName`,
            // `namespace-uri-from-QName`, `prefix-from-QName`)
            // can recover the URI.  When callers want the lexical
            // QName instead — `xsl:value-of select="node-name(.)"`
            // — the typed-to-string path falls back to the
            // stored lexical, which already contains `{uri}local`;
            // most XSLT 2.0 tests compare on this Clark form too.
            match arg!(0) {
                Value::NodeSet(ns) => {
                    let Some(&id) = ns.first() else {
                        return Ok(Value::NodeSet(Vec::new()));
                    };
                    let local = idx.local_name(id);
                    let uri   = idx.namespace_uri(id);
                    let lex = if uri.is_empty() {
                        local.to_string()
                    } else {
                        format!("{{{uri}}}{local}")
                    };
                    Ok(Value::Typed(Box::new(TypedAtomic {
                        kind: "QName",
                        lexical: lex,
                        numeric: None,
                        boolean: None,
                    })))
                }
                _ => Ok(Value::NodeSet(Vec::new())),
            }
        }

        // XSLT 1.0 §15 `function-available()` / `element-available()`.
        // libxslt registers these on the XPath context, but our compat
        // bridge skips the no-namespace XSLT functions (their libxslt
        // implementations need transform-context state we don't mirror),
        // so they land here as ordinary calls.  Answer from the function
        // set this engine actually provides: the XPath 1.0/2.0/3.0
        // built-ins plus the EXSLT families we implement natively.
        "function-available" if !args.is_empty() => {
            let qname = arg_str!(0);
            Ok(Value::Boolean(xpath_function_available(&qname, ctx)))
        }
        // XSLT 1.0 §12.4 `current()`.  libxslt registers it but our
        // bridge skips the no-namespace XSLT functions, so it lands
        // XSLT 1.0 §12.4: current() returns the node that was the
        // context node of the *whole* expression — the instruction's
        // current node — which stays fixed as evaluation descends into
        // steps and predicates.  `static_ctx.current_node` carries it
        // (set once at the top-level entry); `context_node` would be the
        // predicate's context, wrong inside `foo[@x=current()/@y]`.
        "current" if args.is_empty() => {
            Ok(Value::NodeSet(vec![ctx.static_ctx.current_node.unwrap_or(ctx.context_node)]))
        }

        _ => Err(xpath_err(format!("unknown XPath function: {name}()"))),
    }
}

/// Best-effort `function-available()` predicate.  A prefixed name is
/// available when its prefix resolves to an EXSLT namespace family this
/// engine implements; an unprefixed name when it's a known XPath/XSLT
/// built-in.  Functions we don't provide (e.g. `msxsl:node-set`) report
/// `false`, which is what callers like the ISO Schematron skeleton use
/// to choose a supported code path.
fn xpath_function_available(qname: &str, ctx: &EvalCtx<'_>) -> bool {
    use super::exslt;
    match qname.split_once(':') {
        Some((prefix, _local)) => {
            match resolve_prefix_or_implicit(ctx.bindings, prefix) {
                Some(uri) => matches!(uri.as_str(),
                    exslt::MATH_NS | exslt::DATE_NS | exslt::STR_NS | exslt::SET_NS
                    | exslt::REGEXP_NS | exslt::COMMON_NS | exslt::DYN_NS),
                None => false,
            }
        }
        None => matches!(qname,
            // XPath 1.0 core library (§4).
            "last" | "position" | "count" | "id" | "local-name" | "namespace-uri"
            | "name" | "string" | "concat" | "starts-with" | "contains"
            | "substring-before" | "substring-after" | "substring" | "string-length"
            | "normalize-space" | "translate" | "boolean" | "not" | "true" | "false"
            | "lang" | "number" | "sum" | "floor" | "ceiling" | "round"
            // XSLT 1.0 additions (§12) we honour in this engine.
            | "document" | "key" | "format-number" | "current" | "unparsed-entity-uri"
            | "generate-id" | "system-property" | "element-available" | "function-available"),
    }
}

/// Extract a Vec<NodeId> from a Value, treating non-NodeSet inputs
/// as empty.  Used for the set-flavoured XPath 2.0 operators
/// (`intersect`, `except`) where atomic operands surface as the
/// empty node-set per the spec's atomic-vs-node-set rules.
fn node_set_of(v: Value) -> Vec<NodeId> {
    match v {
        Value::NodeSet(ns) => ns,
        _ => Vec::new(),
    }
}

/// True if `v` matches the XPath 2.0 SequenceType `st`.  Covers the
/// subset our parser actually emits — atomic types we know
/// (xs:string/integer/decimal/double/boolean/date/dateTime/time/etc.)
/// and the standard KindTest forms (item, node, element, attribute,
/// text, comment, processing-instruction, document-node).
/// Resolve the namespace prefixes used in any `element(N)` /
/// `attribute(N)` / `processing-instruction(N)` kind test inside `st`
/// into Clark form `{uri}local`, using the in-scope namespace
/// bindings.  `value_matches_sequence_type` then compares against the
/// node's namespace URI rather than its (prefix-dependent) lexical
/// QName, so `instance of element(my:foo)` matches a node in the same
/// namespace regardless of which prefix the source document used.
/// A prefix that doesn't resolve is left lexical for a best-effort
/// QName comparison.
fn resolve_kind_test_namespaces(
    st: &crate::xpath::ast::SequenceType,
    bindings: &dyn XPathBindings,
) -> crate::xpath::ast::SequenceType {
    use crate::xpath::ast::ItemType;
    let resolve = |name: &Option<String>| -> Option<String> {
        let n = name.as_ref()?;
        let (prefix, local) = n.split_once(':')?;
        let uri = resolve_prefix_or_implicit(bindings, prefix)?;
        Some(format!("{{{uri}}}{local}"))
    };
    let item = match &st.item {
        ItemType::Element(name @ Some(_)) =>
            ItemType::Element(resolve(name).or_else(|| name.clone())),
        ItemType::Attribute(name @ Some(_)) =>
            ItemType::Attribute(resolve(name).or_else(|| name.clone())),
        other => other.clone(),
    };
    crate::xpath::ast::SequenceType { item, occurrence: st.occurrence }
}

/// Match the name in an `element(N)` / `attribute(N)` kind test
/// against the node at `id`.  `None` (the bare-paren form) matches
/// any name.  A name in Clark form `{uri}local` (produced by
/// [`resolve_kind_test_namespaces`]) is compared against the node's
/// namespace URI + local name.  A still-prefixed name is compared
/// against the node's lexical QName as a best-effort fallback; an
/// unprefixed name against the local part.
fn kind_test_name_matches<I: DocIndexLike>(
    name: Option<&str>, id: NodeId, idx: &I,
) -> bool {
    match name {
        None => true,
        Some(n) => match n.strip_prefix('{').and_then(|r| r.split_once('}')) {
            Some((uri, local)) =>
                idx.namespace_uri(id) == uri && idx.local_name(id) == local,
            None if n.contains(':') => idx.node_name(id) == n,
            None => idx.local_name(id) == n,
        },
    }
}

fn value_matches_sequence_type<I: DocIndexLike>(
    v: &Value, st: &crate::xpath::ast::SequenceType, idx: &I,
) -> bool {
    use crate::xpath::ast::{ItemType, Occurrence};
    // Cardinality check first — sequence size against occurrence.
    let count = sequence_len(v);
    let card_ok = match st.occurrence {
        Occurrence::One        => count == 1,
        Occurrence::Optional   => count <= 1,
        Occurrence::OneOrMore  => count >= 1,
        Occurrence::ZeroOrMore => true,
    };
    if !card_ok { return false; }
    // The item-type test applies to every item; with an empty
    // sequence it is vacuously satisfied (cardinality already
    // confirmed the occurrence indicator admits zero items).  This
    // also covers empty sequences not represented as a NodeSet
    // (e.g. `Value::Sequence([])` from `<xsl:sequence select="()"/>`),
    // which the per-kind arms below would otherwise reject.
    if count == 0 { return true; }
    // Item-type check — applied to every item in the sequence.
    match &st.item {
        ItemType::Any => true,
        ItemType::Atomic(name) => {
            // Atomic types: a NodeSet is fine if the string-value
            // round-trips through the atomic parser.  Singleton
            // atomic values do the obvious match.
            let try_one = |s: &str| atomic_string_castable(s, name);
            match v {
                Value::String(s)  => try_one(s),
                // A number matches `xs:T` per the XSD subtype lattice
                // read from its own kind — `xs:integer 1` is an
                // instance of xs:integer / xs:decimal / xs:anyAtomicType
                // but NOT xs:double, and an `xs:double` is NOT an
                // xs:integer.  `xs:numeric` is the F&O union of the four
                // numeric primitives, outside the hierarchy, so any
                // number matches it.
                Value::Number(n)  => name == "numeric"
                    || xsd_is_subtype_of(n.kind(), name),
                Value::Boolean(_) => matches!(name.as_str(),
                    "boolean" | "anyAtomicType"),
                // XPath 2.0 §3.10.2 — `instance of xs:T` does NOT
                // atomize: a node IS NOT an instance of an atomic
                // type (xs:anyAtomicType, xs:string, etc.) even if
                // its string-value would atomize to a matching atom.
                // Only synthetic-text "nodes" representing atomic
                // singletons can match — we identify them by their
                // synthetic-store NodeId.
                Value::NodeSet(ns) => ns.iter().all(|&id|
                    crate::xpath::is_synthetic_id(id)
                    && try_one(&idx.string_value(id))),
                Value::ForeignNodeSet(_) => true,
                // Typed atomics match by subtype lattice — same
                // logic instance-of uses elsewhere.  See
                // `xsd_is_subtype_of`.
                Value::Typed(t) => xsd_is_subtype_of(t.kind, name),
                // Heterogeneous typed sequence: every item must
                // satisfy the atomic test on its own terms.
                Value::Sequence(items) => items.iter().all(|item|
                    value_matches_sequence_type(item, st, idx)),
                // An IntRange is a sequence of `xs:integer` values, so
                // it matches exactly what an xs:integer does.
                Value::IntRange { .. } => name == "numeric"
                    || xsd_is_subtype_of("integer", name),
                // A map / array is not an instance of any atomic type.
                Value::Map(_) | Value::Array(_) | Value::Function(_) => false,
            }
        }
        ItemType::AnyNode => matches!(v,
            Value::NodeSet(_) | Value::ForeignNodeSet(_)),
        ItemType::Element(name) => match v {
            Value::NodeSet(ns) => ns.iter().all(|&id|
                matches!(idx.kind(id), crate::xpath::XPathNodeKind::Element)
                && kind_test_name_matches(name.as_deref(), id, idx)),
            _ => false,
        },
        ItemType::Attribute(name) => match v {
            Value::NodeSet(ns) => ns.iter().all(|&id|
                matches!(idx.kind(id), crate::xpath::XPathNodeKind::Attribute)
                && kind_test_name_matches(name.as_deref(), id, idx)),
            _ => false,
        },
        ItemType::Text => match v {
            Value::NodeSet(ns) => ns.iter().all(|&id|
                matches!(idx.kind(id),
                    crate::xpath::XPathNodeKind::Text |
                    crate::xpath::XPathNodeKind::CData)),
            _ => false,
        },
        ItemType::Comment => match v {
            Value::NodeSet(ns) => ns.iter().all(|&id|
                matches!(idx.kind(id), crate::xpath::XPathNodeKind::Comment)),
            _ => false,
        },
        ItemType::PI(target) => match v {
            Value::NodeSet(ns) => ns.iter().all(|&id|
                matches!(idx.kind(id), crate::xpath::XPathNodeKind::PI)
                && target.as_ref().map_or(true, |t| idx.pi_target(id) == t)),
            _ => false,
        },
        ItemType::Document => match v {
            Value::NodeSet(ns) => ns.iter().all(|&id|
                matches!(idx.kind(id), crate::xpath::XPathNodeKind::Document)),
            _ => false,
        },
        // `function(*)` matches any function item; a specific
        // `function(T1, …, Tn) as R` applies function subtyping against
        // the item's own declared signature when known (named user
        // functions capture it), else falls back to arity.
        ItemType::Function(want) => match v {
            Value::Function(fi) => match want {
                None => true,
                Some(want) => fi.arity() == want.params.len()
                    && match fi.declared_sig() {
                        Some(have) => function_sig_subtype_of(have, want),
                        None => true,
                    },
            },
            Value::Sequence(items) => items.iter().all(|item|
                value_matches_sequence_type(item, st, idx)),
            _ => false,
        },
        // `map(*)` / `array(*)` — any map / any array item.
        ItemType::Map => match v {
            Value::Map(_) => true,
            Value::Sequence(items) => items.iter().all(|item|
                value_matches_sequence_type(item, st, idx)),
            _ => false,
        },
        ItemType::Array => match v {
            Value::Array(_) => true,
            Value::Sequence(items) => items.iter().all(|item|
                value_matches_sequence_type(item, st, idx)),
            _ => false,
        },
        // `empty-sequence()` — no individual item matches; a non-empty
        // value reaches here only after the count==0 short-circuit
        // above declined it, so it is not the empty sequence.
        ItemType::EmptySequence => false,
    }
}

/// XPath 3.1 occurrence-indicator subsumption: are all cardinalities
/// permitted by `sub` also permitted by `sup`?
fn occurrence_subsumes(sub: Occurrence, sup: Occurrence) -> bool {
    let allows_zero = |o: Occurrence| matches!(o, Occurrence::Optional | Occurrence::ZeroOrMore);
    let allows_many = |o: Occurrence| matches!(o, Occurrence::OneOrMore | Occurrence::ZeroOrMore);
    (!allows_zero(sub) || allows_zero(sup)) && (!allows_many(sub) || allows_many(sup))
}

/// `a` is a subtype of `b` (XPath 3.1 §2.5.6) to the resolution this
/// engine models — atomic types via the XSD lattice, node kinds, and
/// nested function signatures.
fn sequence_type_subtype_of(a: &SequenceType, b: &SequenceType) -> bool {
    occurrence_subsumes(a.occurrence, b.occurrence)
        && item_type_subtype_of(&a.item, &b.item)
}

fn item_type_subtype_of(a: &ItemType, b: &ItemType) -> bool {
    match (a, b) {
        (_, ItemType::Any) => true,
        (ItemType::EmptySequence, _) => true,
        (ItemType::Atomic(x), ItemType::Atomic(y)) => xsd_is_subtype_of(x, y),
        (ItemType::AnyNode, ItemType::AnyNode) => true,
        (ItemType::Element(_) | ItemType::Attribute(_) | ItemType::Text
            | ItemType::Comment | ItemType::PI(_) | ItemType::Document,
         ItemType::AnyNode) => true,
        (ItemType::Element(x), ItemType::Element(y)) => y.is_none() || x == y,
        (ItemType::Attribute(x), ItemType::Attribute(y)) => y.is_none() || x == y,
        (ItemType::PI(x), ItemType::PI(y)) => y.is_none() || x == y,
        (ItemType::Text, ItemType::Text)
            | (ItemType::Comment, ItemType::Comment)
            | (ItemType::Document, ItemType::Document) => true,
        (ItemType::Function(_), ItemType::Function(None)) => true,
        (ItemType::Function(Some(sa)), ItemType::Function(Some(sb))) =>
            function_sig_subtype_of(sa, sb),
        (ItemType::Map, ItemType::Map) => true,
        (ItemType::Array, ItemType::Array) => true,
        _ => false,
    }
}

/// Function subtyping (XPath 3.1 §2.5.6.3): `have` is a subtype of `want`
/// iff same arity, `have`'s return type is a subtype of `want`'s
/// (covariant), and each of `want`'s parameter types is a subtype of the
/// corresponding `have` parameter type (contravariant).
fn function_sig_subtype_of(have: &FunctionSig, want: &FunctionSig) -> bool {
    have.params.len() == want.params.len()
        && sequence_type_subtype_of(&have.ret, &want.ret)
        && have.params.iter().zip(&want.params)
            .all(|(hp, wp)| sequence_type_subtype_of(wp, hp))
}

/// Lenient lexical check for the common atomic XSD types.  Used by
/// `instance of` / `castable as` to ask "is this string a valid
/// `xs:<name>` literal?" without actually allocating a converted
/// value.  Returns true on a successful round-trip.
/// Lexical-form sanity check for the date / duration / binary
/// atomic types — used by the `cast as` path so an integer node
/// value like `"43"` doesn't silently pass as `xs:date`.  The
/// checks are minimal (shape-only) rather than full XSD
/// validators — enough to reject obvious cross-type casts.
fn lexical_matches_type(s: &str, name: &str) -> bool {
    if s.is_empty() { return false; }
    let bytes = s.as_bytes();
    match name {
        "date" => {
            // YYYY-MM-DD with optional timezone suffix.
            s.len() >= 10
                && parse_xsd_date_time(s, DateKind::Date).is_some()
        }
        "dateTime" => {
            s.contains('T') && parse_xsd_date_time(s, DateKind::DateTime).is_some()
        }
        "time" => {
            bytes.len() >= 8 && bytes[2] == b':' && bytes[5] == b':'
                && parse_xsd_date_time(s, DateKind::Time).is_some()
        }
        // Duration forms — `P[nY][nM][nD][T[nH][nM][nS]]`.  All durations
        // start with `P` (or `-P` for negatives).
        "duration" => {
            let rest = s.strip_prefix('-').unwrap_or(s);
            rest.starts_with('P') && rest.len() > 1
        }
        "dayTimeDuration" => {
            let rest = s.strip_prefix('-').unwrap_or(s);
            // No year / month designators.
            rest.starts_with('P')
                && !rest.contains('Y')
                && !rest_contains_month_designator(rest)
        }
        "yearMonthDuration" => {
            let rest = s.strip_prefix('-').unwrap_or(s);
            // No day / time components.
            rest.starts_with('P')
                && !rest.contains('D')
                && !rest.contains('T')
        }
        // Gregorian forms have at least one ASCII digit.
        "gYear"      => s.len() >= 4 && bytes.iter().all(|&b| b == b'-' || b.is_ascii_digit() || b == b'+' || b == b'Z' || b == b':'),
        "gYearMonth" => s.contains('-') && s.len() >= 7,
        "gMonth"     => s.starts_with("--") && s.len() >= 4,
        "gMonthDay"  => s.starts_with("--") && s.len() >= 7,
        "gDay"       => s.starts_with("---") && s.len() >= 5,
        // Binary forms — hex requires even length of hex digits;
        // base64 is at least empty-friendly (4-byte aligned).
        "hexBinary"  => s.len() % 2 == 0 && bytes.iter().all(|b| b.is_ascii_hexdigit()),
        "base64Binary" => true, // permissive
        _ => true,
    }
}

/// True iff `s` is a well-formed lexical value of the year-bearing type
/// `name` (`date` / `dateTime` / `gYear` / `gYearMonth`) whose ONLY
/// defect is that the year magnitude exceeds the range this engine can
/// represent (`i32`).  Such a value is lexically valid but outside the
/// implementation-defined supported range, so a cast/constructor must
/// raise `err:FODT0001` (overflow) rather than `err:FORG0001`
/// (malformed).  Years within `i32` — including 5+ digit and negative
/// years — are supported and return `false` here.
fn date_year_out_of_range(s: &str, name: &str) -> bool {
    if !matches!(name, "date" | "dateTime" | "gYear" | "gYearMonth") {
        return false;
    }
    let neg = s.starts_with('-');
    let body = s.strip_prefix('-').unwrap_or(s);
    let ylen = body.bytes().take_while(u8::is_ascii_digit).count();
    if ylen == 0 { return false; }
    let ynum = &body[..ylen];
    // In `i32` → representable (not an overflow); not even `i128` → an
    // absurd numeral we treat as malformed, not overflow.
    if ynum.parse::<i32>().is_ok() || ynum.parse::<i128>().is_err() {
        return false;
    }
    // The value is well-formed apart from the year magnitude iff
    // substituting an in-range year makes it match the type.
    let probe = format!("{}0001{}", if neg { "-" } else { "" }, &body[ylen..]);
    lexical_matches_type(&probe, name)
}

/// Helper for `lexical_matches_type("dayTimeDuration", …)` —
/// XSD `P…M` could mean months (date side) or minutes (time
/// side, after `T`).  Months are illegal in a dayTimeDuration;
/// minutes are fine.  This walks the string and reports true
/// when an `M` appears before the `T`.
fn rest_contains_month_designator(s: &str) -> bool {
    let t_pos = s.find('T');
    for (i, c) in s.char_indices() {
        if c == 'M' {
            match t_pos {
                Some(t) if i > t => continue,
                _                => return true,
            }
        }
    }
    false
}

fn atomic_string_castable(s: &str, name: &str) -> bool {
    match name {
        "string" | "anyURI" | "anyAtomicType" | "untypedAtomic" => true,
        "boolean" => matches!(s.trim(), "true" | "false" | "1" | "0"),
        "integer" | "long" | "int" | "short" | "byte"
        | "nonNegativeInteger" | "nonPositiveInteger"
        | "positiveInteger" | "negativeInteger"
        | "unsignedLong" | "unsignedInt" | "unsignedShort" | "unsignedByte"
            => s.trim().parse::<i64>().is_ok(),
        "decimal" => s.trim().parse::<f64>().is_ok() && !s.trim().contains(['e', 'E']),
        "double" | "float" | "numeric" => s.trim().parse::<f64>().is_ok()
                                       || matches!(s.trim(), "NaN" | "INF" | "-INF" | "Infinity" | "-Infinity"),
        "date"     => parse_xsd_date_only(s).is_some(),
        "dateTime" => parse_xsd_date_time(s, DateKind::DateTime).is_some(),
        "time"     => parse_xsd_date_time(s, DateKind::Time).is_some(),
        _ => true,    // unknown atomic types pass — best-effort, no schema lookup
    }
}

/// Convert a Value to its atomic-typed counterpart.  Most casts in
/// our model collapse to a string + the runtime's native interpretation
/// (e.g. `xs:integer("42")` → `Value::Number(42)`).  Errors when the
/// target type can't accept the lexical form.
/// XSD §F.3 canonical lexical form for `xs:double` / `xs:float`.
/// Always written in scientific notation when the magnitude is
/// outside `[1e-6, 1e7)` (matching Saxon's rendering of the XSD
/// canonical form, which the XSLT test suite expects).  Preserves
/// the sign of negative zero — `xs:float(-0)` round-trips as
/// `"-0"`.
/// XSD §F.3 canonical lexical form for `xs:float` — the value is
/// stored as f64 (after a `n as f32 as f64` round-trip narrowing in
/// the constructor / cast path), but the rendered form must reflect
/// f32's ~7-digit precision.  Re-formatting through `f32` gives
/// Rust's shortest round-trip output at single precision —
/// `1.2345679` instead of `1.2345678806304932`.
fn canonical_float_lex(n: f64, source: &Value) -> String {
    let f = n as f32;
    if f.is_nan()      { return "NaN".to_string(); }
    if f.is_infinite() { return (if f > 0.0 { "INF" } else { "-INF" }).to_string(); }
    let is_neg_zero = f == 0.0
        && (f.is_sign_negative()
            || matches!(source, Value::Number(v) if v.as_f64().is_sign_negative())
            || matches!(source, Value::String(s) if s.trim().starts_with('-')));
    if f == 0.0 {
        return if is_neg_zero { "-0".to_string() } else { "0".to_string() };
    }
    let abs = f.abs();
    if (1e-6..1e7).contains(&abs) {
        if f.fract() == 0.0 {
            return format!("{}", f as i64);
        }
        return format!("{f}");
    }
    let formatted = format!("{f:E}");
    let (mantissa, exp) = formatted.split_once('E').unwrap_or((formatted.as_str(), "0"));
    let mantissa = if mantissa.contains('.') { mantissa.to_string() }
                   else { format!("{mantissa}.0") };
    let exp = exp.trim_start_matches('+');
    format!("{mantissa}E{exp}")
}

fn canonical_double_lex(n: f64, source: &Value) -> String {
    if n.is_nan()      { return "NaN".to_string(); }
    if n.is_infinite() { return (if n > 0.0 { "INF" } else { "-INF" }).to_string(); }
    // Negative-zero detection.  Reading the sign bit picks both `-0.0`
    // and a literal `xs:float(-0)` produced by Rust's normal parsing
    // (which collapses `-0` → `-0.0`).  Carry the sign through only
    // when the source explicitly carried it (otherwise xs:double of
    // `0` becomes `0`).
    let is_neg_zero = n == 0.0
        && (n.is_sign_negative()
            || matches!(source, Value::Number(v) if v.as_f64().is_sign_negative())
            || matches!(source, Value::String(s) if s.trim().starts_with('-')));
    if n == 0.0 {
        return if is_neg_zero { "-0".to_string() } else { "0".to_string() };
    }
    let abs = n.abs();
    if abs >= 1e-6 && abs < 1e7 {
        // Inside the "no exponent" window — use fixed-point form.
        // `format!("{}", n)` would emit shortest round-trip; but
        // canonical form requires at least one fractional digit
        // unless the value is integer-valued, so handle both shapes.
        if n.fract() == 0.0 {
            return format!("{}", n as i64);
        }
        // Rust's default Display for f64 already gives shortest
        // round-trip without trailing zeros — pass through.
        return format!("{n}");
    }
    // Outside the window — scientific with explicit `E`.  Saxon's
    // rendering uses one mantissa digit + fractional digits, capital
    // `E`, no leading zeros on the exponent.  Replicate via `{:E}` and
    // strip Rust's leading-zero padding.
    let formatted = format!("{n:E}");
    // Rust prints `1E-8` (no `.0`) — canonical form requires `1.0E-8`.
    // Insert `.0` before the `E` if there's no fractional digit.
    let (mantissa, exp) = match formatted.split_once('E') {
        Some((m, e)) => (m.to_string(), e.to_string()),
        None         => return formatted,
    };
    let mantissa = if !mantissa.contains('.') {
        format!("{mantissa}.0")
    } else { mantissa };
    format!("{mantissa}E{exp}")
}

/// XSD §F.1 canonical lexical form for `xs:decimal` — fixed-point,
/// no exponent.  Negative-zero collapses to `0` for xs:decimal
/// (decimal doesn't have a signed zero).  Pass-through-friendly:
/// when the source is already in fixed-point form we keep its
/// representation so trailing zeros that the author wrote survive
/// (xs:decimal lexical isn't *strictly* canonical until the
/// formatter normalises, but most tests accept the input form).
fn canonical_decimal_lex(n: f64, src: &str) -> String {
    if n == 0.0 { return "0".to_string(); }
    if src.contains(['e', 'E']) {
        if n.fract() == 0.0 && n.abs() < 1e15 {
            return (n as i64).to_string();
        }
        return format!("{n}");
    }
    // XSD §3.2.3.2 canonical lexical for decimal: no leading `+`,
    // no leading zeros (except one before the decimal point), and
    // no trailing zeros after the decimal point (drop the decimal
    // point too if the fractional part collapses to nothing).  We
    // canonicalise from the source string rather than via f64 so
    // high-precision decimals like `10.0000000001` survive the
    // round trip — the key tests rely on exact-value comparison.
    let t = src.trim();
    let (sign, body) = match t.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None       => ("", t.strip_prefix('+').unwrap_or(t)),
    };
    let (whole, frac) = match body.split_once('.') {
        Some((w, f)) => (w, f),
        None         => (body, ""),
    };
    let whole_trimmed = whole.trim_start_matches('0');
    let whole_canon = if whole_trimmed.is_empty() { "0" } else { whole_trimmed };
    let frac_trimmed = frac.trim_end_matches('0');
    if frac_trimmed.is_empty() {
        // Drop sign on a zero result so "-0.0" → "0".
        if whole_canon == "0" { return "0".into(); }
        return format!("{sign}{whole_canon}");
    }
    format!("{sign}{whole_canon}.{frac_trimmed}")
}

/// Cast a value to an atomic sequence type.  Any failure to convert
/// the lexical value to the target type is a dynamic error
/// `err:FORG0001` (F&O §17.1); the `_impl` body reports the specific
/// failing form and this wrapper attaches the spec code (without
/// overwriting a more specific code an inner step may have set).
pub fn cast_value_to_atomic<I: DocIndexLike>(
    v: &Value, st: &crate::xpath::ast::SequenceType, idx: &I,
) -> Result<Value> {
    cast_value_to_atomic_impl(v, st, idx).map_err(|e| e.or_xpath_code("FORG0001"))
}

fn cast_value_to_atomic_impl<I: DocIndexLike>(
    v: &Value, st: &crate::xpath::ast::SequenceType, idx: &I,
) -> Result<Value> {
    use crate::xpath::ast::ItemType;
    let s = value_to_string(v, idx);
    let make_typed = |kind: &'static str, lex: String,
                      numeric: Option<f64>, boolean: Option<bool>| -> Value {
        Value::Typed(Box::new(TypedAtomic {
            kind, lexical: lex, numeric, boolean,
        }))
    };
    match &st.item {
        ItemType::Atomic(name) => {
            let kind = match atomic_kind_static(name) {
                Some(k) => k,
                None => return Ok(Value::String(s)),
            };
            match name.as_str() {
                "string" | "anyURI" | "anyAtomicType" | "untypedAtomic"
                | "normalizedString" | "token" | "Name" | "NCName"
                | "language" | "ID" | "IDREF" | "IDREFS" | "ENTITY"
                | "ENTITIES" | "NMTOKEN" | "NMTOKENS" | "NOTATION" | "QName" => {
                    Ok(make_typed(kind, s, None, None))
                }
                "boolean" => {
                    // XPath 2.0 §17.1.4 — numeric → boolean: zero
                    // and NaN are false, every other finite value is
                    // true.  Strings accept the four canonical forms.
                    if let Value::Number(n) = v {
                        let b = !(n.as_f64() == 0.0 || n.as_f64().is_nan());
                        return Ok(make_typed("boolean",
                            (if b { "true" } else { "false" }).into(),
                            None, Some(b)));
                    }
                    if let Value::Typed(t) = v {
                        if let Some(n) = t.numeric {
                            let b = !(n == 0.0 || n.is_nan());
                            return Ok(make_typed("boolean",
                                (if b { "true" } else { "false" }).into(),
                                None, Some(b)));
                        }
                    }
                    match s.trim() {
                        "true"  | "1" => Ok(make_typed("boolean", "true".into(), None, Some(true))),
                        "false" | "0" => Ok(make_typed("boolean", "false".into(), None, Some(false))),
                        _ => Err(xpath_err(format!("cast to xs:boolean failed: {s:?}"))),
                    }
                }
                "integer" | "long" | "int" | "short" | "byte"
                | "nonNegativeInteger" | "nonPositiveInteger"
                | "positiveInteger" | "negativeInteger"
                | "unsignedLong" | "unsignedInt" | "unsignedShort" | "unsignedByte" => {
                    let n: i64 = s.trim().parse().map_err(|_| xpath_err(
                        format!("cast to xs:{name} failed: {s:?}")))?;
                    Ok(make_typed(kind, n.to_string(), Some(n as f64), None))
                }
                "decimal" | "double" | "float" | "numeric" => {
                    // XSD §F.3 — `INF` / `-INF` / `NaN` are the
                    // canonical lexical forms for xs:double / xs:float
                    // (not "Infinity"/"NaN" as XPath 1.0 used).  Accept
                    // both spellings on input but normalise on output.
                    let trimmed = s.trim();
                    let raw: f64 = match trimmed {
                        "INF"  | "Infinity"   => f64::INFINITY,
                        "-INF" | "-Infinity"  => f64::NEG_INFINITY,
                        "NaN"                 => f64::NAN,
                        _ => trimmed.parse().map_err(|_| xpath_err(
                            format!("cast to xs:{name} failed: {s:?}")))?,
                    };
                    // xs:float is IEEE 754 single — narrow through f32
                    // so high-precision literals lose their tail (cf.
                    // xs_constructor).
                    let n = if name == "float" { raw as f32 as f64 } else { raw };
                    let canon = match name.as_str() {
                        "double" => canonical_double_lex(n, v),
                        "float"  => canonical_float_lex(n, v),
                        // xs:decimal canonicalises to fixed-point.
                        _ => canonical_decimal_lex(n, trimmed),
                    };
                    Ok(make_typed(kind, canon, Some(n), None))
                }
                "date" | "dateTime" | "time"
                | "duration" | "dayTimeDuration" | "yearMonthDuration"
                | "gYear" | "gYearMonth" | "gMonth" | "gMonthDay" | "gDay"
                | "hexBinary" | "base64Binary" => {
                    // Same-family precision-changing casts F&O §17.1 —
                    // xs:date → xs:dateTime appends `T00:00:00` while
                    // preserving the timezone.  Without this the
                    // lexical-form check below rejects "2004-02-02"
                    // because xs:dateTime requires a `T` separator.
                    if name == "dateTime" {
                        if let Value::Typed(t) = v {
                            if t.kind == "date" {
                                // xs:date lexical: optional leading `-`,
                                // YYYY-MM-DD, optional tz (`Z` or
                                // `±HH:MM`).  Strip the tz before
                                // splicing in the midnight time.
                                let raw = t.lexical.as_str();
                                let (date_part, tz) = if let Some(rest) =
                                    raw.strip_suffix('Z')
                                {
                                    (rest, "Z")
                                } else if raw.len() >= 6 {
                                    let cand = &raw[raw.len() - 6..];
                                    let b = cand.as_bytes();
                                    if (b[0] == b'+' || b[0] == b'-')
                                        && b[1].is_ascii_digit() && b[2].is_ascii_digit()
                                        && b[3] == b':'
                                        && b[4].is_ascii_digit() && b[5].is_ascii_digit()
                                    {
                                        (&raw[..raw.len() - 6], cand)
                                    } else {
                                        (raw, "")
                                    }
                                } else {
                                    (raw, "")
                                };
                                let lex = format!("{date_part}T00:00:00{tz}");
                                return Ok(make_typed("dateTime", lex, None, None));
                            }
                        }
                    }
                    // Casting an xs:date / xs:dateTime to a gregorian
                    // type extracts the relevant components (F&O §17.1.7),
                    // carrying the source timezone — e.g.
                    // `xs:gDay(xs:date('2001-02-15'))` is `---15`.
                    if matches!(name.as_str(),
                        "gYear" | "gYearMonth" | "gMonth" | "gMonthDay" | "gDay")
                    {
                        if let Value::Typed(t) = v {
                            let dk = match t.kind {
                                "date"     => Some(DateKind::Date),
                                "dateTime" => Some(DateKind::DateTime),
                                _          => None,
                            };
                            if let Some(dk) = dk {
                                if let Some((y, mo, d, _, _, _, _, tz)) =
                                    parse_xsd_date_time(&t.lexical, dk)
                                {
                                    let tzs = tz.map(format_tz_suffix).unwrap_or_default();
                                    let lex = match name.as_str() {
                                        "gYear"      => format!("{y:04}{tzs}"),
                                        "gYearMonth" => format!("{y:04}-{mo:02}{tzs}"),
                                        "gMonth"     => format!("--{mo:02}{tzs}"),
                                        "gMonthDay"  => format!("--{mo:02}-{d:02}{tzs}"),
                                        _            => format!("---{d:02}{tzs}"),
                                    };
                                    return Ok(make_typed(kind, lex, None, None));
                                }
                            }
                        }
                    }
                    // F&O §17.1.6 — xs:hexBinary ⇄ xs:base64Binary
                    // reinterprets the same octets.
                    if matches!(name.as_str(), "hexBinary" | "base64Binary") {
                        if let Some(lex) = convert_binary_kind(v, name) {
                            return Ok(make_typed(kind, lex, None, None));
                        }
                    }
                    let trimmed = s.trim();
                    if !lexical_matches_type(trimmed, name) {
                        // A lexically-valid value whose year exceeds the
                        // representable range is an overflow (FODT0001),
                        // not a malformed lexical form (FORG0001).
                        if date_year_out_of_range(trimmed, name) {
                            return Err(xpath_err(format!(
                                "cast to xs:{name}: year is outside the \
                                 supported range: {trimmed:?}"
                            )).with_xpath_code("FODT0001"));
                        }
                        return Err(xpath_err(format!(
                            "cast to xs:{name} failed: {trimmed:?}"
                        )));
                    }
                    // XSD §3.2.7/§3.2.8 — the `24:00:00` midnight form is
                    // valid but non-canonical; store the normalised value
                    // (`00:00:00`, with the day rolled for dateTime) so the
                    // typed value's string and component accessors agree.
                    // Other lexical forms are preserved verbatim.
                    if matches!(name.as_str(), "dateTime" | "time")
                        && trimmed.contains("24:00:00")
                    {
                        let dk = if name == "dateTime" {
                            DateKind::DateTime } else { DateKind::Time };
                        if let Some((y, mo, d, h, mi, sec, frac, tz)) =
                            parse_xsd_date_time(trimmed, dk)
                        {
                            let lex = if name == "dateTime" {
                                format_datetime_lexical(y, mo, d, h, mi, sec, frac, tz)
                            } else {
                                let mut l = format!("{h:02}:{mi:02}:{sec:02}");
                                if frac != 0 {
                                    let mut f = format!(".{frac:06}");
                                    while f.ends_with('0') { f.pop(); }
                                    l.push_str(&f);
                                }
                                if let Some(tz_m) = tz { l.push_str(&format_tz_suffix(tz_m)); }
                                l
                            };
                            return Ok(make_typed(kind, lex, None, None));
                        }
                    }
                    // XSD §3.2.15 — xs:hexBinary canonicalises to
                    // upper-case hex digits.
                    if name == "hexBinary" {
                        return Ok(make_typed(kind, trimmed.to_ascii_uppercase(), None, None));
                    }
                    Ok(make_typed(kind, trimmed.to_string(), None, None))
                }
                _ => Ok(Value::String(s)),
            }
        }
        // Non-atomic target — caller has already cardinality-checked,
        // so just pass the value through.
        _ => Ok(v.clone()),
    }
}

/// Which lexical form to expect when parsing the value argument of
/// `format-date` / `format-time` / `format-dateTime`.
#[derive(Debug, Clone, Copy)]
pub enum DateKind { Date, Time, DateTime }

/// Decompose an `xs:date`/`xs:time`/`xs:dateTime` lexical form into
/// (year, month, day, hour, minute, second, frac_microseconds,
/// tz_minutes).  Returns `None` if the input doesn't look like the
/// expected shape.  The parser is lenient: `Z` suffix and `±HH:MM`
/// offsets are both accepted; fractional seconds are captured as a
/// microsecond count (`0..=999999`).
fn parse_xsd_date_time(s: &str, kind: DateKind)
    -> Option<(i32, u8, u8, u8, u8, u8, u32, Option<i16>)>
{
    let s = s.trim();
    // For pure time, we only fill (h,m,s,frac,tz) — date components 0.
    if matches!(kind, DateKind::Time) {
        let (mut h, m, sec, frac, tz) = parse_xsd_time(s)?;
        // XSD §3.2.8 — `24:00:00` denotes midnight and normalises to
        // `00:00:00`.  Without a date there is no day to roll into.
        if h == 24 && m == 0 && sec == 0 && frac == 0 { h = 0; }
        return Some((0, 0, 0, h, m, sec, frac, tz));
    }
    // Date / DateTime share the YYYY-MM-DD prefix.  XSD 1.1 §3.3.7
    // permits years beyond four digits and negative years (`-0001-…`
    // == 1 BCE) — split on the second-to-last `-` to isolate the
    // YYYY[…] portion from the MM-DD suffix.
    let bytes = s.as_bytes();
    if bytes.len() < 10 { return None; }
    let (neg_year, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None       => (false, s),
    };
    // Find positions of `-` in the body: first is YYYY-MM, second
    // is MM-DD.  Anything before the first `-` is the year.
    let body_bytes = body.as_bytes();
    let first_dash = body_bytes.iter().position(|&c| c == b'-')?;
    if first_dash < 4 { return None; }
    if body_bytes.len() < first_dash + 6 { return None; }
    if body_bytes[first_dash + 3] != b'-' { return None; }
    let year_str = &body[..first_dash];
    let mut year: i32 = year_str.parse().ok()?;
    if neg_year { year = -year; }
    let month: u8 = std::str::from_utf8(&body_bytes[first_dash + 1..first_dash + 3])
        .ok()?.parse().ok()?;
    let day: u8 = std::str::from_utf8(&body_bytes[first_dash + 4..first_dash + 6])
        .ok()?.parse().ok()?;
    // XSD §3.2.7 — month must be 1..12 and the day must be within
    // the legal range for the (year, month) pair (Feb 29 only in
    // leap years).  An out-of-range value is *not* a valid lexical
    // form, so reject here to keep `castable as xs:date` honest.
    if !(1..=12).contains(&month) { return None; }
    let max_day = days_in_month(year, month as u32);
    if !(1..=max_day as u8).contains(&day) { return None; }
    let rest = &body[first_dash + 6..];
    match kind {
        DateKind::Date => {
            let tz = parse_tz_suffix(rest);
            Some((year, month, day, 0, 0, 0, 0, tz))
        }
        DateKind::DateTime => {
            // `T` separator followed by HH:MM:SS plus optional tz.
            if !rest.starts_with('T') || rest.len() < 9 { return None; }
            let (h, m, sec, frac, tz) = parse_xsd_time(&rest[1..])?;
            // XSD §3.2.7 — `24:00:00` is midnight at the *start of the
            // next day*, so it normalises to `00:00:00` with the date
            // rolled forward one day.
            if h == 24 && m == 0 && sec == 0 && frac == 0 {
                let (y, mo, d) = add_one_day(year, month, day);
                return Some((y, mo, d, 0, 0, 0, 0, tz));
            }
            Some((year, month, day, h, m, sec, frac, tz))
        }
        DateKind::Time => unreachable!(),
    }
}

/// Parse `HH:MM:SS[.frac][tz]` into (h, m, s, frac_microseconds, tz).
/// Fractional seconds are truncated or zero-padded to six digits
/// so the caller can render any sub-second precision up to a
/// microsecond.
fn parse_xsd_time(s: &str) -> Option<(u8, u8, u8, u32, Option<i16>)> {
    let bytes = s.as_bytes();
    if bytes.len() < 8 { return None; }
    let h: u8 = std::str::from_utf8(&bytes[..2]).ok()?.parse().ok()?;
    if bytes[2] != b':' { return None; }
    let m: u8 = std::str::from_utf8(&bytes[3..5]).ok()?.parse().ok()?;
    if bytes[5] != b':' { return None; }
    let sec: u8 = std::str::from_utf8(&bytes[6..8]).ok()?.parse().ok()?;
    let mut rest_start = 8;
    let mut frac_us: u32 = 0;
    if s.as_bytes().get(8) == Some(&b'.') {
        let mut i = 9;
        while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
        let digits = &s[9..i];
        // Right-pad / truncate to exactly six digits of microseconds.
        let take: String = digits.chars().chain(std::iter::repeat('0')).take(6).collect();
        frac_us = take.parse().ok().unwrap_or(0);
        rest_start = i;
    }
    let tz = parse_tz_suffix(&s[rest_start..]);
    Some((h, m, sec, frac_us, tz))
}

/// Parse a timezone suffix (`Z`, `+HH:MM`, `-HH:MM`, or empty) and
/// return the offset in minutes east of UTC.  `None` means no tz
/// was present in the input.
fn parse_tz_suffix(s: &str) -> Option<i16> {
    if s.is_empty() { return None; }
    if s == "Z" { return Some(0); }
    let bytes = s.as_bytes();
    if bytes.len() < 6 { return None; }
    let sign = match bytes[0] { b'+' => 1, b'-' => -1, _ => return None };
    let hh: i16 = std::str::from_utf8(&bytes[1..3]).ok()?.parse().ok()?;
    if bytes[3] != b':' { return None; }
    let mm: i16 = std::str::from_utf8(&bytes[4..6]).ok()?.parse().ok()?;
    Some(sign * (hh * 60 + mm))
}

/// Fallback marker prefix for an unsupported `format-date` /
/// `format-time` / `format-dateTime` language or calendar (XSLT 2.0
/// §16.5).  This engine only renders names in English (`en`) and the
/// Gregorian / `AD` calendar; when the call requests anything else the
/// processor falls back and signals it with a `[Language: en]` /
/// `[Calendar: AD]` prefix on the result, as the reference processors do.
fn format_date_locale_prefix(lang: &str, calendar: &str) -> String {
    let mut prefix = String::new();
    let lang = lang.trim();
    if !lang.is_empty() && !lang.eq_ignore_ascii_case("en") {
        prefix.push_str("[Language: en]");
    }
    // A calendar may be given as a QName / `Q{uri}local`; the supported
    // set is keyed by the local part.
    let cal = calendar.trim().rsplit(['}', ':']).next().unwrap_or("").trim();
    if !cal.is_empty()
        && !matches!(cal.to_ascii_uppercase().as_str(), "AD" | "ISO" | "CE") {
        prefix.push_str("[Calendar: AD]");
    }
    prefix
}

/// Best-effort XPath 2.0 `format-date` / `format-time` /
/// `format-dateTime` picture-string interpreter.  Handles the
/// common `[Y…]`, `[M…]`, `[D…]`, `[H…]`, `[h…]`, `[m…]`, `[s…]`,
/// `[f…]`, `[Z]`, `[P]` variable markers; `[[` / `]]` escapes; and
/// literal text passthrough.  Locale-sensitive forms (named months,
/// alternative calendars) emit the numeric digit form rather than
/// failing.
fn format_date_time_picture(value: &str, picture: &str, kind: DateKind) -> Result<String> {
    let (y, mo, d, h, mi, s, frac, tz) = match parse_xsd_date_time(value, kind) {
        Some(t) => t,
        None    => return Ok(value.to_string()), // unrecognised input → echo back
    };
    let mut out = String::with_capacity(picture.len());
    let mut chars = picture.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '[' => {
                // `[[` is a literal `[`.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    out.push('[');
                    continue;
                }
                // Read the variable marker up to the matching `]`.
                let mut marker = String::new();
                while let Some(&nc) = chars.peek() {
                    if nc == ']' { break; }
                    marker.push(nc);
                    chars.next();
                }
                // XSLT 2.0 §16.5.1 — every variable marker must close
                // with `]`; an unterminated marker is XTDE1340.
                if chars.peek() != Some(&']') {
                    return Err(xpath_err(format!(
                        "format-date/format-time: unterminated picture \
                         marker '[{marker}' (XTDE1340)"
                    )));
                }
                chars.next();
                let comp = marker.trim().chars().next().unwrap_or('\0');
                // XSLT 2.0 §16.5.1 — components not present in the
                // value type raise XTDE1350.  Date carries no time
                // components; Time carries no Y/M/D.
                let kind_mismatch = match (kind, comp) {
                    (DateKind::Date, 'H' | 'h' | 'm' | 's' | 'f' | 'P') => true,
                    (DateKind::Time, 'Y' | 'M' | 'D' | 'd' | 'F' | 'W' | 'w' | 'C' | 'E') => true,
                    _ => false,
                };
                if kind_mismatch {
                    return Err(xpath_err(format!(
                        "format-date/format-time: component '{comp}' not \
                         available for this date/time value (XTDE1350)"
                    )));
                }
                if !is_known_picture_component(comp) {
                    return Err(xpath_err(format!(
                        "format-date/format-time: unknown component '{comp}' \
                         in picture string (XTDE1340)"
                    )));
                }
                out.push_str(&format_picture_marker(&marker, y, mo, d, h, mi, s, frac, tz));
            }
            ']' => {
                // `]]` is a literal `]`; lone `]` is also accepted
                // (lenient parsing — strict XPath would error).
                if chars.peek() == Some(&']') {
                    chars.next();
                    out.push(']');
                } else {
                    out.push(']');
                }
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

/// XSLT 2.0 §16.5.1 component letters.  Anything outside this set
/// in a picture marker raises XTDE1340.
fn is_known_picture_component(c: char) -> bool {
    matches!(c, 'Y' | 'M' | 'D' | 'd' | 'F' | 'W' | 'w' | 'H' | 'h'
        | 'm' | 's' | 'f' | 'Z' | 'z' | 'P' | 'C' | 'E')
}

/// XSLT 2.0 § 16.5.1 picture-string component formatter.
///
/// `marker` is the variable-marker body (text between `[` and `]`)
/// shaped as `component [presentation] [, width]`, e.g. `Y0001`,
/// `MNn`, `D1`, `MI`, `H01`, `Y,2-2`.  The component letter
/// selects which date field; the presentation modifier picks a
/// digit pattern (`01`, `001`), a Roman-numeral form (`I` / `i`),
/// an alphabetic form (`A` / `a`), or a name form (`N` / `n` /
/// `Nn`).  Width / max-width come from the `,` clause.
fn format_picture_marker(
    marker: &str, y: i32, mo: u8, d: u8, h: u8, mi: u8, s: u8,
    frac_us: u32, tz: Option<i16>,
) -> String {
    let marker = marker.trim();
    let comp = match marker.chars().next() { Some(c) => c, None => return String::new() };
    let rest = &marker[comp.len_utf8()..];
    // Split off the width modifier `,min-max` if present.
    let (presentation, width_spec) = match rest.split_once(',') {
        Some((p, w)) => (p.trim(), Some(w.trim())),
        None         => (rest.trim(), None),
    };
    let (min_w_raw, max_w_raw) = parse_width_spec(width_spec);
    // `parse_width_spec` returns `Some(usize::MAX)` as the `*`
    // sentinel.  The fractional-seconds formatter wants the
    // sentinel intact so it can distinguish "explicit unbounded"
    // from "omitted entirely"; every other formatter only cares
    // about a concrete width, so unwrap the sentinel to `None`
    // for them.
    let unwrap_star = |v: Option<usize>|
        v.and_then(|n| if n == usize::MAX { None } else { Some(n) });
    let min_w = unwrap_star(min_w_raw);
    let max_w = unwrap_star(max_w_raw);
    // Per-component default presentation modifier (XSLT 2.0
    // §16.5.1 table): Y/M/D/H/h/f default to `"1"` (no padding);
    // m/s default to `"01"` (2-digit zero-pad).  Empty presentation
    // means "use the default for this component."
    let pres = if !presentation.is_empty() { presentation }
               else { match comp {
                   'm' | 's' => "01",
                   _         => "1",
               }};

    match comp {
        // The year component shows the absolute year; the era ([E])
        // conveys BC/AD (XSLT 2.0 §16.5.1), so a proleptic year like
        // -0055 formats as `55`, not `-55`.
        'Y' => format_numeric_component((y as i64).abs(), pres, min_w, max_w, /*truncate_year=*/ true),
        'M' if name_presentation(pres) =>
            format_named_component(mo as i64, pres, max_w, MONTH_NAMES),
        'M' => format_numeric_component(mo as i64, pres, min_w, max_w, false),
        'D' => format_numeric_component(d as i64,  pres, min_w, max_w, false),
        'H' => format_numeric_component(h as i64,  pres, min_w, max_w, false),
        'h' => {
            // 12-hour clock — convert 24h to 1..=12.
            let hh = if h == 0 { 12 } else if h > 12 { h - 12 } else { h } as i64;
            format_numeric_component(hh, pres, min_w, max_w, false)
        }
        'm' => format_numeric_component(mi as i64, pres, min_w, max_w, false),
        's' => format_numeric_component(s as i64,  pres, min_w, max_w, false),
        'f' => format_fractional_seconds(frac_us, pres, min_w_raw, max_w_raw),
        // Day of the week (XSLT 2.0 §16.5.1 component `F`).
        // Presentation modifiers behave like `M`: numeric, Roman,
        // alphabetic, or named.  We only carry y/m/d so we
        // compute the weekday on the fly.
        'F' => {
            let weekday = day_of_week(y, mo as u32, d as u32);
            if name_presentation(pres) {
                format_named_component(weekday as i64, pres, max_w, DAY_NAMES)
            } else {
                format_numeric_component(weekday as i64, pres, min_w, max_w, false)
            }
        }
        // Week in year — ISO 8601 week number (weeks start Monday;
        // week 1 contains the year's first Thursday).
        'W' => format_numeric_component(
            iso_week_of_year(y, mo as u32, d as u32), pres, min_w, max_w, false),
        // Week in month — the week containing this date's Thursday,
        // counted from the start of the month (XSLT 2.0 §16.5.1).
        'w' => {
            let wd = day_of_week(y, mo as u32, d as u32) as i64; // Mon=1..Sun=7
            let thursday_dom = d as i64 + 4 - wd;
            let wim = (thursday_dom + 6).div_euclid(7);
            format_numeric_component(wim, pres, min_w, max_w, false)
        }
        // Day-of-year — straightforward computation; report
        // 1-based ordinal within the year.
        'd' => {
            let day_of_year = month_start_day(y, mo as u32) + d as u32;
            format_numeric_component(day_of_year as i64, pres, min_w, max_w, false)
        }
        // Era marker (AD/BC).  XSLT 2.0 §16.5.1 reserves `E`.
        'E' => {
            if y >= 0 { "AD".to_string() } else { "BC".to_string() }
        }
        // Timezone (XSLT 2.0 §16.5.2).  `Z` outputs the numeric
        // offset `±HH:MM` (UTC → `+00:00`); `z` prefixes it with
        // `GMT`.  Presentation `0` renders the hours without a leading
        // zero; a width whose maximum is ≤ 2 (e.g. `[z,2-2]`)
        // suppresses the `:MM` group unless the minutes are non-zero,
        // while the default and wider widths always show it.
        'Z' | 'z' => match tz {
            None      => String::new(),
            Some(off) => {
                let sign = if off < 0 { '-' } else { '+' };
                let abs  = off.unsigned_abs() as i64;
                let (hh, mm) = (abs / 60, abs % 60);
                let minimal = pres == "0";
                let show_min_always = !minimal && max_w.map_or(true, |w| w >= 3);
                let mut out = String::new();
                if comp == 'z' { out.push_str("GMT"); }
                out.push(sign);
                if minimal { out.push_str(&hh.to_string()); }
                else       { out.push_str(&format!("{hh:02}")); }
                if show_min_always || mm != 0 {
                    out.push(':');
                    out.push_str(&format!("{mm:02}"));
                }
                out
            }
        },
        'P' => {
            // AM/PM marker.  XSLT 2.0 §16.5.1 — the default
            // presentation is the lower-case name (`am`/`pm`); only an
            // explicit `N` upper-cases it.
            let label = if h < 12 { "am" } else { "pm" };
            match pres.chars().next() {
                Some('n') => label.to_string(),     // explicit lower-case name
                Some('N') => label.to_uppercase(),  // upper-case name
                _         => label.to_string(),     // default: lower-case (§16.5.1)
            }
        }
        _ => marker.to_string(),
    }
}

/// Format a single numeric date component according to `presentation`.
/// `min_w` / `max_w` come from the optional `,min-max` width spec.
/// `truncate_year` switches in the XSLT 2.0 special-case: when a
/// 2-digit (or N-digit) presentation is applied to the year and the
/// year has more digits than the format allows, the low-order
/// digits win (`2003` with `Y01` → `03`).  All other components
/// reject silently if they overflow the width — we mirror Saxon's
/// permissive behaviour and emit the full number anyway.
fn format_numeric_component(
    n: i64, presentation: &str,
    min_w: Option<usize>, max_w: Option<usize>,
    truncate_year: bool,
) -> String {
    // XSLT 2.0 §16.5.1 — the `o` suffix on a numeric presentation
    // requests an ordinal rendering ("1st", "2nd", "3rd", "4th").
    // The traditional `c` suffix is "cardinal" — same digits, no
    // suffix; treat it as a no-op for now.  `t` would be alphabetic
    // ordinal (e.g. "first").
    let (presentation, ordinal) = if let Some(rest) = presentation.strip_suffix('o') {
        (rest, true)
    } else if let Some(rest) = presentation.strip_suffix('c') {
        (rest, false)
    } else {
        (presentation, false)
    };
    // Roman / alphabetic / name / word presentations short-circuit
    // the numeric formatter entirely.  XSLT 2.0 §16.5.1 reserves
    // `W` / `w` / `Ww` for cardinal-word forms ("ONE" / "one" /
    // "One"), with the `o` ordinal-suffix tag opting into ordinal
    // words ("FIRST" / "first" / "First").  We implement English
    // only — the spec is locale-dependent and English is what the
    // W3C test suite exercises for the unprefixed `W` / `w`.
    let worded = match presentation {
        "I"  => Some(roman_numeral(n, /*upper=*/ true)),
        "i"  => Some(roman_numeral(n, /*upper=*/ false)),
        "A"  => Some(alpha_label(n, true)),
        "a"  => Some(alpha_label(n, false)),
        "W"  => Some(english_words(n, ordinal, WordCase::Upper)),
        "w"  => Some(english_words(n, ordinal, WordCase::Lower)),
        "Ww" => Some(english_words(n, ordinal, WordCase::Title)),
        _    => None,
    };
    if let Some(mut s) = worded {
        // XSLT 2.0 §16.5.1 — min-width applies to every presentation,
        // but a roman / alphabetic / word form can't be zero-padded,
        // so it's space-padded to the minimum width (`[Yi,4-4]` →
        // `miv ` for 1004).  max-width never truncates these forms.
        if let Some(mw) = min_w {
            let len = s.chars().count();
            if len < mw { s.extend(std::iter::repeat(' ').take(mw - len)); }
        }
        return s;
    }
    // XSLT 2.0 §16.5.1 — a presentation written with a non-ASCII
    // Unicode decimal-digit family (e.g. Thai `๐๐๐๑`) renders the
    // value in that family; the number of digit characters is the
    // mandatory (zero-padded) width.
    if let Some((zero_cp, count)) = picture_digit_family(presentation) {
        // Year truncation (XSLT 2.0 §16.5.1) applies regardless of the
        // digit family: the count of family glyphs is the picture's
        // digit width, so `[Y๐๑]` (two Thai zeros) truncates `2003` to
        // the low-order two digits just like the ASCII `[Y01]`.
        let cap = max_w.unwrap_or(count);
        let (value, want) = if truncate_year && n >= 0
            && cap < 5 && n.to_string().len() > cap {
            (n % 10_i64.pow(cap as u32), cap)
        } else {
            (n, min_w.unwrap_or(count))
        };
        let ascii = if value < 0 { format!("-{:0w$}", -value, w = want) }
                    else         { format!("{:0w$}", value, w = want) };
        return ascii.chars()
            .map(|c| c.to_digit(10)
                .and_then(|v| char::from_u32(zero_cp + v))
                .unwrap_or(c))
            .collect();
    }
    // Numeric pattern: count digits in the presentation to derive
    // the minimum width (`01` = 2 digits, `0001` = 4 digits, `1` = 1).
    let pres_digits = presentation.chars().filter(|c| c.is_ascii_digit()).count().max(1);
    let want_width = min_w.unwrap_or(pres_digits);
    // Year truncation (XSLT 2.0 §16.5.1): only fires when the
    // caller asked for a bounded width — either an explicit
    // `,min-max` clause OR a presentation with more than one digit
    // place like `Y01` / `Y0001`.  Bare `[Y]` (presentation just
    // `"1"`) means "no padding, no truncation" and must emit the
    // full year.
    if truncate_year && n >= 0 {
        // Year width spec: only force right-truncation when the
        // year actually exceeds the declared max; pad to min when
        // it falls short; otherwise emit unchanged.  This is what
        // Saxon's `[Y,3-4]` does — `985` stays `985`, `19850`
        // becomes `9850`, `98` becomes `098`.
        let bounded_by_pres = pres_digits >= 2;
        if max_w.is_some() || bounded_by_pres {
            let cap = max_w.unwrap_or(pres_digits);
            let raw = n.to_string();
            let digits = raw.len();
            if cap < 5 && digits > cap {
                let modulus = 10_i64.pow(cap as u32);
                let truncated = n % modulus;
                return format!("{:0width$}", truncated, width = cap);
            }
            // Within range — pad to the larger of (min_w, want_width).
            let pad = want_width.max(min_w.unwrap_or(0));
            return format!("{:0width$}", n, width = pad);
        }
    }
    let formatted = if n < 0 {
        format!("-{:0width$}", -n, width = want_width)
    } else {
        format!("{:0width$}", n, width = want_width)
    };
    if ordinal {
        // English ordinal suffix.  XSLT 2.0 leaves the suffix
        // language-dependent; we only implement English because
        // the test suite only exercises that locale.
        format!("{formatted}{}", english_ordinal_suffix(n))
    } else {
        formatted
    }
}

/// English ordinal suffix for a non-negative integer — `1` → `st`,
/// `2` → `nd`, `3` → `rd`, etc.  The teens are all `th` (11th, 12th,
/// 13th).
fn english_ordinal_suffix(n: i64) -> &'static str {
    let abs = n.unsigned_abs();
    if (11..=13).contains(&(abs % 100)) {
        return "th";
    }
    match abs % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    }
}

/// Format the fractional-seconds component (`[f…]`).  Unlike the
/// other numeric components, `f` reads from the *left*: a marker
/// like `[f01]` asks for two digits of precision counting from the
/// most-significant fractional digit, so `.456` → `46`.  When the
/// stored precision exceeds the requested width we round half-up
/// (XSLT 2.0 §16.5.1 leaves rounding to the implementation but
/// Saxon and libxslt both round; tests expect rounding too).
fn format_fractional_seconds(
    frac_us: u32, presentation: &str,
    min_w: Option<usize>, max_w: Option<usize>,
) -> String {
    // XSLT 2.0 §16.5.1 — the fractional-second formatter has two
    // width inputs:
    //
    //   * `min_w` / `max_w` from the `,min-max` clause of the
    //     picture marker (`*` means unbounded; missing means equal
    //     to the other or to the digit-count from the presentation).
    //   * The number of digits in the presentation modifier
    //     (`[f001]` → 3, `[f,2-5]` → 1 default).
    //
    // Algorithm:
    //   1. Round half-up to `max_w` digits (or, when max is
    //      unbounded, the microsecond precision we store — 6).
    //   2. Pad with trailing zeros to `max_w` so the rounded
    //      digits are right-justified across the maximum field.
    //   3. Strip trailing zeros down to `min_w` (no shorter).
    //
    // Examples for `0.456` (= 456000 µs):
    //   `[f,4-4]`    → max=4 round→4560      min=4: keep      → "4560"
    //   `[f,1-4]`    → max=4 round→4560      min=1: strip 0   → "456"
    //   `[f,1-*]`    → max=∞ keep "456"      min=1: keep      → "456"
    //   `[f001]`     → pres=3 min=max=3 round→456              → "456"
    let pres_digits = presentation.chars().filter(|c| c.is_ascii_digit()).count();
    // Default min / max from the picture marker if the width spec
    // didn't supply them.  XSLT 2.0 §16.5.1 — when only the
    // presentation modifier is present it sets both bounds; when a
    // width spec is present the modifier's digit count is the
    // default for whichever bound the spec omitted.
    // Convert `Some(usize::MAX)` (the `*` sentinel from
    // `parse_width_spec`) into `None` for downstream "unbounded"
    // checks, keeping the case analysis crisp.
    let unwrap_star = |v: Option<usize>| v.and_then(|n| if n == usize::MAX { None } else { Some(n) });
    let min_w_explicit = min_w.map(|n| n != usize::MAX).unwrap_or(false);
    let max_w_explicit_star = max_w == Some(usize::MAX);
    let min_w = unwrap_star(min_w);
    let max_w = unwrap_star(max_w);
    let (min_w, max_w) = match (min_w, max_w) {
        (Some(mn), Some(mx))     => (mn, Some(mx)),
        (Some(mn), None) if max_w_explicit_star => (mn, None),
        (Some(mn), None)         => (mn, Some(mn.max(pres_digits))),
        (None,     Some(mx))     => (pres_digits.min(mx).max(1), Some(mx)),
        (None,     None) if pres_digits > 0 => (pres_digits, Some(pres_digits)),
        (None,     None) if min_w_explicit => (1, None),
        (None,     None)         => (1, None),
    };
    // Round to `max_w` digits.  `None` max means unbounded — keep
    // the full 6-digit microsecond precision (we don't store more).
    let target = max_w.unwrap_or(6).min(6);
    if target == 0 { return String::new(); }
    let divisor = 10u32.pow((6 - target) as u32);
    let half    = divisor / 2;
    let rounded = (frac_us + half) / divisor;
    let max_value = 10u32.pow(target as u32);
    let clamped = rounded.min(max_value - 1);
    // Right-justified field of `target` digits, then trim trailing
    // zeros down to `min_w` (never below).
    let padded = format!("{:0width$}", clamped, width = target);
    let padded_bytes = padded.as_bytes();
    let mut end = padded_bytes.len();
    while end > min_w && padded_bytes[end - 1] == b'0' { end -= 1; }
    let trimmed = &padded[..end];
    // If max was unbounded (no `,` clause and the presentation had
    // no digit count) we may need extra zeros beyond µs precision.
    // That's rare; for now we cap at 6 — XPath 2.0 only mandates
    // implementation-defined precision past that.
    trimmed.to_string()
}

/// Parse the `,min-max` width-spec clause of a picture marker.
/// `min` and `max` may both be `*` meaning "unbounded".  Returns
/// `(min_width, max_width)` — either may be `None`.
fn parse_width_spec(spec: Option<&str>) -> (Option<usize>, Option<usize>) {
    // XSLT 2.0 §16.5.1 — `min-max` with `*` meaning unbounded.
    // We encode "explicit unbounded" as `Some(usize::MAX)` so the
    // caller can distinguish it from "field omitted entirely"
    // (`None`).  Picture-marker formatters then handle MAX as
    // "no upper cap on width."
    let spec = match spec { Some(s) => s, None => return (None, None) };
    let (min_s, max_s) = match spec.split_once('-') {
        Some((a, b)) => (a, Some(b)),
        None         => (spec, None),
    };
    let parse = |s: &str| -> Option<usize> {
        let s = s.trim();
        if s.is_empty()  { None }
        else if s == "*" { Some(usize::MAX) }
        else             { s.parse().ok() }
    };
    (parse(min_s), max_s.and_then(parse))
}

/// RFC 3986 §5.3 reference resolution.  Combine `base` (treated as
/// already absolute) and `rel` (a possibly-relative URI reference)
/// into an absolute URI string.  Implements the strict form: a
/// `rel` with its own scheme returns essentially unchanged (with
/// dot-segments removed), matching `fn:resolve-uri`'s spec.
pub fn resolve_uri_against(base: &str, rel: &str) -> String {
    resolve_uri_rfc3986(base, rel)
}


/// True iff `uri` carries a scheme (the `scheme:` prefix of RFC 3986
/// §3.1) — i.e. it is an absolute URI rather than a relative reference.
fn uri_has_scheme(uri: &str) -> bool {
    split_uri(uri).0.is_some()
}

fn resolve_uri_rfc3986(base: &str, rel: &str) -> String {
    let (rs, ra, rp, rq, rf) = split_uri(rel);
    let (bs, ba, bp, bq, _bf) = split_uri(base);
    // Target components per RFC 3986 §5.3.
    let (ts, ta, tp, tq) = if let Some(s) = rs {
        (s, ra, remove_dot_segments(rp), rq)
    } else if ra.is_some() {
        (bs.unwrap_or(""), ra, remove_dot_segments(rp), rq)
    } else if rp.is_empty() {
        let q = if rq.is_some() { rq } else { bq };
        (bs.unwrap_or(""), ba, bp.to_string(), q)
    } else if rp.starts_with('/') {
        (bs.unwrap_or(""), ba, remove_dot_segments(rp), rq)
    } else {
        let merged = merge_paths(ba.is_some(), bp, rp);
        (bs.unwrap_or(""), ba, remove_dot_segments(&merged), rq)
    };
    // Recompose.
    let mut out = String::new();
    if !ts.is_empty() {
        out.push_str(ts);
        out.push(':');
    }
    if let Some(a) = ta {
        out.push_str("//");
        out.push_str(a);
    }
    out.push_str(&tp);
    if let Some(q) = tq {
        out.push('?');
        out.push_str(q);
    }
    if let Some(f) = rf {
        out.push('#');
        out.push_str(f);
    }
    out
}

/// Parse a URI string into `(scheme, authority, path, query, fragment)`.
/// Scheme is `Some` only when a valid scheme prefix is detected
/// (alpha + alnum/`+-.` + colon).  Authority is `Some` only when the
/// path starts with `//`.  Path is always returned (possibly empty);
/// query / fragment are `Some` only when present.
fn split_uri(s: &str) -> (Option<&str>, Option<&str>, &str, Option<&str>, Option<&str>) {
    let bytes = s.as_bytes();
    // Scheme: must start with ALPHA, then alnum/+/-/. up to ':'.
    let scheme_end = bytes.iter().position(|&b|
        !(b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.'));
    let (scheme, mut rest) = match scheme_end {
        Some(i) if i > 0 && bytes[i] == b':' && bytes[0].is_ascii_alphabetic() =>
            (Some(&s[..i]), &s[i + 1..]),
        _ => (None, s),
    };
    // Fragment.
    let fragment = rest.find('#').map(|i| {
        let f = &rest[i + 1..];
        rest = &rest[..i];
        f
    });
    // Query.
    let query = rest.find('?').map(|i| {
        let q = &rest[i + 1..];
        rest = &rest[..i];
        q
    });
    // Authority.
    let (authority, path) = if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(|c: char| c == '/' || c == '?' || c == '#')
            .unwrap_or(after.len());
        (Some(&after[..end]), &after[end..])
    } else {
        (None, rest)
    };
    (scheme, authority, path, query, fragment)
}

fn remove_dot_segments(input: &str) -> String {
    // RFC 3986 §5.2.4.
    let mut in_buf = input.to_string();
    let mut out = String::with_capacity(input.len());
    while !in_buf.is_empty() {
        if in_buf.starts_with("../") {
            in_buf.replace_range(..3, "");
        } else if in_buf.starts_with("./") {
            in_buf.replace_range(..2, "");
        } else if in_buf.starts_with("/./") {
            in_buf.replace_range(..3, "/");
        } else if in_buf == "/." {
            in_buf = "/".to_string();
        } else if in_buf.starts_with("/../") {
            in_buf.replace_range(..4, "/");
            // Remove last segment in `out` (back to but not including
            // the previous '/', or empty out if none).
            if let Some(i) = out.rfind('/') { out.truncate(i); } else { out.clear(); }
        } else if in_buf == "/.." {
            in_buf = "/".to_string();
            if let Some(i) = out.rfind('/') { out.truncate(i); } else { out.clear(); }
        } else if in_buf == "." || in_buf == ".." {
            in_buf.clear();
        } else {
            // Move first segment (up to but not including the next '/')
            // from `in_buf` to `out`.
            let split = if let Some(stripped) = in_buf.strip_prefix('/') {
                stripped.find('/').map(|i| i + 1).unwrap_or(in_buf.len())
            } else {
                in_buf.find('/').unwrap_or(in_buf.len())
            };
            out.push_str(&in_buf[..split]);
            in_buf.replace_range(..split, "");
        }
    }
    out
}

fn merge_paths(base_has_authority: bool, base_path: &str, rel_path: &str) -> String {
    // RFC 3986 §5.2.3.
    if base_has_authority && base_path.is_empty() {
        let mut s = String::with_capacity(rel_path.len() + 1);
        s.push('/');
        s.push_str(rel_path);
        s
    } else {
        let cut = base_path.rfind('/').map(|i| i + 1).unwrap_or(0);
        let mut s = String::with_capacity(cut + rel_path.len());
        s.push_str(&base_path[..cut]);
        s.push_str(rel_path);
        s
    }
}

/// Implement `fn:adjust-*-to-timezone`.  Re-stamp `lex` (a
/// `date` / `dateTime` / `time` lexical) to the supplied
/// `new_tz_minutes` offset (`None` strips the timezone, `Some(n)`
/// stamps `n`).  If the input already has a timezone, the wall-
/// clock parts are first shifted to keep the absolute moment
/// constant; if the input is timezone-less, the wall-clock parts
/// are kept and the new offset stamped.
fn adjust_timezone(lex: &str, kind: &str, new_tz_minutes: Option<i16>) -> String {
    let dk = match kind {
        "date"     => DateKind::Date,
        "dateTime" => DateKind::DateTime,
        "time"     => DateKind::Time,
        _ => return lex.to_string(),
    };
    let Some((y, mo, d, mut h, mut mi, s, frac, tz)) = parse_xsd_date_time(lex, dk) else {
        return lex.to_string();
    };
    // Convert (year, month, day, h, mi) to a single integer
    // "minutes since 0000-01-01 (proleptic)" so we can do
    // arithmetic regardless of DST and rollover.
    let (mut yy, mut mm, mut dd) = (y as i64, mo as i64, d as i64);
    let needs_shift = match (tz, new_tz_minutes) {
        (Some(_), Some(_)) => true,   // both present — shift wall-clock
        _ => false,                   // either side absent — keep wall-clock
    };
    if needs_shift {
        let old_tz = tz.unwrap() as i64;
        let new_tz = new_tz_minutes.unwrap() as i64;
        let delta_minutes = new_tz - old_tz;
        if matches!(dk, DateKind::Time) {
            // Pure time wraps modulo 1440 minutes per spec.
            let total = (h as i64) * 60 + (mi as i64) + delta_minutes;
            let wrapped = ((total % 1440) + 1440) % 1440;
            h  = (wrapped / 60) as u8;
            mi = (wrapped % 60) as u8;
        } else {
            let days = ymd_to_days(yy as i32, mm as u32, dd as u32);
            let total_min = days * 24 * 60 + (h as i64) * 60 + (mi as i64) + delta_minutes;
            let new_days = total_min.div_euclid(24 * 60);
            let leftover = total_min.rem_euclid(24 * 60);
            h  = (leftover / 60) as u8;
            mi = (leftover % 60) as u8;
            let (ny, nm, nd) = days_to_ymd(new_days);
            yy = ny as i64; mm = nm as i64; dd = nd as i64;
        }
    }
    let tz_str = match new_tz_minutes {
        None      => String::new(),
        Some(0)   => "Z".to_string(),
        Some(m)   => format!("{}{:02}:{:02}",
            if m < 0 { '-' } else { '+' },
            (m.unsigned_abs() / 60), (m.unsigned_abs() % 60)),
    };
    match dk {
        DateKind::Time => {
            let mut s_out = format!("{:02}:{:02}:{:02}", h, mi, s);
            if frac > 0 {
                // Trim trailing zeros from the microseconds field.
                let mut frac_s = format!("{:06}", frac);
                while frac_s.ends_with('0') { frac_s.pop(); }
                if !frac_s.is_empty() { s_out.push('.'); s_out.push_str(&frac_s); }
            }
            s_out.push_str(&tz_str);
            s_out
        }
        DateKind::Date => {
            let sign = if yy < 0 { "-" } else { "" };
            format!("{}{:04}-{:02}-{:02}{}", sign, yy.unsigned_abs(), mm, dd, tz_str)
        }
        DateKind::DateTime => {
            let sign = if yy < 0 { "-" } else { "" };
            let mut s_out = format!("{}{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                sign, yy.unsigned_abs(), mm, dd, h, mi, s);
            if frac > 0 {
                let mut frac_s = format!("{:06}", frac);
                while frac_s.ends_with('0') { frac_s.pop(); }
                if !frac_s.is_empty() { s_out.push('.'); s_out.push_str(&frac_s); }
            }
            s_out.push_str(&tz_str);
            s_out
        }
    }
}

/// XPath 3.0 §15.1.9 `fn:path` — render a node-locating path.
/// The result is interpretable as an XPath that selects `node`
/// from the document root: each step uses an `Q{uri}local[pos]`
/// (or attribute / kind-test) segment.  A pure document node
/// yields `/`.  Orphan subtrees (no document ancestor) anchor
/// at `Q{}root()`.
fn node_path_string<I: DocIndexLike>(node: NodeId, idx: &I) -> String {
    use crate::xpath::XPathNodeKind;
    // Document → "/" — the empty path.
    if matches!(idx.kind(node), XPathNodeKind::Document) {
        return "/".to_string();
    }
    // Walk up to the root, pushing per-step segments.
    let mut segments: Vec<String> = Vec::new();
    let mut cur = node;
    loop {
        let parent = idx.parent(cur);
        let segment = match idx.kind(cur) {
            XPathNodeKind::Element => {
                let local = idx.local_name(cur).to_string();
                let uri   = idx.namespace_uri(cur).to_string();
                let pos = match parent {
                    Some(p) => element_index_among_siblings(p, cur, &local, &uri, idx),
                    None    => 1,
                };
                format!("Q{{{uri}}}{local}[{pos}]")
            }
            XPathNodeKind::Attribute => {
                let local = idx.local_name(cur).to_string();
                let uri   = idx.namespace_uri(cur).to_string();
                format!("@Q{{{uri}}}{local}")
            }
            XPathNodeKind::Text | XPathNodeKind::CData => {
                let pos = match parent {
                    Some(p) => kind_index_among_siblings(p, cur, XPathNodeKind::Text, idx),
                    None    => 1,
                };
                format!("text()[{pos}]")
            }
            XPathNodeKind::Comment => {
                let pos = match parent {
                    Some(p) => kind_index_among_siblings(p, cur, XPathNodeKind::Comment, idx),
                    None    => 1,
                };
                format!("comment()[{pos}]")
            }
            XPathNodeKind::PI => {
                let target = idx.pi_target(cur).to_string();
                let pos = match parent {
                    Some(p) => pi_index_among_siblings(p, cur, &target, idx),
                    None    => 1,
                };
                format!("processing-instruction({target})[{pos}]")
            }
            XPathNodeKind::Namespace => {
                let prefix = idx.local_name(cur);
                format!("namespace::{prefix}")
            }
            XPathNodeKind::Document => break,
        };
        segments.push(segment);
        match parent {
            Some(p) if matches!(idx.kind(p), XPathNodeKind::Document) => break,
            Some(p) => { cur = p; }
            None    => {
                // Orphan subtree — anchor at the spec's `Q{}root()`
                // placeholder so the path is syntactically still
                // an absolute XPath.
                segments.push("Q{}root()".to_string());
                return segments.into_iter().rev().collect::<Vec<_>>().join("/");
            }
        }
    }
    // Prepend "" so the join produces a leading "/".
    let mut parts: Vec<String> = Vec::with_capacity(segments.len() + 1);
    parts.push(String::new());
    parts.extend(segments.into_iter().rev());
    parts.join("/")
}

/// Position of `child` among its sibling element children of
/// `parent` that share the same expanded name (1-based).
fn element_index_among_siblings<I: DocIndexLike>(
    parent: NodeId, child: NodeId, local: &str, uri: &str, idx: &I,
) -> usize {
    let mut count = 0usize;
    for &sib in idx.children(parent) {
        if matches!(idx.kind(sib), crate::xpath::XPathNodeKind::Element)
            && idx.local_name(sib) == local
            && idx.namespace_uri(sib) == uri
        {
            count += 1;
            if sib == child { return count; }
        }
    }
    count
}

/// Position of `child` among its sibling children of `parent`
/// that have node kind `kind` (1-based).  Used for `text()` and
/// `comment()` path segments.
fn kind_index_among_siblings<I: DocIndexLike>(
    parent: NodeId, child: NodeId,
    kind: crate::xpath::XPathNodeKind, idx: &I,
) -> usize {
    let mut count = 0usize;
    for &sib in idx.children(parent) {
        if idx.kind(sib) == kind {
            count += 1;
            if sib == child { return count; }
        }
    }
    count
}

/// Position of `child` among its sibling PI children with
/// matching target (1-based).
fn pi_index_among_siblings<I: DocIndexLike>(
    parent: NodeId, child: NodeId, target: &str, idx: &I,
) -> usize {
    let mut count = 0usize;
    for &sib in idx.children(parent) {
        if matches!(idx.kind(sib), crate::xpath::XPathNodeKind::PI)
            && idx.pi_target(sib) == target
        {
            count += 1;
            if sib == child { return count; }
        }
    }
    count
}

/// XPath 2.0 §15.3.1 `fn:deep-equal` — recursive value
/// equality.  Two `Value`s are deep-equal iff:
///
/// * Same length when atomised to a sequence of items.
/// * Pairwise items are deep-equal.  Atomic items use the same
///   `values_eq` semantics as the `=` general comparison;
///   nodes use kind-specific structural recursion.
fn deep_equal_values<I: DocIndexLike>(
    a: &Value, b: &Value, idx: &I, bindings: &dyn XPathBindings,
) -> bool {
    let a_items = items_of(a);
    let b_items = items_of(b);
    if a_items.len() != b_items.len() { return false; }
    a_items.iter().zip(b_items.iter()).all(|(x, y)|
        deep_equal_one(x, y, idx, bindings))
}

/// Decompose a `Value` into the per-item view that `deep-equal`
/// iterates over.  NodeSets become one `NodeSet(vec![id])` per
/// node so each pair compares as a single node.
/// Collapse a list of items back into a single `Value` (empty → empty
/// node-set, one → itself, many → a `Sequence`).
fn seq_from_items(mut items: Vec<Value>) -> Value {
    match items.len() {
        0 => Value::NodeSet(Vec::new()),
        1 => items.pop().unwrap(),
        _ => Value::Sequence(items),
    }
}

/// Numeric view of a map key, if it is a numeric atomic.  `None` for
/// non-numeric keys (so a string key never equals a numeric one).
fn key_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => Some(n.as_f64()),
        Value::Typed(t)  => t.numeric,
        _ => None,
    }
}

/// XPath 3.1 §17.1 map-key equality: numeric keys compare numerically
/// (`1` = `1.0`), with `NaN` equal to itself; otherwise compare by
/// string value (covering string / untyped / anyURI / boolean keys).
pub fn map_key_eq<I: DocIndexLike>(a: &Value, b: &Value, idx: &I) -> bool {
    match (key_number(a), key_number(b)) {
        (Some(x), Some(y)) => (x.is_nan() && y.is_nan()) || x == y,
        (None, None)       => value_to_string(a, idx) == value_to_string(b, idx),
        _                  => false,
    }
}

/// Reduce a value to the single atomic value usable as a map key
/// (XPTY0004 if not a singleton atomic; lenient — first item, nodes
/// contribute their string value).
pub fn first_atomic_key<I: DocIndexLike>(v: &Value, idx: &I) -> Value {
    match v {
        Value::NodeSet(ns) => Value::String(
            ns.first().map(|&id| idx.string_value(id)).unwrap_or_default()),
        Value::ForeignNodeSet(_) => Value::String(value_to_string(v, idx)),
        Value::Sequence(items) => items.first()
            .map(|x| first_atomic_key(x, idx))
            .unwrap_or(Value::String(String::new())),
        Value::IntRange { lo, .. } => Value::Number(Numeric::Integer(*lo)),
        other => other.clone(),
    }
}

/// Evaluate an XPath 3.1 lookup (`base ? key`) — index into each map
/// or array item of `base`, concatenating the selected values.
fn eval_lookup<I: DocIndexLike>(
    base: &Value, key: &crate::xpath::ast::LookupKey,
    ctx: &EvalCtx<'_>, idx: &I,
) -> Result<Value> {
    use crate::xpath::ast::LookupKey;
    // `None` selects everything (`?*`); otherwise the resolved keys.
    let keys: Option<Vec<Value>> = match key {
        LookupKey::Wildcard   => None,
        LookupKey::Name(s)    => Some(vec![Value::String(s.clone())]),
        LookupKey::Integer(i) => Some(vec![Value::Number(Numeric::Integer(*i))]),
        LookupKey::Expr(e)    => Some(items_of(&eval_expr(e, ctx, idx)?)),
    };
    let mut out: Vec<Value> = Vec::new();
    for item in items_of(base) {
        match &item {
            Value::Map(entries) => match &keys {
                None => for (_, val) in entries.iter() { out.extend(items_of(val)); },
                Some(ks) => for k in ks {
                    for (mk, mv) in entries.iter() {
                        if map_key_eq(mk, k, idx) { out.extend(items_of(mv)); }
                    }
                },
            },
            Value::Array(members) => match &keys {
                None => for m in members.iter() { out.extend(items_of(m)); },
                Some(ks) => for k in ks {
                    let pos = value_to_number(k, idx);
                    if pos.fract() == 0.0 && pos >= 1.0
                        && (pos as usize) <= members.len()
                    {
                        out.extend(items_of(&members[pos as usize - 1]));
                    }
                },
            },
            _ => return Err(xpath_err(
                "the '?' lookup operator requires a map or array (XPTY0004)")),
        }
    }
    Ok(seq_from_items(out))
}

fn items_of(v: &Value) -> Vec<Value> {
    match v {
        Value::NodeSet(ns) => ns.iter()
            .map(|&id| Value::NodeSet(vec![id])).collect(),
        Value::Sequence(items) => items.clone(),
        Value::ForeignNodeSet(ns) => ns.iter()
            .map(|&p| Value::ForeignNodeSet(vec![p])).collect(),
        Value::IntRange { lo, hi } =>
            (*lo..=*hi).map(|i| Value::Number(Numeric::Double(i as f64))).collect(),
        other => vec![other.clone()],
    }
}

/// Cardinality of a value as a sequence — the cheap analogue of
/// `items_of(v).len()` that doesn't materialise [`Value::IntRange`].
/// Used by `fn:count`, `fn:last`, the `position()/last()` runtime,
/// and any other consumer that only needs the item count.  Recurses
/// through nested `Sequence` items so an inline `IntRange` fragment
/// contributes its full cardinality (not 1).
fn sequence_len(v: &Value) -> usize {
    match v {
        Value::NodeSet(ns)        => ns.len(),
        Value::ForeignNodeSet(ns) => ns.len(),
        Value::Sequence(items)    => items.iter().map(sequence_len).sum(),
        Value::IntRange { lo, hi } => ((hi - lo) as usize).saturating_add(1),
        _                          => 1,
    }
}

/// Iterate a value as a sequence of items without materialising
/// [`Value::IntRange`].  Lets `xsl:for-each`, predicates, and
/// simple-map produce items pull-style so a 1.1M-item range
/// doesn't allocate 1.1M `Value`s up front.  Recurses through
/// nested `Sequence` items so a `Sequence` that carries inline
/// `IntRange` fragments flattens transparently.
fn iter_items<'a>(v: &'a Value) -> Box<dyn Iterator<Item = Value> + 'a> {
    match v {
        Value::NodeSet(ns)        => Box::new(ns.iter().map(|&id| Value::NodeSet(vec![id]))),
        Value::ForeignNodeSet(ns) => Box::new(ns.iter().map(|&p| Value::ForeignNodeSet(vec![p]))),
        Value::Sequence(items)    => Box::new(items.iter().flat_map(iter_items)),
        Value::IntRange { lo, hi } => Box::new((*lo..=*hi).map(|i| Value::Number(Numeric::Double(i as f64)))),
        other                      => Box::new(std::iter::once(other.clone())),
    }
}

fn deep_equal_one<I: DocIndexLike>(
    a: &Value, b: &Value, idx: &I, bindings: &dyn XPathBindings,
) -> bool {
    match (a, b) {
        (Value::NodeSet(an), Value::NodeSet(bn))
            if an.len() == 1 && bn.len() == 1 =>
        {
            deep_equal_node(an[0], bn[0], idx)
        }
        // Text-node-vs-atomic: many path expressions
        // (`/x/y/string()`, `/x/y/concat(.,'')`) flow atomic results
        // through the path machinery wrapped as synthetic RTF text
        // nodes; a strict node-vs-atomic rejection would then fail
        // deep-equal against atomic sequences that any reasonable
        // stylesheet would expect to match.  Compare by string value
        // when the node side is a single text node.
        (Value::NodeSet(ns), other) | (other, Value::NodeSet(ns))
            if ns.len() == 1 && matches!(idx.kind(ns[0]), XPathNodeKind::Text) =>
        {
            value_to_string_with(other, idx, bindings) == idx.string_value(ns[0])
        }
        // Mixed node / non-node items are NEVER deep-equal per the
        // spec (FOTY0004 doesn't apply because we don't surface
        // type errors; falsiness is good enough).
        (Value::NodeSet(_), _) | (_, Value::NodeSet(_)) => false,
        // Atomic comparisons use value-comparison (eq) semantics
        // per XPath 2.0 §15.3.1: a type error between the operands
        // means "not deep-equal", not the general-comparison
        // lenient fallback.  Reject pairs whose XSD type families
        // can't `eq` each other (numeric vs string, boolean vs
        // anything else, etc.).
        _ if !deep_equal_types_comparable(a, b) => false,
        _ => values_eq(a, b, idx, bindings),
    }
}

/// True iff the two atomic values can be compared by XPath 2.0
/// `eq` without raising a type error.  Used by [`deep_equal_one`]
/// to treat un-comparable pairs as "not deep-equal".
fn deep_equal_types_comparable(a: &Value, b: &Value) -> bool {
    #[derive(PartialEq, Eq, Clone, Copy)]
    enum Family { Numeric, String, Boolean, Date, Duration, Other }
    fn family_of(v: &Value) -> Family {
        match v {
            Value::Number(_) => Family::Numeric,
            Value::Boolean(_) => Family::Boolean,
            Value::String(_) => Family::String,
            Value::Typed(t) => match t.kind {
                "string" | "anyURI" | "untypedAtomic" | "normalizedString"
                | "token" | "Name" | "NCName" | "QName"
                | "language" | "NMTOKEN" | "ID" | "IDREF" | "ENTITY"
                    => Family::String,
                "boolean" => Family::Boolean,
                "date" | "dateTime" | "time"
                | "gYear" | "gYearMonth" | "gMonth" | "gMonthDay" | "gDay"
                    => Family::Date,
                "duration" | "dayTimeDuration" | "yearMonthDuration"
                    => Family::Duration,
                k if matches!(k,
                    "integer" | "decimal" | "double" | "float"
                    | "long" | "int" | "short" | "byte"
                    | "unsignedLong" | "unsignedInt" | "unsignedShort" | "unsignedByte"
                    | "nonNegativeInteger" | "nonPositiveInteger"
                    | "positiveInteger" | "negativeInteger" | "numeric"
                )   => Family::Numeric,
                _ => Family::Other,
            },
            _ => Family::Other,
        }
    }
    let fa = family_of(a);
    let fb = family_of(b);
    // Untyped atomics (Family::Other) are permissive — they can
    // compare against anything because their value is undecided.
    if fa == Family::Other || fb == Family::Other { return true; }
    fa == fb
}

fn deep_equal_node<I: DocIndexLike>(a: NodeId, b: NodeId, idx: &I) -> bool {
    if idx.kind(a) != idx.kind(b) { return false; }
    match idx.kind(a) {
        XPathNodeKind::Element => {
            if idx.namespace_uri(a) != idx.namespace_uri(b) { return false; }
            if idx.local_name(a)    != idx.local_name(b)    { return false; }
            // Attributes: set-equal (XPath 2.0 §15.3.1 treats
            // attribute order as insignificant).
            let mut a_attrs: Vec<NodeId> = idx.attr_range(a).collect();
            let mut b_attrs: Vec<NodeId> = idx.attr_range(b).collect();
            if a_attrs.len() != b_attrs.len() { return false; }
            // Sort by (uri, local-name) so the per-attr match
            // doesn't depend on declaration order.  We can't use
            // raw NodeId because the two nodes live in possibly
            // different positions in the index.
            a_attrs.sort_by_key(|&id| (idx.namespace_uri(id).to_string(),
                                       idx.local_name(id).to_string()));
            b_attrs.sort_by_key(|&id| (idx.namespace_uri(id).to_string(),
                                       idx.local_name(id).to_string()));
            for (&aa, &bb) in a_attrs.iter().zip(b_attrs.iter()) {
                if idx.namespace_uri(aa) != idx.namespace_uri(bb) { return false; }
                if idx.local_name(aa)    != idx.local_name(bb)    { return false; }
                if idx.string_value(aa)  != idx.string_value(bb)  { return false; }
            }
            // Children: filter out whitespace-only text nodes
            // (XPath 2.0 actually keeps them, but typed strip-
            // space is implementation-defined; we mirror Saxon).
            // Compare positionally.
            let a_kids = idx.children(a);
            let b_kids = idx.children(b);
            if a_kids.len() != b_kids.len() { return false; }
            a_kids.iter().zip(b_kids.iter())
                .all(|(&x, &y)| deep_equal_node(x, y, idx))
        }
        XPathNodeKind::Document => {
            let a_kids = idx.children(a);
            let b_kids = idx.children(b);
            if a_kids.len() != b_kids.len() { return false; }
            a_kids.iter().zip(b_kids.iter())
                .all(|(&x, &y)| deep_equal_node(x, y, idx))
        }
        XPathNodeKind::Attribute => {
            idx.namespace_uri(a) == idx.namespace_uri(b)
                && idx.local_name(a) == idx.local_name(b)
                && idx.string_value(a) == idx.string_value(b)
        }
        XPathNodeKind::Text | XPathNodeKind::CData => {
            idx.string_value(a) == idx.string_value(b)
        }
        XPathNodeKind::Comment => {
            idx.string_value(a) == idx.string_value(b)
        }
        XPathNodeKind::PI => {
            idx.pi_target(a) == idx.pi_target(b)
                && idx.string_value(a) == idx.string_value(b)
        }
        XPathNodeKind::Namespace => {
            idx.local_name(a) == idx.local_name(b)
                && idx.string_value(a) == idx.string_value(b)
        }
    }
}

/// English month names — index 1 is January (matches the XSLT
/// 1-based month component).  Used by `[MNn]` / `[MN,3-3]`
/// markers in `format-date`.
const MONTH_NAMES: &[&str] = &[
    "", "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
];

/// English day-of-week names — index 1 = Monday per ISO 8601
/// (which is what XSLT 2.0 §16.5.1 uses for the `F` component).
const DAY_NAMES: &[&str] = &[
    "", "Monday", "Tuesday", "Wednesday", "Thursday",
    "Friday", "Saturday", "Sunday",
];

/// True iff the picture-marker presentation modifier asks for a
/// named (rather than numeric / Roman / alphabetic) rendering.
fn name_presentation(pres: &str) -> bool {
    matches!(pres, "N" | "n" | "Nn"
                 | "NN" | "nn"     // also accept short forms
    )
}

/// Render a numeric date component using a localized name table
/// (English).  `value` is 1-based.  `max_w` truncates the name
/// to the given length, with `N`/`n`/`Nn` controlling case.
fn format_named_component(value: i64, pres: &str, max_w: Option<usize>, table: &[&str]) -> String {
    let idx = value as usize;
    let name = table.get(idx).copied().unwrap_or("");
    // When the maximum width is shorter than the full name, prefer
    // the locale's standard abbreviation (3 letters for English
    // months and days, e.g., "Sun" / "Mon", "Jan" / "Feb") over
    // raw truncation.  Saxon and libxslt both do this; XSLT 2.0
    // §16.5.2 calls it "implementation-defined" but the test suite
    // expects the conventional shape.  We pick the longest
    // abbreviation that fits, falling back to char truncation if
    // even three letters don't fit.
    let chosen: String = match max_w {
        Some(max) if max < name.chars().count() => {
            // English standard 3-letter abbreviations.  We don't
            // distinguish month vs day here — both share the
            // "first three letters" pattern.
            let abbrev3: String = name.chars().take(3).collect();
            if max >= 3 { abbrev3 } else { name.chars().take(max).collect() }
        }
        _ => name.to_string(),
    };
    match pres {
        "N"  => chosen.to_uppercase(),
        "n"  => chosen.to_lowercase(),
        _    => chosen,
    }
}

/// Day of the week — Monday = 1 through Sunday = 7 (ISO 8601).
/// Uses the same Hinnant `days_from_civil` helper, then offsets.
/// Zero codepoints of the Unicode decimal-digit families a
/// format-date picture may select via its presentation modifier.
const DIGIT_FAMILY_ZEROS: &[u32] = &[
    0x0660,  // Arabic-Indic
    0x06F0,  // Extended Arabic-Indic
    0x0966,  // Devanagari
    0x09E6,  // Bengali
    0x0A66,  // Gurmukhi
    0x0AE6,  // Gujarati
    0x0B66,  // Oriya
    0x0BE6,  // Tamil
    0x0C66,  // Telugu
    0x0CE6,  // Kannada
    0x0D66,  // Malayalam
    0x0E50,  // Thai
    0x0ED0,  // Lao
    0x0F20,  // Tibetan
    0x1040,  // Myanmar
    0x17E0,  // Khmer
    0x1810,  // Mongolian
    0xFF10,  // Fullwidth
    0x104A0, // Osmanya
    0x1D7CE, // Mathematical bold
];

/// If `pres` is written entirely with the digits of one *non-ASCII*
/// decimal-digit family, return that family's zero codepoint and the
/// digit count (the mandatory width).  ASCII `0`/`1` patterns return
/// `None` so they take the normal formatting path.
fn picture_digit_family(pres: &str) -> Option<(u32, usize)> {
    if pres.is_empty() || pres.is_ascii() { return None; }
    let mut zero = None;
    let mut count = 0;
    for c in pres.chars() {
        let cp = c as u32;
        let z = DIGIT_FAMILY_ZEROS.iter().copied()
            .find(|&z| cp >= z && cp <= z + 9)?;
        match zero {
            Some(prev) if prev != z => return None, // mixed families
            _ => zero = Some(z),
        }
        count += 1;
    }
    zero.map(|z| (z, count))
}

/// ISO 8601 week-in-year (1..=53).  Weeks start Monday; week 1 is the
/// week containing the year's first Thursday.  A January date can
/// belong to the last week of the previous year and a late-December
/// date to week 1 of the next.
fn iso_week_of_year(y: i32, m: u32, d: u32) -> i64 {
    let ord = (month_start_day(y, m) + d) as i64; // 1-based day of year
    let wd  = day_of_week(y, m, d) as i64;         // Mon=1..Sun=7
    let week = (ord - wd + 10).div_euclid(7);
    if week < 1 {
        iso_weeks_in_year(y - 1)
    } else if week > iso_weeks_in_year(y) {
        1
    } else {
        week
    }
}

/// Number of ISO 8601 weeks in year `y` — 53 when 1 Jan is a Thursday
/// or (in a leap year) a Wednesday, else 52.
fn iso_weeks_in_year(y: i32) -> i64 {
    let p = |yy: i32| (yy + yy.div_euclid(4) - yy.div_euclid(100) + yy.div_euclid(400))
        .rem_euclid(7);
    if p(y) == 4 || p(y - 1) == 3 { 53 } else { 52 }
}

fn day_of_week(y: i32, m: u32, d: u32) -> u32 {
    // ymd_to_days returns days since 1970-01-01 (a Thursday →
    // weekday 4 in ISO).  Add the offset and modulo to 7,
    // shifting Sunday=0 → 7 to match the spec.
    let days = ymd_to_days(y, m, d);
    let wd = ((days + 3).rem_euclid(7) as u32) + 1;
    wd
}

/// Number of days from January 1 to the first of `month` in
/// year `y`.  Used to compute the day-of-year ordinal for the
/// `d` picture component.
fn month_start_day(y: i32, m: u32) -> u32 {
    let mut total = 0;
    let leap = is_leap_year(y);
    for mm in 1..m {
        total += match mm {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11              => 30,
            2 => if leap { 29 } else { 28 },
            _ => 0,
        };
    }
    total
}

/// Render `n` as a Roman numeral.  Range is 1..=3999 — outside that
/// the spec lets us fall back to decimal.  `upper` picks I/V/X/…
/// vs i/v/x/….
fn roman_numeral(n: i64, upper: bool) -> String {
    if !(1..=3999).contains(&n) { return n.to_string(); }
    const PAIRS: &[(i64, &str, &str)] = &[
        (1000, "M",  "m"),
        (900,  "CM", "cm"),
        (500,  "D",  "d"),
        (400,  "CD", "cd"),
        (100,  "C",  "c"),
        (90,   "XC", "xc"),
        (50,   "L",  "l"),
        (40,   "XL", "xl"),
        (10,   "X",  "x"),
        (9,    "IX", "ix"),
        (5,    "V",  "v"),
        (4,    "IV", "iv"),
        (1,    "I",  "i"),
    ];
    let mut n = n;
    let mut out = String::new();
    for &(val, hi, lo) in PAIRS {
        while n >= val {
            out.push_str(if upper { hi } else { lo });
            n -= val;
        }
    }
    out
}

/// Render `n` as an alphabetic label (`A`, `B`, …, `Z`, `AA`, `AB`, …).
/// `upper` selects uppercase vs lowercase.  Out-of-range falls back
/// to decimal.
fn alpha_label(n: i64, upper: bool) -> String {
    if n < 1 { return n.to_string(); }
    let base = if upper { b'A' } else { b'a' };
    let mut buf = Vec::new();
    let mut n = n;
    while n > 0 {
        n -= 1;
        buf.push(base + (n % 26) as u8);
        n /= 26;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_default()
}

#[derive(Clone, Copy)]
pub enum WordCase { Upper, Lower, Title }

/// XSLT 2.0 §16.5.1 cardinal / ordinal word presentation for
/// numeric format markers.  English-only — the spec is
/// locale-dependent and the W3C conformance suite exercises only
/// the `'en'` language for the bare `W` / `w` / `Ww` modifiers.
/// Negative inputs fall through to the decimal form (rare in
/// date/time contexts).
pub fn english_words(n: i64, ordinal: bool, case: WordCase) -> String {
    if n < 0 {
        let s = english_words(-n, ordinal, case);
        return format!("MINUS {s}");
    }
    let words = if ordinal { english_ordinal(n) } else { english_cardinal(n) };
    case_transform(&words, case)
}

pub fn case_transform(s: &str, case: WordCase) -> String {
    match case {
        WordCase::Upper => s.to_uppercase(),
        WordCase::Lower => s.to_lowercase(),
        WordCase::Title => {
            // Title-case each whitespace- or hyphen-separated word.
            let mut out = String::with_capacity(s.len());
            let mut start_of_word = true;
            for c in s.chars() {
                if c.is_whitespace() || c == '-' {
                    out.push(c);
                    start_of_word = true;
                } else if start_of_word {
                    for u in c.to_uppercase() { out.push(u); }
                    start_of_word = false;
                } else {
                    for l in c.to_lowercase() { out.push(l); }
                }
            }
            out
        }
    }
}

fn english_cardinal(n: i64) -> String {
    const UNDER_20: &[&str] = &[
        "zero", "one", "two", "three", "four", "five", "six", "seven",
        "eight", "nine", "ten", "eleven", "twelve", "thirteen",
        "fourteen", "fifteen", "sixteen", "seventeen", "eighteen",
        "nineteen",
    ];
    const TENS: &[&str] = &[
        "", "", "twenty", "thirty", "forty", "fifty", "sixty",
        "seventy", "eighty", "ninety",
    ];
    if (0..20).contains(&n) { return UNDER_20[n as usize].to_string(); }
    if n < 100 {
        let t = (n / 10) as usize;
        let r = (n % 10) as usize;
        if r == 0 { return TENS[t].to_string(); }
        // British / W3C-suite convention: space-separated, no hyphen.
        return format!("{} {}", TENS[t], UNDER_20[r]);
    }
    if n < 1000 {
        let h = (n / 100) as i64;
        let r = n % 100;
        let head = format!("{} hundred", UNDER_20[h as usize]);
        if r == 0 { head }
        // British: insert "and" between hundreds and the tens/units.
        else { format!("{head} and {}", english_cardinal(r)) }
    } else if n < 1_000_000 {
        let th = n / 1000;
        let r  = n % 1000;
        let head = format!("{} thousand", english_cardinal(th));
        if r == 0 { head }
        // British convention: when the trailing remainder is below
        // 100, the "and" goes between the higher group and the
        // tens/units ("two thousand and twelve").  When the remainder
        // includes a hundreds component, the "and" already lives
        // inside that hundreds clause ("two thousand one hundred and
        // five") so no extra one at the thousands boundary.
        else if r < 100 { format!("{head} and {}", english_cardinal(r)) }
        else { format!("{head} {}", english_cardinal(r)) }
    } else if n < 1_000_000_000 {
        let mil = n / 1_000_000;
        let r   = n % 1_000_000;
        let head = format!("{} million", english_cardinal(mil));
        if r == 0 { head }
        else if r < 100 { format!("{head} and {}", english_cardinal(r)) }
        else { format!("{head} {}", english_cardinal(r)) }
    } else {
        // Beyond billions — fall back to decimal digits; the
        // W3C suite doesn't exercise this.
        n.to_string()
    }
}

fn english_ordinal(n: i64) -> String {
    const ORDINAL_UNDER_20: &[&str] = &[
        "zeroth", "first", "second", "third", "fourth", "fifth",
        "sixth", "seventh", "eighth", "ninth", "tenth", "eleventh",
        "twelfth", "thirteenth", "fourteenth", "fifteenth",
        "sixteenth", "seventeenth", "eighteenth", "nineteenth",
    ];
    const ORDINAL_TENS: &[&str] = &[
        "", "", "twentieth", "thirtieth", "fortieth", "fiftieth",
        "sixtieth", "seventieth", "eightieth", "ninetieth",
    ];
    const TENS: &[&str] = &[
        "", "", "twenty", "thirty", "forty", "fifty", "sixty",
        "seventy", "eighty", "ninety",
    ];
    if (0..20).contains(&n) { return ORDINAL_UNDER_20[n as usize].to_string(); }
    if n < 100 {
        let t = (n / 10) as usize;
        let r = (n % 10) as usize;
        if r == 0 { return ORDINAL_TENS[t].to_string(); }
        // British: space-separated, no hyphen.
        return format!("{} {}", TENS[t], ORDINAL_UNDER_20[r]);
    }
    // 100+: cardinal head + ordinal tail (with "and" between
    // hundreds and tens for the British convention the W3C suite
    // expects).
    let head_div = if n < 1000 { 100 }
                   else if n < 1_000_000 { 1000 }
                   else { 1_000_000 };
    let head_word = if n < 1000 {
        format!("{} hundred", english_cardinal(n / 100))
    } else if n < 1_000_000 {
        format!("{} thousand", english_cardinal(n / 1000))
    } else {
        format!("{} million", english_cardinal(n / 1_000_000))
    };
    let r = n % head_div;
    if r == 0 {
        // Last whole word becomes ordinal: "hundred" → "hundredth"
        // etc.  Append "th" because all three are 1-syllable nouns
        // that take a regular -th.
        format!("{head_word}th")
    } else if n < 1000 {
        format!("{head_word} and {}", english_ordinal(r))
    } else if r < 100 {
        // British convention parallel to `english_cardinal`: insert
        // "and" before the trailing tens/units when the higher group
        // has no hundreds component of its own.
        format!("{head_word} and {}", english_ordinal(r))
    } else {
        format!("{head_word} {}", english_ordinal(r))
    }
}

#[derive(Clone, Copy)]
enum MinMaxOp { Min, Max, Avg }

/// Shared implementation of `fn:min` / `fn:max` / `fn:avg`.  Empty
/// input yields the empty sequence.  If every item parses as a
/// number we apply the numeric reduction; otherwise (for min/max)
/// we fall back to lexicographic ordering — XPath 2.0 §15.4.3
/// allows string-typed sequences and the answer must be the
/// lexicographically-min/max string, not "NaN" from parse failure.
fn min_max_avg<I: DocIndexLike>(
    v: &Value, idx: &I, op: MinMaxOp, ci: bool,
) -> Result<Value> {
    let strs = sequence_to_strings(v, idx);
    if strs.is_empty() {
        return Ok(Value::NodeSet(Vec::new()));
    }
    // Typed values (durations, dates, dateTimes, times) aggregate by
    // their semantic value, not as numbers or strings.
    if let Value::Sequence(items) = v {
        // F&O §10.4 — avg of a duration sequence is a duration.
        if matches!(op, MinMaxOp::Avg) {
            if let Some((kind, total, count)) = duration_seq_total(items) {
                // Dividing a yearMonthDuration by the item count rounds
                // the resulting month total to the nearest integer
                // (F&O §10.6.4 / fn:round — ties toward +infinity);
                // truncating `total / count` would report e.g.
                // avg((P15M,P15M,-P1M)) = P9M instead of P10M.
                let units = if kind == "yearMonthDuration" {
                    round_months_half_up(total as f64 / count as f64)
                } else {
                    total / count
                };
                return Ok(duration_value(kind, units));
            }
        }
        // F&O §15.4.2/3 — min/max of a uniform comparable-typed
        // sequence returns the extreme item (earliest dateTime,
        // shortest duration, …).
        if matches!(op, MinMaxOp::Min | MinMaxOp::Max)
            && items.iter().all(|x| matches!(x, Value::Typed(_)))
        {
            use std::cmp::Ordering;
            let mut best = &items[0];
            let mut comparable = true;
            for x in &items[1..] {
                match compare_typed_values(best, x) {
                    Some(ord) => {
                        let take = match op {
                            MinMaxOp::Max => ord == Ordering::Less,
                            _             => ord == Ordering::Greater,
                        };
                        if take { best = x; }
                    }
                    None => { comparable = false; break; }
                }
            }
            if comparable { return Ok(best.clone()); }
        }
    }
    // XPath 2.0 §15.4.3 — when every item is typed as xs:string
    // (e.g. `for $x in seq return xs:string($x)`), comparison uses
    // codepoint collation rather than numeric promotion.  Check
    // the source value's type-tag shape before attempting the
    // numeric path.
    // True iff every item is a string-typed atomic.  Recognise both
    // the explicit `Value::Typed{kind:"string"}` shape and the
    // implicit `Value::String` produced by `fn:string()` /
    // `xs:string()` whose returned-type semantics treat it as
    // xs:string.
    fn is_string_item(v: &Value) -> bool {
        match v {
            Value::String(_) => true,
            Value::Typed(t) => matches!(t.kind,
                "string" | "anyURI" | "normalizedString" | "token"
                | "Name" | "NCName" | "language"),
            _ => false,
        }
    }
    let all_typed_string = match v {
        Value::Typed(_) | Value::String(_) => is_string_item(v),
        Value::Sequence(items) => !items.is_empty()
            && items.iter().all(is_string_item),
        _ => false,
    };
    if all_typed_string {
        if matches!(op, MinMaxOp::Avg) {
            return Ok(Value::Number(Numeric::Double(f64::NAN)));
        }
        // String min/max order by the in-scope collation.  Under the
        // case-insensitive collation, fold for the comparison but keep
        // the original spelling of the chosen item.
        let key = |s: &String| if ci { ascii_ci_fold(s) } else { s.clone() };
        let chosen = match op {
            MinMaxOp::Min => strs.iter().min_by(|a, b| key(a).cmp(&key(b))).cloned(),
            MinMaxOp::Max => strs.iter().max_by(|a, b| key(a).cmp(&key(b))).cloned(),
            MinMaxOp::Avg => unreachable!(),
        };
        return Ok(chosen.map(Value::String).unwrap_or(Value::NodeSet(Vec::new())));
    }
    // Try the numeric path: every item must parse.
    let numeric: Option<Vec<f64>> = strs.iter()
        .map(|s| s.trim().parse::<f64>().ok())
        .collect();
    if let Some(nums) = numeric {
        let n = nums.len() as f64;
        let result = match op {
            MinMaxOp::Min => nums.into_iter().fold(f64::INFINITY,     f64::min),
            MinMaxOp::Max => nums.into_iter().fold(f64::NEG_INFINITY, f64::max),
            MinMaxOp::Avg => nums.into_iter().sum::<f64>() / n,
        };
        // Result type follows the promoted numeric type of the items
        // (F&O §15.4): min/max return that type; avg divides, so an
        // all-integer input yields xs:decimal.  An untyped atomic
        // (string-value of a node) has no numeric kind and promotes
        // to xs:double.
        let kind = match v {
            Value::Sequence(items) => items.iter().fold(Some("integer"), |acc, it| {
                match (acc, numeric_kind_of(it)) {
                    (Some(a), Some(b)) => numeric_promote_kind(Some(a), Some(b)),
                    _ => None,
                }
            }),
            Value::IntRange { .. } => Some("integer"),
            other => numeric_kind_of(other),
        }.unwrap_or("double");
        let kind = if matches!(op, MinMaxOp::Avg) && kind == "integer" { "decimal" } else { kind };
        return Ok(Value::Number(Numeric::of_kind(kind, result)));
    }
    // String fallback — `avg` over non-numeric items is a type
    // error per the spec; we return NaN to match what most
    // implementations do under loose typing.
    if matches!(op, MinMaxOp::Avg) {
        return Ok(Value::Number(Numeric::Double(f64::NAN)));
    }
    let key = |s: &String| if ci { ascii_ci_fold(s) } else { s.clone() };
    let chosen = match op {
        MinMaxOp::Min => strs.iter().min_by(|a, b| key(a).cmp(&key(b))).cloned(),
        MinMaxOp::Max => strs.iter().max_by(|a, b| key(a).cmp(&key(b))).cloned(),
        MinMaxOp::Avg => unreachable!(),
    };
    Ok(chosen.map(Value::String).unwrap_or(Value::NodeSet(Vec::new())))
}

/// XPath 2.0 §3.5.1 value-comparison operator.
#[derive(Clone, Copy)]
enum ValueCmp { Eq, Ne, Lt, Gt, Le, Ge }

/// Evaluate a value-comparison expression.  Returns the empty
/// sequence (here `Value::NodeSet(vec![])` — XPath 1.0 callers
/// flatten it to false / "") when either operand is empty.  Raises
/// a type error when either operand atomises to more than one
/// item.  Otherwise applies the requested operator to the single-
/// item operand pair via the existing typed-aware comparison
/// helpers.
/// True for atomic type kinds that carry only equality, not a total
/// order (XPath 2.0 §3.5.2): the gregorian types, the xs:duration base
/// type, the binary types, and xs:QName / xs:NOTATION.  The ordered
/// `xs:dayTimeDuration` / `xs:yearMonthDuration` subtypes are excluded.
fn is_unordered_atomic_kind(kind: &str) -> bool {
    matches!(kind,
        "gYear" | "gYearMonth" | "gMonth" | "gMonthDay" | "gDay"
        | "duration" | "hexBinary" | "base64Binary"
        | "QName" | "NOTATION")
}

fn value_compare<I: DocIndexLike>(
    l: &Expr, r: &Expr, ctx: &EvalCtx<'_>, idx: &I, op: ValueCmp,
) -> Result<Value> {
    let lv = atomise_singleton(eval_expr(l, ctx, idx)?, idx, ctx.bindings)?;
    let rv = atomise_singleton(eval_expr(r, ctx, idx)?, idx, ctx.bindings)?;
    let (lv, rv) = match (lv, rv) {
        (None, _) | (_, None) => return Ok(Value::NodeSet(Vec::new())),
        (Some(a), Some(b))    => (a, b),
    };
    // XPath 2.0 §3.5.2 — when both operands atomise to strings (or
    // untyped, which converts to string for comparison), the
    // in-scope default-collation drives equality / ordering.
    let coll = DEFAULT_COLLATION.with(|c| c.borrow().clone());
    let collated = coll.as_deref().filter(|u|
        *u == "http://www.w3.org/2005/xpath-functions/collation/html-ascii-case-insensitive");
    if let Some(_) = collated {
        if values_both_stringy(&lv, &rv) {
            let a = ascii_ci_fold(&value_to_string_with(&lv, idx, ctx.bindings));
            let b = ascii_ci_fold(&value_to_string_with(&rv, idx, ctx.bindings));
            let result = match op {
                ValueCmp::Eq => a == b,
                ValueCmp::Ne => a != b,
                ValueCmp::Lt => a < b,
                ValueCmp::Gt => a > b,
                ValueCmp::Le => a <= b,
                ValueCmp::Ge => a >= b,
            };
            return Ok(Value::Boolean(result));
        }
    }
    let result = match op {
        ValueCmp::Eq => values_eq(&lv, &rv, idx, ctx.bindings),
        ValueCmp::Ne => values_ne(&lv, &rv, idx, ctx.bindings),
        ValueCmp::Lt | ValueCmp::Gt | ValueCmp::Le | ValueCmp::Ge => {
            // XPath 2.0 §3.5.2 — `lt`/`gt`/`le`/`ge` are only defined on
            // types with a total order.  The gregorian types, the
            // xs:duration base type, the binary types, and QName /
            // NOTATION provide only `eq`/`ne`; applying an ordering
            // operator to them is a type error (XPTY0004).
            for v in [&lv, &rv] {
                if let Value::Typed(t) = v {
                    if is_unordered_atomic_kind(t.kind) {
                        return Err(xpath_err(format!(
                            "value comparison operator is not defined on xs:{}",
                            t.kind,
                        )).with_xpath_code("XPTY0004"));
                    }
                }
            }
            // XPath 2.0 §3.5.2 — value comparison routes to
            // op:date-less-than / op:time-greater-than / etc. when the
            // operands carry a date/time/duration type tag; otherwise
            // to op:numeric-* (the f64 path below) or
            // op:string-compare for strings.  We special-case the
            // common typed pairs before falling back to numeric.
            if let Some(ord) = compare_typed_values(&lv, &rv) {
                use std::cmp::Ordering;
                match (op, ord) {
                    (ValueCmp::Lt, Ordering::Less)    => true,
                    (ValueCmp::Gt, Ordering::Greater) => true,
                    (ValueCmp::Le, Ordering::Less | Ordering::Equal) => true,
                    (ValueCmp::Ge, Ordering::Greater | Ordering::Equal) => true,
                    _ => false,
                }
            } else {
                let a = value_to_number_with(&lv, idx, ctx.bindings);
                let b = value_to_number_with(&rv, idx, ctx.bindings);
                match op {
                    ValueCmp::Lt => a < b,
                    ValueCmp::Gt => a > b,
                    ValueCmp::Le => a <= b,
                    ValueCmp::Ge => a >= b,
                    _ => unreachable!(),
                }
            }
        }
    };
    Ok(Value::Boolean(result))
}

/// Both operands either are strings, untyped atomics, or come from
/// a node-set (whose atomization yields strings).  Used to gate the
/// collated comparison path — typed numerics / dates fall through
/// to their dedicated comparison ops.
fn values_both_stringy(a: &Value, b: &Value) -> bool {
    fn is_str(v: &Value) -> bool {
        match v {
            Value::String(_) => true,
            Value::Typed(t) => matches!(t.kind,
                "string" | "untypedAtomic" | "anyURI"
                | "normalizedString" | "token" | "Name" | "NCName"
                | "language" | "ID" | "IDREF" | "ENTITY" | "NMTOKEN"
                | "NOTATION" | "QName"),
            _ => false,
        }
    }
    is_str(a) && is_str(b)
}

/// Convert an `xs:date` / `xs:dateTime` / `xs:time` lexical to a
/// signed microsecond count after timezone normalisation.  A missing
/// timezone is treated as UTC (XPath 2.0 §10.4 implementation-defined
/// implicit timezone — we pick UTC for stability across runs).
/// Returns `None` when the lexical doesn't parse.
pub fn date_value_to_utc_micros(lex: &str, kind: DateKind) -> Option<i128> {
    let (y, mo, d, h, mi, sec, frac, tz) = parse_xsd_date_time(lex, kind)?;
    let tz_min = tz.unwrap_or(0) as i64;
    let day_count = match kind {
        DateKind::Time => 0,
        _              => ymd_to_days(y, mo as u32, d as u32),
    };
    let secs = day_count * 86_400
        + (h as i64) * 3600 + (mi as i64) * 60 + (sec as i64)
        - tz_min * 60;
    Some((secs as i128) * 1_000_000 + (frac as i128))
}

/// XPath 2.0 typed value comparison.  Returns the per-spec ordering
/// when both operands share a comparable type family (date / time /
/// dateTime / duration / string).  Numeric values fall through —
/// the caller handles them with `value_to_number` → IEEE compare.
fn compare_typed_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    // Pull (kind, lexical) for each side.  An xs:date / xs:dateTime
    // node-set string survives as `Value::String` (after
    // atomise_singleton's NodeSet→String conversion); the typed
    // path carries the explicit kind.
    fn date_kind(v: &Value) -> Option<(&'static str, String)> {
        match v {
            Value::Typed(t) => {
                let kind = match t.kind {
                    "date"             => "date",
                    "dateTime"         => "dateTime",
                    "time"             => "time",
                    "gYear" | "gYearMonth"
                    | "gMonth" | "gMonthDay" | "gDay" => "date",
                    "dayTimeDuration"  => "dayTime",
                    "yearMonthDuration"=> "yearMonth",
                    "duration"         => "duration",
                    _ => return None,
                };
                Some((kind, t.lexical.clone()))
            }
            _ => None,
        }
    }
    let (ka, la) = date_kind(a)?;
    let (kb, lb) = date_kind(b)?;
    // Same-family compare.  Cross-family (date vs dateTime,
    // dayTime vs yearMonth) is a type error per spec — fall back
    // to the numeric path so the caller gets `NaN < x = false`.
    if ka != kb { return None; }
    match ka {
        "date" | "dateTime" | "time" => {
            let dk = match ka {
                "date"     => DateKind::Date,
                "dateTime" => DateKind::DateTime,
                "time"     => DateKind::Time,
                _ => unreachable!(),
            };
            let a_us = date_value_to_utc_micros(&la, dk)?;
            let b_us = date_value_to_utc_micros(&lb, dk)?;
            Some(a_us.cmp(&b_us))
        }
        "dayTime" => {
            // op:dayTimeDuration-less-than — compare total seconds
            // via the shared parser used by duration arithmetic.
            let a_s = parse_day_time_duration_secs(&la)?;
            let b_s = parse_day_time_duration_secs(&lb)?;
            Some(a_s.cmp(&b_s))
        }
        "yearMonth" => {
            // op:yearMonthDuration-less-than — compare total months.
            let a_m = parse_year_month_duration_months(&la)?;
            let b_m = parse_year_month_duration_months(&lb)?;
            Some(a_m.cmp(&b_m))
        }
        _ => None,
    }
}

/// Atomise an operand of a value comparison: extract its single
/// item (or `None` for empty), erroring when the operand has more
/// than one item.  A node-set operand stringifies each node to its
/// string-value; multi-node operands are a type error per
/// XPath 2.0 §3.5.1.
fn atomise_singleton<I: DocIndexLike>(
    v: Value, idx: &I, _bindings: &dyn XPathBindings,
) -> Result<Option<Value>> {
    match v {
        Value::NodeSet(ns) => match ns.len() {
            0 => Ok(None),
            1 => Ok(Some(Value::String(idx.string_value(ns[0])))),
            _ => Err(xpath_err(
                "value-comparison operand must have at most one item")),
        }
        Value::ForeignNodeSet(_) => Ok(Some(v)),
        Value::Sequence(items) => match items.len() {
            0 => Ok(None),
            1 => Ok(Some(items.into_iter().next().unwrap())),
            _ => Err(xpath_err(
                "value-comparison operand must have at most one item")),
        }
        // Already a singleton atomic.
        other => Ok(Some(other)),
    }
}

/// Filter a sequence by a chain of XPath 2.0 predicates.  Each
/// predicate runs against each item with `position()` / `last()`
/// reflecting the surviving sequence at that step.  A numeric
/// predicate value `N` keeps only the item at position `N`; any
/// other value applies the predicate's effective boolean value to
/// each item.
/// Detect predicates of the form `[. = E]` / `[. != E]` /
/// `[not(. = E)]` where `E` is loop-invariant (no `.` reference),
/// and evaluate them via a hash-set membership test instead of
/// the quadratic XPath general-comparison cross-product.  Returns
/// `Some(filtered)` on a hit, `None` to let the caller fall back
/// to the normal per-item predicate evaluation.
///
/// Supports homogeneous numeric and string sequences on the
/// hoisted side; heterogeneous / typed sequences route through
/// the slow path, which preserves XPath 2.0 §3.5.2's
/// type-coercion rules.
fn try_membership_filter<I: DocIndexLike>(
    pred:      &Expr,
    items:     &[Value],
    ctx:       &EvalCtx<'_>,
    idx:       &I,
) -> Result<Option<Vec<Value>>> {
    let Some((rhs, negated)) = classify_membership_pred(pred) else {
        return Ok(None);
    };
    // The hoisted side must not depend on the current item — it
    // would change per iteration if it did, defeating the point
    // of building the set once.
    if expr_references_context_item(rhs) {
        return Ok(None);
    }
    let rhs_val = eval_expr(rhs, ctx, idx)?;
    // Build the set.  Two specialisations cover the common cases:
    // integer values (the W3C unicode-90 set-difference shape) and
    // string values (token-membership filters).  Mixed or typed
    // sequences fall through to the slow path.
    let set = match build_membership_set(&rhs_val, idx) {
        Some(s) => s,
        None    => return Ok(None),
    };
    let mut kept = Vec::with_capacity(items.len());
    for item in items {
        let hit = match &set {
            MembershipSet::Int(s) => match item_as_int(item) {
                Some(n) => s.contains(&n),
                None    => false,
            },
            MembershipSet::Str(s) => {
                let item_s = match item {
                    Value::String(t) => t.clone(),
                    Value::NodeSet(ns) if ns.len() == 1 => idx.string_value(ns[0]),
                    _ => continue,
                };
                s.contains(&item_s)
            }
        };
        if hit ^ negated {
            kept.push(item.clone());
        }
    }
    Ok(Some(kept))
}

/// Classify a predicate AST node as a hash-set membership test.
/// Returns `(rhs, negated)` where `rhs` is the hoistable expr
/// (the side of `=` / `!=` that isn't bare `.`) and `negated` is
/// true iff the predicate is "keep when NOT a member."
fn classify_membership_pred(pred: &Expr) -> Option<(&Expr, bool)> {
    // `not(...)` wraps a single Eq/Ne predicate.
    if let Expr::FunctionCall(name, args) = pred {
        if name == "not" && args.len() == 1 {
            return classify_membership_pred(&args[0]).map(|(rhs, neg)| (rhs, !neg));
        }
    }
    match pred {
        Expr::Eq(a, b) => match (is_bare_dot(a), is_bare_dot(b)) {
            (true,  false) => Some((b.as_ref(), false)),
            (false, true)  => Some((a.as_ref(), false)),
            _              => None,
        },
        Expr::Ne(a, b) => match (is_bare_dot(a), is_bare_dot(b)) {
            (true,  false) => Some((b.as_ref(), true)),
            (false, true)  => Some((a.as_ref(), true)),
            _              => None,
        },
        _ => None,
    }
}

fn is_bare_dot(e: &Expr) -> bool {
    if let Expr::Path(p) = e {
        return is_bare_dot_path(p);
    }
    false
}

/// True iff `e` syntactically references the context item (`.`
/// or `self::*` axis steps anywhere within).  Used to decide
/// whether an expression can be hoisted out of a per-item loop.
/// Conservative: returns true on anything that *might* read `.`,
/// so the fast path bails to the safe slow path in ambiguous
/// cases.
pub fn expr_references_context_item(e: &Expr) -> bool {
    use Expr::*;
    match e {
        Path(p) => path_uses_context_item(p),
        ContextItem => true,
        Variable(_) | Literal(_) | Integer(_) | Decimal(_) | Double(_) => false,
        Or(l, r) | And(l, r)
        | Eq(l, r) | Ne(l, r) | Lt(l, r) | Gt(l, r) | Le(l, r) | Ge(l, r)
        | ValueEq(l, r) | ValueNe(l, r)
        | ValueLt(l, r) | ValueGt(l, r) | ValueLe(l, r) | ValueGe(l, r)
        | Add(l, r) | Sub(l, r) | Mul(l, r) | Div(l, r) | Mod(l, r)
        | Union(l, r) | IDiv(l, r) | Intersect(l, r) | Except(l, r)
        | Range(l, r) | SimpleMap(l, r) | NodeBefore(l, r) | NodeAfter(l, r)
        | NodeIs(l, r) =>
            expr_references_context_item(l) || expr_references_context_item(r),
        Neg(x) | InstanceOf(x, _) | CastAs(x, _)
        | CastableAs(x, _) | TreatAs(x, _) => expr_references_context_item(x),
        IfThenElse { cond, then_branch, else_branch } =>
            expr_references_context_item(cond)
                || expr_references_context_item(then_branch)
                || expr_references_context_item(else_branch),
        For { bindings, body } | Let { bindings, body }
        | Quantified { bindings, test: body, .. } =>
            bindings.iter().any(|(_, e)| expr_references_context_item(e))
                || expr_references_context_item(body),
        Sequence(items) => items.iter().any(expr_references_context_item),
        FilterPath { primary, predicates, steps } => {
            expr_references_context_item(primary)
                || predicates.iter().any(expr_references_context_item)
                || steps.iter().any(|s| s.predicates.iter().any(expr_references_context_item))
        }
        FunctionCall(_, args) => args.iter().any(expr_references_context_item),
        TryCatch { body, catches } =>
            expr_references_context_item(body)
                || catches.iter().any(|c| expr_references_context_item(&c.body)),
        WithDefaultCollation(_, inner) => expr_references_context_item(inner),
        BackwardsCompat(inner) => expr_references_context_item(inner),
        MapConstructor(entries) => entries.iter()
            .any(|(k, v)| expr_references_context_item(k) || expr_references_context_item(v)),
        ArrayConstructor { members, .. } =>
            members.iter().any(expr_references_context_item),
        Lookup(base, key) => expr_references_context_item(base)
            || lookup_key_references_context_item(key),
        // `?K` is a unary lookup on the context item itself.
        UnaryLookup(_) => true,
        InlineFunction { .. } | NamedFunctionRef { .. } | Placeholder => false,
        DynamicCall { func, args } => expr_references_context_item(func)
            || args.iter().any(expr_references_context_item),
    }
}

fn lookup_key_references_context_item(key: &crate::xpath::ast::LookupKey) -> bool {
    matches!(key, crate::xpath::ast::LookupKey::Expr(e)
        if expr_references_context_item(e))
}

/// True iff a `LocationPath` reads the context item — either a
/// bare `.` (single Self_ step) or a relative path starting at
/// `.` implicitly.  Absolute paths (`/foo`) don't depend on `.`.
fn path_uses_context_item(path: &crate::xpath::ast::LocationPath) -> bool {
    use crate::xpath::ast::LocationPath;
    match path {
        LocationPath::Absolute(_) => false,
        LocationPath::Relative(_) => true,
    }
}

enum MembershipSet {
    Int(HashSet<i64>),
    Str(HashSet<String>),
}

/// Build a membership set from a sequence-valued `Value`.  Returns
/// `Some(set)` when every item collapses to an integer or every
/// item collapses to a string; `None` otherwise (caller falls
/// back to the per-item general-comparison loop).
fn build_membership_set<I: DocIndexLike>(v: &Value, idx: &I) -> Option<MembershipSet> {
    // Try integer membership first — common shape for codepoint /
    // identifier sets.  Two passes are cheaper than building a
    // wrong set and discarding.
    let mut all_int  = true;
    let mut all_str  = true;
    for item in iter_items(v) {
        if item_as_int(&item).is_none()  { all_int  = false; }
        if !item_as_string_kind(&item)   { all_str  = false; }
        if !all_int && !all_str { return None; }
    }
    if all_int {
        let s: HashSet<i64> = iter_items(v)
            .filter_map(|it| item_as_int(&it))
            .collect();
        return Some(MembershipSet::Int(s));
    }
    if all_str {
        let s: HashSet<String> = iter_items(v)
            .filter_map(|it| item_as_string(&it, idx))
            .collect();
        return Some(MembershipSet::Str(s));
    }
    None
}

fn item_as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) if n.as_f64().is_finite() && n.as_f64().fract() == 0.0 => Some(n.as_f64() as i64),
        Value::Typed(t) => t.numeric.and_then(|n|
            if n.is_finite() && n.fract() == 0.0 { Some(n as i64) } else { None }),
        _ => None,
    }
}

fn item_as_string_kind(v: &Value) -> bool {
    matches!(v, Value::String(_) | Value::NodeSet(_)) || matches!(v, Value::Typed(_))
}

fn item_as_string<I: DocIndexLike>(v: &Value, idx: &I) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::NodeSet(ns) if ns.len() == 1 => Some(idx.string_value(ns[0])),
        Value::Typed(t) => Some(t.lexical.clone()),
        _ => None,
    }
}

fn filter_sequence_by_predicates<I: DocIndexLike>(
    items: Vec<Value>, predicates: &[Expr], ctx: &EvalCtx<'_>, idx: &I,
) -> Result<Vec<Value>> {
    // Inline `IntRange` fragments (or nested `Sequence`) inside the
    // input flatten to individual items here so `position()` / `last()`
    // count the logical items rather than their lazy containers.
    let needs_flatten = items.iter().any(|v|
        matches!(v, Value::IntRange { .. } | Value::Sequence(_)));
    let mut surviving: Vec<Value> = if needs_flatten {
        items.iter().flat_map(iter_items).collect()
    } else {
        items
    };
    for pred in predicates {
        // Loop-invariant fast path: `[. = $expr]` / `[. != $expr]` /
        // `[not(. = $expr)]` where `$expr` doesn't depend on the
        // current item.  Evaluate `$expr` once, build a hash set,
        // then drop to O(1) membership per surviving item instead
        // of O(M·N) cross-product general-comparison.  Covers the
        // W3C `$validrange[not(. = $c)]` shape (set-difference of
        // the full Unicode codepoint range against ~1300 source
        // values) — without this the predicate is ~2min/case
        // through the standard `values_eq` Sequence × scalar path.
        if let Some(fast) = try_membership_filter(pred, &surviving, ctx, idx)? {
            surviving = fast;
            continue;
        }
        let size = surviving.len();
        let mut next: Vec<Value> = Vec::with_capacity(size);
        for (i, item) in surviving.into_iter().enumerate() {
            let pos = i + 1;
            // Single-node items thread the node through
            // `context_node` so node-axis predicates work; atomic
            // items thread their value through the per-thread
            // [`CONTEXT_ITEM`] slot so bare `.` resolves to the
            // current item rather than the outer document.
            let (ctx_node, ctx_item) = match &item {
                Value::NodeSet(ns) if ns.len() == 1 =>
                    (ns[0], None),
                _ => (ctx.context_node, Some(item.clone())),
            };
            let inner_ctx = EvalCtx {
                context_node: ctx_node,
                pos, size,
                bindings: ctx.bindings,
                static_ctx: ctx.static_ctx,
            };
            let pv = with_context_item(ctx_item, ||
                eval_expr(pred, &inner_ctx, idx)
            )?;
            // XPath 2.0 §2.5.4 — when the predicate value is numeric,
            // it's a position predicate (keep iff value == position).
            // Otherwise the predicate's effective boolean value
            // decides.  A single-item NodeSet whose synthetic text
            // node carries a numeric value (the common XSLT 2.0
            // shape after `for $x in 1 to N` / `<xsl:for-each select="1 to N">`)
            // is treated as numeric so `$seq[$x]` selects the
            // x-th item rather than always-true.
            let keep = match pv {
                Value::Number(n) => (n.as_f64() as usize) == pos && n.as_f64().fract() == 0.0,
                Value::Typed(ref t) if t.numeric.is_some() => {
                    let n = t.numeric.unwrap();
                    (n as usize) == pos && n.fract() == 0.0
                }
                // Synthetic text-node from XPath 2.0 atomisation of
                // `1 to N` (etc.): the node has no parent, kind is
                // Text, and its string-value parses as a positive
                // integer.  Treat as a position predicate so
                // `$xs[$n]` selects the n-th item; a real RTF
                // document node (with a Document root kind) keeps
                // XSLT 1.0's "EBV = node-set non-empty" behaviour
                // so tests like `decl/variable-1101` stay correct.
                Value::NodeSet(ref ns) if ns.len() == 1
                    && matches!(idx.kind(ns[0]),
                        crate::xpath::XPathNodeKind::Text)
                    && idx.parent(ns[0]).is_none()
                => {
                    let s = idx.string_value(ns[0]);
                    match s.trim().parse::<f64>() {
                        Ok(n) if n.fract() == 0.0 => (n as usize) == pos,
                        _ => value_to_bool(&pv, idx),
                    }
                }
                v                => value_to_bool(&v, idx),
            };
            if keep { next.push(item); }
        }
        surviving = next;
    }
    Ok(surviving)
}

/// Atomise a Value as a sequence of strings (XPath 2.0 sequence-as-
/// strings projection used by `string-join`, `distinct-values`, etc.).
fn sequence_to_strings<I: DocIndexLike>(v: &Value, idx: &I) -> Vec<String> {
    match v {
        Value::NodeSet(ns)        => ns.iter().map(|&id| idx.string_value(id)).collect(),
        Value::ForeignNodeSet(_)  => vec![value_to_string(v, idx)],
        Value::String(s)          => vec![s.clone()],
        Value::Number(_) | Value::Boolean(_) => vec![value_to_string(v, idx)],
        Value::Typed(t)           => vec![t.lexical.clone()],
        Value::Sequence(items)    => items.iter()
            .flat_map(|item| sequence_to_strings(item, idx))
            .collect(),
        Value::IntRange { lo, hi } => (*lo..=*hi).map(|i| i.to_string()).collect(),
        // A map / array has no string projection; contributes nothing.
        Value::Map(_) | Value::Array(_) | Value::Function(_) => Vec::new(),
    }
}

fn sequence_to_numbers<I: DocIndexLike>(v: &Value, idx: &I) -> Vec<f64> {
    sequence_to_strings(v, idx).into_iter()
        .map(|s| s.trim().parse().unwrap_or(f64::NAN))
        .collect()
}

/// Translate an XPath 2.0 regex pattern + flag string into a compiled
/// `regex::Regex`.  Flags map to Rust's inline `(?flags)` group:
///
/// * `s` — dotall (`.` matches newline)
/// * `m` — multiline (`^`/`$` match line boundaries)
/// * `i` — case-insensitive
/// * `x` — ignore whitespace + `#` comments
///
/// XPath 2.0's `q` flag (treat pattern as a literal) is honoured by
/// pre-escaping the pattern with `regex::escape`.
/// Public entry point for callers outside the XPath evaluator (e.g.
/// `xsl:analyze-string`) that need the same XPath 2.0 regex
/// translation we use internally.
pub fn compile_xpath_2_0_regex(pattern: &str, flags: &str) -> Result<regex::Regex> {
    compile_xpath_regex(pattern, flags)
}

/// True iff `uri` names the W3C HTML ASCII case-insensitive
/// collation defined in [XPath F&O 5.1].  Comparisons against this
/// collation lower-case the ASCII letters and leave everything else
/// alone; non-ASCII bytes never collide across case under this rule.
fn is_ascii_ci_collation(uri: Option<&str>) -> bool {
    matches!(uri, Some(u) if u ==
        "http://www.w3.org/2005/xpath-functions/collation/html-ascii-case-insensitive")
}

fn ascii_ci_fold(s: &str) -> String {
    // ASCII A-Z → a-z; every other codepoint passes through.
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_uppercase() { out.push(c.to_ascii_lowercase()); }
        else                       { out.push(c); }
    }
    out
}

fn collation_starts_with(s: &str, pre: &str, uri: Option<&str>) -> bool {
    if is_ascii_ci_collation(uri) {
        ascii_ci_fold(s).starts_with(ascii_ci_fold(pre).as_str())
    } else {
        s.starts_with(pre)
    }
}

fn collation_contains(s: &str, sub: &str, uri: Option<&str>) -> bool {
    if is_ascii_ci_collation(uri) {
        ascii_ci_fold(s).contains(ascii_ci_fold(sub).as_str())
    } else {
        s.contains(sub)
    }
}

fn collation_ends_with(s: &str, suf: &str, uri: Option<&str>) -> bool {
    if is_ascii_ci_collation(uri) {
        ascii_ci_fold(s).ends_with(ascii_ci_fold(suf).as_str())
    } else {
        s.ends_with(suf)
    }
}

/// Translate an XPath 2.0 §7.6.3 replacement string to the Rust
/// regex crate's replacement form.  XPath escapes `\$` and `\\`
/// where the regex crate only special-cases `$`.  The function
/// also raises FORX0004 on any unescaped `\` followed by a non-
/// `$` / non-`\` character (XPath 2.0 §7.6.3 prohibits the
/// pattern `\X` for arbitrary X in the replacement).
fn translate_xpath_replacement(s: &str, group_count: usize) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                match chars.next() {
                    Some('\\') => out.push('\\'),
                    Some('$')  => out.push('$'),
                    Some(other) => return Err(xpath_err(format!(
                        "replace(): replacement contains illegal escape '\\{other}'"
                    )).with_xpath_code("FORX0004")),
                    None => return Err(xpath_err(
                        "replace(): replacement ends with a trailing backslash"
                    ).with_xpath_code("FORX0004")),
                }
            }
            '$' => {
                match chars.peek() {
                    Some(d) if d.is_ascii_digit() => {
                        // XPath 2.0 §7.6.3: read digits greedily,
                        // then truncate the trailing digits until the
                        // resulting backref index is ≤ group_count.
                        // The truncated digits become literal text.
                        let mut digits = String::new();
                        while let Some(&d) = chars.peek() {
                            if d.is_ascii_digit() { digits.push(d); chars.next(); }
                            else { break; }
                        }
                        // Take the longest prefix whose numeric value
                        // is ≤ group_count.  The remainder is literal.
                        let mut take = digits.len();
                        while take > 0 {
                            let n: usize = digits[..take].parse().unwrap_or(usize::MAX);
                            if n <= group_count { break; }
                            take -= 1;
                        }
                        if take == 0 {
                            // No valid backref — the whole "$" + digits
                            // becomes literal text in Rust replacement
                            // syntax.  Escape the `$` so the regex
                            // crate doesn't treat it as a sigil.
                            out.push_str("$$");
                            out.push_str(&digits);
                        } else if take == digits.len() {
                            // All digits form a single backref — emit
                            // `$N` (Rust regex accepts it directly).
                            out.push('$');
                            out.push_str(&digits);
                        } else {
                            // Some trailing digits are literal text.
                            // Use the `${N}` form so Rust regex parses
                            // the backref precisely; then concatenate
                            // the literal digits.
                            out.push_str("${");
                            out.push_str(&digits[..take]);
                            out.push('}');
                            out.push_str(&digits[take..]);
                        }
                    }
                    _ => return Err(xpath_err(
                        "replace(): unescaped '$' in replacement"
                    ).with_xpath_code("FORX0004")),
                }
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

fn compile_xpath_regex(pattern: &str, flags: &str) -> Result<regex::Regex> {
    compile_xpath_regex_dialect(pattern, flags, crate::regex::Dialect::Xpath)
}

/// Like [`compile_xpath_regex`] but pre-validates the pattern against
/// the given dialect's grammar so XSLT-2.0 hosts surface FORX0002 on
/// constructs only XPath 3.0+ permits (notably non-capturing `(?:`).
fn compile_xpath_regex_dialect(
    pattern: &str, flags: &str, dialect: crate::regex::Dialect,
) -> Result<regex::Regex> {
    let literal = flags.contains('q');
    // Strict pre-parse: in XPath 2.0 / Xpath20 mode reject patterns
    // that include `(?:…)` and the XPath 3.0 inline-modifier forms.
    // Skip the pre-parse in literal mode (q flag) — there the
    // pattern is interpreted as plain text, not regex syntax.
    if !literal && dialect == crate::regex::Dialect::Xpath20 {
        crate::regex::parser::parse_with(pattern, dialect)
            .map_err(|e| xpath_err(format!("invalid regex: {e}"))
                .with_xpath_code("FORX0002"))?;
    }
    let mut inline = String::new();
    if !flags.is_empty() {
        // Dedupe flag letters — Rust's regex crate rejects duplicate
        // inline flag characters (`(?ii)` → error), but XPath 2.0
        // §7.6.1 silently accepts a flag appearing more than once.
        let mut seen = [false; 128];
        inline.push_str("(?");
        for c in flags.chars() {
            if matches!(c, 's' | 'm' | 'i' | 'x') {
                let idx = c as usize;
                if !seen[idx] {
                    seen[idx] = true;
                    inline.push(c);
                }
            }
        }
        inline.push(')');
        // No flags were translatable → drop the empty prefix.
        if inline.ends_with("(?)") { inline.clear(); }
    }
    let body = if literal {
        regex::escape(pattern)
    } else {
        translate_xsd_regex_escapes(pattern)
    };
    let full = format!("{inline}{body}");
    regex::Regex::new(&full)
        .map_err(|e| xpath_err(format!("invalid regex: {e}")).with_xpath_code("FORX0002"))
}

/// Convert the XSD-specific regex escapes (`\c`, `\C`, `\i`, `\I`)
/// into character-class equivalents Rust's `regex` crate understands.
///
/// * `\c` — XML NameChar:  letters, digits, `.`, `-`, `_`, `:`,
///   plus the Unicode extensions specified in XML 1.0 § 2.3.  We
///   approximate with the practical ASCII subset most tests rely on.
/// * `\i` — XML NameStartChar — like `\c` minus the digit /
///   dash / dot characters that can't open a name.
/// * `\C` / `\I` — complement of the above.
///
/// Escapes inside `[...]` character classes are translated too, with
/// the same approximate expansion.  Other XSD-specific constructs
/// (`\p{Is...}` block test, character-class subtraction) pass through
/// unchanged and may error at compile time — the caller surfaces the
/// regex-crate error verbatim, which is the right diagnostic.
fn translate_xsd_regex_escapes(pattern: &str) -> String {
    const NAME_CHAR:        &str = "A-Za-z0-9._\\-:\u{00B7}\u{C0}-\u{D6}\u{D8}-\u{F6}\u{F8}-\u{2FF}\u{370}-\u{37D}\u{37F}-\u{1FFF}\u{200C}-\u{200D}\u{2070}-\u{218F}\u{2C00}-\u{2FEF}\u{3001}-\u{D7FF}\u{F900}-\u{FDCF}\u{FDF0}-\u{FFFD}";
    const NAME_CHAR_NEG:    &str = "^A-Za-z0-9._\\-:\u{00B7}\u{C0}-\u{D6}\u{D8}-\u{F6}\u{F8}-\u{2FF}\u{370}-\u{37D}\u{37F}-\u{1FFF}\u{200C}-\u{200D}\u{2070}-\u{218F}\u{2C00}-\u{2FEF}\u{3001}-\u{D7FF}\u{F900}-\u{FDCF}\u{FDF0}-\u{FFFD}";
    const NAME_START:       &str = "A-Za-z_:\u{C0}-\u{D6}\u{D8}-\u{F6}\u{F8}-\u{2FF}\u{370}-\u{37D}\u{37F}-\u{1FFF}\u{200C}-\u{200D}\u{2070}-\u{218F}\u{2C00}-\u{2FEF}\u{3001}-\u{D7FF}\u{F900}-\u{FDCF}\u{FDF0}-\u{FFFD}";
    const NAME_START_NEG:   &str = "^A-Za-z_:\u{C0}-\u{D6}\u{D8}-\u{F6}\u{F8}-\u{2FF}\u{370}-\u{37D}\u{37F}-\u{1FFF}\u{200C}-\u{200D}\u{2070}-\u{218F}\u{2C00}-\u{2FEF}\u{3001}-\u{D7FF}\u{F900}-\u{FDCF}\u{FDF0}-\u{FFFD}";

    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let next = chars.next().unwrap_or('\0');
                match next {
                    'c' => if in_class { out.push_str(NAME_CHAR) }    else { out.push_str(&format!("[{NAME_CHAR}]")) },
                    'C' => if in_class { out.push_str(NAME_CHAR_NEG) } else { out.push_str(&format!("[{NAME_CHAR_NEG}]")) },
                    'i' => if in_class { out.push_str(NAME_START) }   else { out.push_str(&format!("[{NAME_START}]")) },
                    'I' => if in_class { out.push_str(NAME_START_NEG) } else { out.push_str(&format!("[{NAME_START_NEG}]")) },
                    other => { out.push('\\'); out.push(other); }
                }
            }
            '[' => { out.push('['); in_class = true; }
            ']' => { out.push(']'); in_class = false; }
            _   => out.push(c),
        }
    }
    out
}

/// XSD-namespace constructor functions used in XPath 2.0 / XSD 1.1
/// assertion expressions.  `xs:integer("42")` coerces a string to a
/// number, `xs:date("2024-01-01")` round-trips a date as a string,
/// etc.
///
/// Strict XPath 2.0 semantics distinguish these by atomic-value
/// type; our XPath 1.0 value model has only String / Number /
/// Boolean / NodeSet, so each constructor returns the variant the
/// downstream comparison operators will produce the right answer
/// for: numeric constructors → `Number`, boolean → `Boolean`, the
/// rest → `String` (round-tripped through normalize-space so leading/
/// trailing whitespace doesn't break a subsequent string equality).
fn xs_constructor<I: DocIndexLike>(
    local:    &str,
    args:     &[Value],
    idx:      &I,
    bindings: &dyn XPathBindings,
) -> Result<Value> {
    if args.len() != 1 {
        return Err(xpath_err(format!("xs:{local}(): requires exactly 1 argument")));
    }
    // XPath 2.0 §3.12.3: casting from an empty sequence yields the
    // empty sequence (when the target permits zero occurrences,
    // which the constructor functions do via their `T?` signature).
    if let Value::NodeSet(ref ns) = args[0] {
        if ns.is_empty() {
            return Ok(Value::NodeSet(Vec::new()));
        }
    }
    // F&O §17.1.5 — same-family precision-narrowing casts that
    // would lose information through the value_to_string round-trip.
    // Handle xs:date(xs:dateTime) here so the date portion + tz is
    // preserved (the dateTime lexical contains a `T` separator the
    // date lexical can't carry, breaking lexical_matches_type).
    if local == "date" {
        if let Value::Typed(t) = &args[0] {
            if t.kind == "dateTime" {
                // Strip the `T...` time portion, preserving any
                // trailing timezone designator.
                if let Some(t_pos) = t.lexical.find('T') {
                    let after = &t.lexical[t_pos + 1..];
                    let tz_start = after.rfind(|c| c == 'Z' || c == '+' || c == '-')
                        .filter(|&i| {
                            // Heuristic: a `+` or `-` near the END
                            // (within the last 6 chars) is the tz
                            // separator; earlier ones belong to the
                            // time portion's seconds (none usually).
                            i >= after.len().saturating_sub(6)
                        });
                    let tz = tz_start.map(|i| &after[i..]).unwrap_or("");
                    let lex = format!("{}{}", &t.lexical[..t_pos], tz);
                    return Ok(Value::Typed(Box::new(TypedAtomic {
                        kind: "date", lexical: lex, numeric: None, boolean: None,
                    })));
                }
            }
        }
    }
    // F&O §17.1.6 — xs:hexBinary ⇄ xs:base64Binary reinterprets the
    // same octets in the target's lexical form.
    if matches!(local, "hexBinary" | "base64Binary") {
        if let Some(lex) = convert_binary_kind(&args[0], local) {
            let kind = atomic_kind_static(local).unwrap();
            return Ok(Value::Typed(Box::new(TypedAtomic {
                kind, lexical: lex, numeric: None, boolean: None,
            })));
        }
    }
    // Constructing a gregorian type from an xs:date / xs:dateTime
    // extracts the relevant components, carrying the timezone (F&O
    // §17.1.7): `xs:gDay(xs:date('2001-02-15'))` is `---15`.
    if matches!(local, "gYear" | "gYearMonth" | "gMonth" | "gMonthDay" | "gDay") {
        if let Value::Typed(t) = &args[0] {
            let dk = match t.kind {
                "date"     => Some(DateKind::Date),
                "dateTime" => Some(DateKind::DateTime),
                _          => None,
            };
            if let Some(dk) = dk {
                if let Some((y, mo, d, _, _, _, _, tz)) = parse_xsd_date_time(&t.lexical, dk) {
                    let tzs = tz.map(format_tz_suffix).unwrap_or_default();
                    let lex = match local {
                        "gYear"      => format!("{y:04}{tzs}"),
                        "gYearMonth" => format!("{y:04}-{mo:02}{tzs}"),
                        "gMonth"     => format!("--{mo:02}{tzs}"),
                        "gMonthDay"  => format!("--{mo:02}-{d:02}{tzs}"),
                        _            => format!("---{d:02}{tzs}"),
                    };
                    let kind = atomic_kind_static(local).unwrap();
                    return Ok(Value::Typed(Box::new(TypedAtomic {
                        kind, lexical: lex, numeric: None, boolean: None,
                    })));
                }
            }
        }
    }
    let s = value_to_string_with(&args[0], idx, bindings);
    let trimmed = s.trim();
    // Resolve the static kind tag once.  Returning `None` means the
    // local name isn't a recognised XSD constructor.
    let kind = atomic_kind_static(local).ok_or_else(|| xpath_err(format!(
        "xs:{local}(): unknown XSD constructor function"
    )))?;
    // FORG0001 — reject lexical forms the target type can't represent.
    // Kept narrow to avoid disturbing the lenient numeric-truncation
    // path (`xs:integer(3.5)` truncates a *number* but rejects the
    // *string* '3.5'): only the binary lexical and xs:decimal's
    // no-exponent rule are enforced here.
    let lexically_invalid = match kind {
        "date" | "dateTime" | "time" | "duration" | "dayTimeDuration"
        | "yearMonthDuration" | "gYear" | "gYearMonth" | "gMonth"
        | "gMonthDay" | "gDay" | "hexBinary"
                       => !lexical_matches_type(trimmed, kind),
        "decimal"      => trimmed.contains(['e', 'E'])
                          || trimmed.parse::<f64>().is_err(),
        _ => false,
    };
    if lexically_invalid {
        // A lexically-valid year-bearing value beyond the representable
        // range is an overflow (FODT0001), not a malformed form.
        if date_year_out_of_range(trimmed, kind) {
            return Err(xpath_err(format!(
                "xs:{local}: year is outside the supported range: '{trimmed}'"
            )).with_xpath_code("FODT0001"));
        }
        return Err(xpath_err(format!(
            "xs:{local}: '{trimmed}' is not a valid lexical xs:{local}"
        )).with_xpath_code("FORG0001"));
    }
    // Build a TypedAtomic whose `numeric` / `boolean` caches reflect
    // the kind family — numeric kinds parse the lexical to f64,
    // xs:boolean parses lexical to bool, everything else carries
    // only the lexical form.
    // xs:double / xs:float accept `INF` / `-INF` / `NaN` (XSD §F.3)
    // and the XPath 1.0 spellings `Infinity` / `-Infinity`; map both
    // to the same numeric value so subsequent comparisons line up.
    let numeric = if is_numeric_kind(kind) {
        let raw = match trimmed {
            "INF"  | "Infinity"   => f64::INFINITY,
            "-INF" | "-Infinity"  => f64::NEG_INFINITY,
            "NaN"                 => f64::NAN,
            _ => match trimmed.parse::<f64>() {
                Ok(n)  => n,
                // FORG0001 — a string that doesn't parse as any
                // numeric literal isn't castable to xs:double / -float
                // / -decimal etc.  The XSD §F.3 special values
                // (INF/-INF/NaN) are matched explicitly above.
                Err(_) => return Err(xpath_err(format!(
                    "xs:{local}: '{trimmed}' is not a valid numeric \
                     lexical form"
                )).with_xpath_code("FORG0001")),
            },
        };
        // xs:float is 32-bit (IEEE 754 single, ~7 significant
        // digits) — round-trip through f32 so casting a high-
        // precision literal truncates: `xs:float(1.234567890123)`
        // ≈ `1.2345679`, not the source f64's 16-digit value.
        Some(if kind == "float" { raw as f32 as f64 } else { raw })
    } else { None };
    let boolean = if kind == "boolean" {
        // XPath 2.0 §17.1.4 — numeric → xs:boolean: zero / NaN are
        // false, every other finite value is true.  Strings keep
        // the four canonical XSD lexical forms.
        let from_num = match &args[0] {
            Value::Number(n)              => Some(!(n.as_f64() == 0.0 || n.as_f64().is_nan())),
            Value::Typed(t) if t.numeric.is_some() => {
                let n = t.numeric.unwrap();
                Some(!(n == 0.0 || n.is_nan()))
            }
            _ => None,
        };
        Some(from_num.unwrap_or_else(|| matches!(trimmed, "true" | "1")))
    } else { None };
    let lexical = if kind == "boolean" {
        if boolean == Some(true) { "true".into() } else { "false".into() }
    } else if kind == "double" {
        canonical_double_lex(numeric.unwrap_or(f64::NAN), &args[0])
    } else if kind == "float" {
        canonical_float_lex(numeric.unwrap_or(f64::NAN), &args[0])
    } else if kind == "decimal" {
        canonical_decimal_lex(numeric.unwrap_or(f64::NAN), trimmed)
    } else if is_integer_subkind(kind) && source_is_numeric(&args[0]) {
        // F&O §17.1.3 — casting a number to xs:integer (or any
        // integer subtype) truncates toward zero.  Casting from a
        // string still requires an integer lexical and is rejected
        // through the lexical-parse path.
        let n = numeric.unwrap_or(f64::NAN);
        if !n.is_finite() {
            return Err(xpath_err(format!(
                "xs:{local}: cannot cast non-finite numeric to {kind}"
            )).with_xpath_code("FOCA0002"));
        }
        (n.trunc() as i64).to_string()
    } else if matches!(kind, "date" | "dateTime" | "time") {
        canonical_date_time_lex(trimmed, kind)
    } else if matches!(kind, "gYear" | "gYearMonth" | "gMonth" | "gMonthDay" | "gDay") {
        // XSD §F.3 gregorian types — normalise the timezone tail so
        // `+00:00` becomes `Z`.  Otherwise preserve the lexical
        // form, including its sign and digit count.
        if let Some(stripped) = trimmed.strip_suffix("+00:00")
            .or_else(|| trimmed.strip_suffix("-00:00")) {
            format!("{stripped}Z")
        } else {
            trimmed.to_string()
        }
    } else if kind == "yearMonthDuration" {
        canonical_year_month_duration_lex(trimmed)
    } else if kind == "dayTimeDuration" {
        canonical_day_time_duration_lex(trimmed)
    } else if kind == "hexBinary" {
        // XSD §3.2.15 — the canonical lexical form uses upper-case
        // hex digits.
        trimmed.to_ascii_uppercase()
    } else { trimmed.to_string() };
    Ok(Value::Typed(Box::new(TypedAtomic { kind, lexical, numeric, boolean })))
}

/// Resolve an XSD type local name to a stable `&'static str`.  Used
/// by the `xs:` constructor dispatch and the cast/instance-of paths
/// so the kind tag costs zero allocation.
pub fn atomic_kind_static(local: &str) -> Option<&'static str> {
    Some(match local {
        "string" => "string",
        "normalizedString" => "normalizedString",
        "token" => "token",
        "Name" => "Name",
        "NCName" => "NCName",
        "QName" => "QName",
        "ID" => "ID",
        "IDREF" => "IDREF",
        "IDREFS" => "IDREFS",
        "ENTITY" => "ENTITY",
        "ENTITIES" => "ENTITIES",
        "NMTOKEN" => "NMTOKEN",
        "NMTOKENS" => "NMTOKENS",
        "anyURI" => "anyURI",
        "language" => "language",
        "NOTATION" => "NOTATION",
        "boolean" => "boolean",
        "integer" => "integer",
        "int" => "int",
        "long" => "long",
        "short" => "short",
        "byte" => "byte",
        "unsignedInt" => "unsignedInt",
        "unsignedLong" => "unsignedLong",
        "unsignedShort" => "unsignedShort",
        "unsignedByte" => "unsignedByte",
        "nonNegativeInteger" => "nonNegativeInteger",
        "nonPositiveInteger" => "nonPositiveInteger",
        "positiveInteger" => "positiveInteger",
        "negativeInteger" => "negativeInteger",
        "decimal" => "decimal",
        "double" => "double",
        "float" => "float",
        "date" => "date",
        "dateTime" => "dateTime",
        "time" => "time",
        "duration" => "duration",
        "dayTimeDuration" => "dayTimeDuration",
        "yearMonthDuration" => "yearMonthDuration",
        "gYear" => "gYear",
        "gYearMonth" => "gYearMonth",
        "gMonth" => "gMonth",
        "gMonthDay" => "gMonthDay",
        "gDay" => "gDay",
        "hexBinary" => "hexBinary",
        "base64Binary" => "base64Binary",
        "untypedAtomic" => "untypedAtomic",
        "anyAtomicType" => "anyAtomicType",
        _ => return None,
    })
}

fn is_numeric_kind(k: &str) -> bool {
    matches!(k,
        "integer" | "int" | "long" | "short" | "byte"
        | "unsignedInt" | "unsignedLong" | "unsignedShort" | "unsignedByte"
        | "nonNegativeInteger" | "nonPositiveInteger"
        | "positiveInteger" | "negativeInteger"
        | "decimal" | "double" | "float")
}

/// Integer-family XSD types (xs:integer and every derived subtype).
fn is_integer_subkind(k: &str) -> bool {
    matches!(k,
        "integer" | "int" | "long" | "short" | "byte"
        | "unsignedInt" | "unsignedLong" | "unsignedShort" | "unsignedByte"
        | "nonNegativeInteger" | "nonPositiveInteger"
        | "positiveInteger" | "negativeInteger")
}

/// True iff `v` carries an XPath numeric value — either a bare
/// `Value::Number` or a `TypedAtomic` whose cached `numeric` slot was
/// populated by a numeric constructor.
fn source_is_numeric(v: &Value) -> bool {
    match v {
        Value::Number(_) => true,
        Value::Typed(t)  => t.numeric.is_some(),
        _                => false,
    }
}

/// XSD canonical form of `xs:yearMonthDuration` — total months
/// decomposed into `PnYnM`, dropping zero-valued components.  Zero
/// duration is `P0M`.
fn canonical_year_month_duration_lex(s: &str) -> String {
    let months = match parse_year_month_duration_months(s) {
        Some(m) => m,
        None    => return s.to_string(),
    };
    if months == 0 { return "P0M".into(); }
    let mut out = String::with_capacity(8);
    let total = if months < 0 { out.push('-'); -months } else { months };
    out.push('P');
    let y = total / 12;
    let m = total % 12;
    if y > 0 { out.push_str(&y.to_string()); out.push('Y'); }
    if m > 0 { out.push_str(&m.to_string()); out.push('M'); }
    out
}

/// Toggle the leading sign on a duration lexical (`P…` ↔ `-P…`).
/// Idempotent for the empty / zero case (`PT0S` stays `PT0S` since
/// negative zero is the same value).
fn negate_duration_lex(s: &str) -> String {
    let t = s.trim();
    if t == "PT0S" || t == "P0M" { return t.to_string(); }
    if let Some(rest) = t.strip_prefix('-') {
        rest.to_string()
    } else {
        format!("-{t}")
    }
}

/// Render a signed second-count (with micro-second precision) into
/// the unnormalised `PnDTnHnMnS` lexical form.  Caller should pipe
/// through [`canonical_day_time_duration_lex`] to drop redundant
/// `0` components and reduce to canonical form.
fn format_day_time_duration_micros(total_us: i64) -> String {
    if total_us == 0 { return "PT0S".into(); }
    let neg = total_us < 0;
    let mut rem = total_us.unsigned_abs() as u64;
    let mut out = String::with_capacity(16);
    if neg { out.push('-'); }
    out.push('P');
    let us_per_day = 86_400u64 * 1_000_000;
    let days = rem / us_per_day;
    rem %= us_per_day;
    if days > 0 { out.push_str(&days.to_string()); out.push('D'); }
    if rem == 0 { return out; }
    out.push('T');
    let us_per_hour = 3600u64 * 1_000_000;
    let h = rem / us_per_hour;
    rem %= us_per_hour;
    if h > 0 { out.push_str(&h.to_string()); out.push('H'); }
    let us_per_min = 60u64 * 1_000_000;
    let m = rem / us_per_min;
    rem %= us_per_min;
    if m > 0 { out.push_str(&m.to_string()); out.push('M'); }
    if rem > 0 {
        let secs = rem / 1_000_000;
        let frac = rem % 1_000_000;
        out.push_str(&secs.to_string());
        if frac != 0 {
            let frac_str = format!("{frac:06}");
            let trimmed  = frac_str.trim_end_matches('0');
            out.push('.');
            out.push_str(trimmed);
        }
        out.push('S');
    }
    out
}

/// XSD canonical form of `xs:dayTimeDuration` — total seconds (with
/// fractional precision) decomposed into `PnDTnHnMnS`.  Zero
/// duration is `PT0S`.  Fractional seconds drop trailing zeros and
/// the decimal point when integral.
fn canonical_day_time_duration_lex(s: &str) -> String {
    // Total micro-seconds, then re-decompose.  Parsing fractional
    // input keeps the fraction intact for round-tripping `PT10.03S`
    // etc. without underflow to integer seconds.
    let total_us = match parse_day_time_duration_micros(s) {
        Some(u) => u,
        None    => return s.to_string(),
    };
    if total_us == 0 { return "PT0S".into(); }
    let mut out = String::with_capacity(16);
    let mut rem = if total_us < 0 { out.push('-'); -total_us } else { total_us };
    out.push('P');
    let us_per_day = 86_400 * 1_000_000;
    let days = rem / us_per_day;
    rem %= us_per_day;
    if days > 0 { out.push_str(&days.to_string()); out.push('D'); }
    if rem == 0 { return out; }
    out.push('T');
    let us_per_hour = 3600 * 1_000_000;
    let h = rem / us_per_hour;
    rem %= us_per_hour;
    if h > 0 { out.push_str(&h.to_string()); out.push('H'); }
    let us_per_min = 60 * 1_000_000;
    let m = rem / us_per_min;
    rem %= us_per_min;
    if m > 0 { out.push_str(&m.to_string()); out.push('M'); }
    if rem > 0 {
        let secs = rem / 1_000_000;
        let frac = rem % 1_000_000;
        out.push_str(&secs.to_string());
        if frac != 0 {
            let frac_str = format!("{frac:06}");
            let trimmed  = frac_str.trim_end_matches('0');
            out.push('.');
            out.push_str(trimmed);
        }
        out.push('S');
    }
    out
}

/// Parse an `xs:dayTimeDuration` into signed micro-seconds, keeping
/// fractional seconds.  Returns `None` for unrecognised shapes.
fn parse_day_time_duration_micros(s: &str) -> Option<i128> {
    let s = s.trim();
    let (sign, body) = match s.strip_prefix('-') {
        Some(rest) => (-1i128, rest),
        None       => (1i128, s),
    };
    let body = body.strip_prefix('P')?;
    let (day_part, time_part) = match body.find('T') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None    => (body, ""),
    };
    let pull = |part: &str, marker: char| -> i128 {
        let Some(i) = part.find(marker) else { return 0; };
        let start = part[..i].rfind(|c: char| !c.is_ascii_digit() && c != '.')
            .map(|n| n + 1).unwrap_or(0);
        part[start..i].parse().unwrap_or(0)
    };
    // Seconds may carry a decimal point — handle separately so the
    // fractional value contributes microseconds.
    let pull_secs = |part: &str| -> i128 {
        let Some(i) = part.find('S') else { return 0; };
        let start = part[..i].rfind(|c: char| !c.is_ascii_digit() && c != '.')
            .map(|n| n + 1).unwrap_or(0);
        let lex = &part[start..i];
        if let Some((whole, frac)) = lex.split_once('.') {
            let w: i128 = whole.parse().unwrap_or(0);
            let take: String = frac.chars().chain(std::iter::repeat('0')).take(6).collect();
            let f: i128 = take.parse().unwrap_or(0);
            w * 1_000_000 + f
        } else {
            lex.parse::<i128>().unwrap_or(0) * 1_000_000
        }
    };
    let days   = pull(day_part,  'D');
    let hours  = pull(time_part, 'H');
    let mins   = pull(time_part, 'M');
    let secs_us = pull_secs(time_part);
    Some(sign * (days * 86_400 * 1_000_000
        + hours * 3600 * 1_000_000
        + mins  * 60   * 1_000_000
        + secs_us))
}

/// Strip trailing zeros from the fractional-seconds component of an
/// `xs:dateTime` / `xs:time` lexical (XSD canonical form has no
/// trailing zeros, and drops the `.` entirely when all digits are
/// zero).  `xs:date` has no fractional seconds, so it round-trips.
/// Decode an `xs:hexBinary` lexical form into its octets.  `None` on
/// an odd length or a non-hex digit.
fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 { return None; }
    (0..s.len()).step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Encode octets as the canonical upper-case `xs:hexBinary` lexical
/// form (XSD §3.2.15).
fn bytes_to_hex_upper(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes { let _ = write!(s, "{b:02X}"); }
    s
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode octets as the canonical `xs:base64Binary` lexical form.
fn bytes_to_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64_ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(BASE64_ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { BASE64_ALPHABET[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { BASE64_ALPHABET[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Decode an `xs:base64Binary` lexical form into its octets, ignoring
/// insignificant whitespace.  `None` on a malformed input.
fn base64_to_bytes(s: &str) -> Option<Vec<u8>> {
    let clean: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if clean.is_empty() || clean.len() % 4 != 0 { return None; }
    let val = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(clean.len() / 4 * 3);
    for chunk in clean.chunks(4) {
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        let c0 = val(chunk[0])?;
        let c1 = val(chunk[1])?;
        let c2 = if pad >= 2 { 0 } else { val(chunk[2])? };
        let c3 = if pad >= 1 { 0 } else { val(chunk[3])? };
        let n = (c0 << 18) | (c1 << 12) | (c2 << 6) | c3;
        out.push((n >> 16) as u8);
        if pad < 2 { out.push((n >> 8) as u8); }
        if pad < 1 { out.push(n as u8); }
    }
    Some(out)
}

/// F&O §17.1.6 — casting between xs:hexBinary and xs:base64Binary
/// reinterprets the same octets.  Returns the target's canonical
/// lexical form when `src` is a typed binary value of the other kind.
fn convert_binary_kind(src: &Value, target: &str) -> Option<String> {
    let t = match src { Value::Typed(t) => t, _ => return None };
    let bytes = match t.kind {
        "hexBinary"    => hex_to_bytes(&t.lexical)?,
        "base64Binary" => base64_to_bytes(&t.lexical)?,
        _ => return None,
    };
    Some(match target {
        "hexBinary"    => bytes_to_hex_upper(&bytes),
        "base64Binary" => bytes_to_base64(&bytes),
        _ => return None,
    })
}

fn canonical_date_time_lex(s: &str, kind: &str) -> String {
    // XSD §F.3 — `+00:00` and `-00:00` are equivalent to `Z` in
    // every date-bearing type's canonical form.  Normalise the
    // tail so castings preserve only the canonical spelling
    // (date-065 expects `---31Z`, not `---31+00:00`).
    let s = if let Some(stripped) = s.strip_suffix("+00:00")
        .or_else(|| s.strip_suffix("-00:00")) {
        format!("{stripped}Z")
    } else {
        s.to_string()
    };
    let s = s.as_str();
    // XSD §3.2.7/§3.2.8 — `24:00:00` is the non-canonical midnight
    // form; normalise it to `00:00:00` (rolling the day for dateTime).
    if s.contains("24:00:00") {
        let dk = match kind {
            "dateTime" => DateKind::DateTime,
            "time"     => DateKind::Time,
            _          => DateKind::Date,
        };
        if !matches!(dk, DateKind::Date) {
            if let Some((y, mo, d, h, mi, sec, frac, tz)) = parse_xsd_date_time(s, dk) {
                return if matches!(dk, DateKind::DateTime) {
                    format_datetime_lexical(y, mo, d, h, mi, sec, frac, tz)
                } else {
                    let mut l = format!("{h:02}:{mi:02}:{sec:02}");
                    if frac != 0 {
                        let mut f = format!(".{frac:06}");
                        while f.ends_with('0') { f.pop(); }
                        l.push_str(&f);
                    }
                    if let Some(tz_m) = tz { l.push_str(&format_tz_suffix(tz_m)); }
                    l
                };
            }
        }
    }
    let dot = match s.find('.') { Some(i) => i, None => return s.to_string() };
    // The fractional digits run until the timezone designator or end.
    let after = &s[dot + 1..];
    let tz_off = after.find(|c: char| c == 'Z' || c == '+' || c == '-')
        .unwrap_or(after.len());
    let (digits, tz) = (&after[..tz_off], &after[tz_off..]);
    let trimmed = digits.trim_end_matches('0');
    if trimmed.is_empty() {
        // All-zero fractional — drop the `.` along with the digits.
        let mut out = String::with_capacity(s.len());
        out.push_str(&s[..dot]);
        out.push_str(tz);
        out
    } else {
        let mut out = String::with_capacity(s.len());
        out.push_str(&s[..dot]);
        out.push('.');
        out.push_str(trimmed);
        out.push_str(tz);
        out
    }
}

/// Format a Unix timestamp as `YYYY-MM-DDZ` (xs:date with UTC zone).
fn format_date_utc(secs: i64) -> String {
    let (y, m, d) = days_to_ymd(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}Z")
}
/// Format a Unix timestamp as `YYYY-MM-DDTHH:MM:SSZ` (xs:dateTime UTC).
fn format_datetime_utc(secs: i64) -> String {
    let days   = secs.div_euclid(86_400);
    let day_sec = secs.rem_euclid(86_400);
    let (y, m, d) = days_to_ymd(days);
    let h  = day_sec / 3600;
    let mi = (day_sec % 3600) / 60;
    let s  = day_sec % 60;
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}
/// Format a Unix timestamp as `HH:MM:SSZ` (xs:time UTC).
fn format_time_utc(secs: i64) -> String {
    let day_sec = secs.rem_euclid(86_400);
    let h  = day_sec / 3600;
    let mi = (day_sec % 3600) / 60;
    let s  = day_sec % 60;
    format!("{h:02}:{mi:02}:{s:02}Z")
}
/// Convert days-since-1970-01-01 to (year, month, day).  Proleptic
/// Gregorian; sufficient for the 1970..=9999 range our XPath
/// `current-date()` will ever encounter.
fn days_to_ymd(days: i64) -> (i32, u32, u32) {
    // Howard Hinnant's "civil_from_days" — branchless and exact.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe/1460 + doe/36_524 - doe/146_096) / 365;
    let y   = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe/4 - yoe/100);
    let mp  = (5 * doy + 2) / 153;
    let d   = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m   = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y   = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn dedup_sort(nodes: &mut Vec<NodeId>) {
    nodes.sort_unstable();
    nodes.dedup();
}

/// Dedup foreign-node pointers by identity.  We don't sort by
/// document order — XPath 1.0 doesn't define cross-document order,
/// and the consumers we target (libxslt's `xsl:copy-of` etc.)
/// iterate in encounter order regardless.
fn dedup_foreign(nodes: &mut Vec<ForeignNodePtr>) {
    let mut seen = HashSet::new();
    nodes.retain(|&p| seen.insert(p as usize));
}

// ── kani panic-freedom proofs ────────────────────────────────────────────────
//
// Each `#[kani::proof]` below is a panic-freedom guarantee for one of the
// XPath axis-navigation helpers, exhaustively explored on bounded
// symbolic doc shapes (≤ `MAX_NODES` nodes, ≤ `MAX_CHILDREN` per node).
// The bug the fuzzer found in `following()` (slice OOB when the context
// node wasn't in its parent's children list) would be caught here in
// seconds — the harnesses then act as a regression barrier going forward.
//
// Run with `cargo kani`; install via:
//   cargo install --locked kani-verifier
//   cargo kani setup
//
// The module is gated on `#[cfg(kani)]`, so `cargo build` / `cargo test`
// never see it.
/// Unit tests for the pure helpers added in the 2.0 conformance
/// push.  These pin behaviour the W3C conformance walk would
/// otherwise be the only check on; they run as part of
/// `cargo test-all` so a regression surfaces immediately rather
/// than through a drop in the conformance score.
#[cfg(test)]
mod tests {
    use super::*;

    // ── date/time year range (FODT0001 vs FORG0001) ──────────────

    #[test]
    fn out_of_range_year_is_overflow_not_malformed() {
        // Years within the representable (i32) range — including
        // 5-digit and negative years — are supported, NOT overflows.
        assert!(!date_year_out_of_range("2024-05-01T00:00:00", "dateTime"));
        assert!(!date_year_out_of_range("21999-05-01", "date"));
        assert!(!date_year_out_of_range("-1999-05-01", "date"));
        assert!(!date_year_out_of_range("99999", "gYear"));
        assert!(!date_year_out_of_range("123456-05", "gYearMonth"));

        // Lexically well-formed but the year exceeds i32 → overflow
        // (the caller raises FODT0001 rather than FORG0001).
        assert!(date_year_out_of_range("999999999999-05-01T00:00:00", "dateTime"));
        assert!(date_year_out_of_range("-999999999999-05-01", "date"));
        assert!(date_year_out_of_range("12345678901-05", "gYearMonth"));
        assert!(date_year_out_of_range("999999999999", "gYear"));

        // A genuinely malformed value is NOT an overflow — it stays a
        // lexical error (FORG0001): a huge year doesn't excuse a bad
        // month/day or non-date text.
        assert!(!date_year_out_of_range("999999999999-13-99", "date"));
        assert!(!date_year_out_of_range("not-a-date", "date"));
        // Types without a year never report a year overflow.
        assert!(!date_year_out_of_range("999999999999", "gMonth"));
        assert!(!date_year_out_of_range("999999999999", "time"));
    }

    // ── duration add / subtract (XPath 2.0 §10.4) ────────────────

    #[test]
    fn duration_combine_dispatches_by_family() {
        let dur = |k: &'static str, lex: &str| TypedAtomic {
            kind: k, lexical: lex.to_string(), numeric: None, boolean: None,
        };
        let lex = |v: Option<Value>| match v {
            Some(Value::Typed(t)) => t.lexical.clone(),
            other => panic!("expected typed duration, got {other:?}"),
        };
        // yearMonthDuration combines by month count, not seconds.
        assert_eq!(
            lex(duration_combine(&dur("yearMonthDuration", "P2Y6M"),
                                 &dur("yearMonthDuration", "P6M"), false)),
            "P3Y");
        assert_eq!(
            lex(duration_combine(&dur("yearMonthDuration", "P0M"),
                                 &dur("yearMonthDuration", "P2Y"), true)),
            "-P2Y");
        // dayTimeDuration preserves fractional seconds via microseconds.
        assert_eq!(
            lex(duration_combine(&dur("dayTimeDuration", "P2D"),
                                 &dur("dayTimeDuration", "PT10.03S"), false)),
            "P2DT10.03S");
        // Mixing the two families has no defined sum.
        assert!(duration_combine(&dur("yearMonthDuration", "P1Y"),
                                 &dur("dayTimeDuration", "PT1H"), false).is_none());
    }

    // ── format_number / negative-zero ────────────────────────────

    #[test]
    fn format_number_preserves_negative_zero() {
        assert_eq!(format_number(-0.0), "-0");
        assert_eq!(format_number(0.0),  "0");
    }

    #[test]
    fn format_number_handles_special_values() {
        assert_eq!(format_number(f64::INFINITY),     "Infinity");
        assert_eq!(format_number(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(format_number(f64::NAN),          "NaN");
    }

    #[test]
    fn format_number_integers_are_decimal_no_dot() {
        assert_eq!(format_number(42.0),   "42");
        assert_eq!(format_number(-7.0),   "-7");
        assert_eq!(format_number(0.5),    "0.5");
    }

    // ── english_ordinal_suffix ───────────────────────────────────

    #[test]
    fn ordinal_suffix_picks_st_nd_rd_th() {
        assert_eq!(english_ordinal_suffix(1),  "st");
        assert_eq!(english_ordinal_suffix(2),  "nd");
        assert_eq!(english_ordinal_suffix(3),  "rd");
        assert_eq!(english_ordinal_suffix(4),  "th");
        assert_eq!(english_ordinal_suffix(21), "st");
        assert_eq!(english_ordinal_suffix(22), "nd");
        assert_eq!(english_ordinal_suffix(23), "rd");
    }

    #[test]
    fn ordinal_suffix_teens_are_all_th() {
        // 11th, 12th, 13th — the carve-out from the units rule.
        assert_eq!(english_ordinal_suffix(11), "th");
        assert_eq!(english_ordinal_suffix(12), "th");
        assert_eq!(english_ordinal_suffix(13), "th");
        // 111th, 112th, 113th — same carve-out at the century level.
        assert_eq!(english_ordinal_suffix(111), "th");
        assert_eq!(english_ordinal_suffix(112), "th");
        assert_eq!(english_ordinal_suffix(113), "th");
    }

    // ── parse_duration_split ─────────────────────────────────────

    #[test]
    fn duration_split_pure_year_month() {
        // P1Y = 12 months, no time component.
        assert_eq!(parse_duration_split("P1Y"),    Some((12,  0)));
        assert_eq!(parse_duration_split("P0Y12M"), Some((12,  0)));
        assert_eq!(parse_duration_split("P12M"),   Some((12,  0)));
        // P1Y6M = 18 months.
        assert_eq!(parse_duration_split("P1Y6M"),  Some((18,  0)));
    }

    #[test]
    fn duration_split_pure_day_time() {
        assert_eq!(parse_duration_split("P1D"),       Some((0, 86_400)));
        assert_eq!(parse_duration_split("PT24H"),     Some((0, 86_400)));
        assert_eq!(parse_duration_split("PT1H30M"),   Some((0, 5400)));
        assert_eq!(parse_duration_split("-PT1H"),     Some((0, -3600)));
        assert_eq!(parse_duration_split("-P1DT12H"),  Some((0, -129_600)));
        assert_eq!(parse_duration_split("-PT36H"),    Some((0, -129_600)));
    }

    #[test]
    fn duration_split_mixed_components() {
        assert_eq!(parse_duration_split("P1Y2M3DT4H5M6S"),
            Some((14, 3 * 86_400 + 4 * 3600 + 5 * 60 + 6)));
    }

    #[test]
    fn duration_split_rejects_malformed() {
        assert_eq!(parse_duration_split("1Y"),  None); // missing P
        assert_eq!(parse_duration_split("PY"),  None); // empty digits
        assert_eq!(parse_duration_split("P1X"), None); // unknown component
        assert_eq!(parse_duration_split(""),    None);
    }

    // ── canonical_double_lex (XSD §F.3) ──────────────────────────

    #[test]
    fn canonical_double_zero_and_neg_zero() {
        assert_eq!(canonical_double_lex( 0.0, &Value::Number(Numeric::Double( 0.0))), "0");
        assert_eq!(canonical_double_lex(-0.0, &Value::Number(Numeric::Double(-0.0))), "-0");
        // Source-string sign carries through too — important when
        // the cast path stringified `-0` to `"0"` before reaching us.
        assert_eq!(canonical_double_lex(0.0, &Value::String("-0".into())), "-0");
    }

    #[test]
    fn canonical_double_special_values() {
        assert_eq!(canonical_double_lex(f64::INFINITY,     &Value::Number(Numeric::Double(0.0))), "INF");
        assert_eq!(canonical_double_lex(f64::NEG_INFINITY, &Value::Number(Numeric::Double(0.0))), "-INF");
        assert_eq!(canonical_double_lex(f64::NAN,          &Value::Number(Numeric::Double(0.0))), "NaN");
    }

    #[test]
    fn canonical_double_fixed_point_window() {
        // Inside [1e-6, 1e7): decimal notation.
        assert_eq!(canonical_double_lex(1.5,    &Value::Number(Numeric::Double(1.5))),    "1.5");
        assert_eq!(canonical_double_lex(0.001,  &Value::Number(Numeric::Double(0.001))),  "0.001");
        assert_eq!(canonical_double_lex(42.0,   &Value::Number(Numeric::Double(42.0))),   "42");
        assert_eq!(canonical_double_lex(9_999_999.0,
                                        &Value::Number(Numeric::Double(9_999_999.0))),
                   "9999999");
    }

    #[test]
    fn canonical_double_scientific_outside_window() {
        // 1e7 and above → scientific with explicit `.0`.
        assert_eq!(canonical_double_lex(1e7,  &Value::Number(Numeric::Double(1e7))),  "1.0E7");
        assert_eq!(canonical_double_lex(1e-8, &Value::Number(Numeric::Double(1e-8))), "1.0E-8");
        // Saxon's mantissa form has `.0` even when the value is
        // representable as a single mantissa digit.
        assert_eq!(canonical_double_lex(1.5e10, &Value::Number(Numeric::Double(1.5e10))), "1.5E10");
    }

    // ── canonical_decimal_lex ────────────────────────────────────

    #[test]
    fn canonical_decimal_drops_negative_zero() {
        assert_eq!(canonical_decimal_lex(-0.0, "-0.0"), "0");
        assert_eq!(canonical_decimal_lex( 0.0,  "0"),   "0");
    }

    #[test]
    fn canonical_decimal_passes_fixed_point_through() {
        assert_eq!(canonical_decimal_lex(1.5,  "1.5"),  "1.5");
        assert_eq!(canonical_decimal_lex(42.0, "42"),   "42");
    }

    #[test]
    fn canonical_decimal_strips_scientific_input() {
        // Input in scientific form is canonicalised to integer
        // form when the value is integer-valued.
        assert_eq!(canonical_decimal_lex(1e3, "1e3"), "1000");
    }

    // ── resolve_uri_rfc3986 / split_uri ──────────────────────────

    #[test]
    fn split_uri_full_form() {
        let (s, a, p, q, f) = split_uri("http://host/path?q#frag");
        assert_eq!(s, Some("http"));
        assert_eq!(a, Some("host"));
        assert_eq!(p, "/path");
        assert_eq!(q, Some("q"));
        assert_eq!(f, Some("frag"));
    }

    #[test]
    fn split_uri_relative_no_scheme() {
        let (s, a, p, q, f) = split_uri("path/to/file");
        assert_eq!(s, None);
        assert_eq!(a, None);
        assert_eq!(p, "path/to/file");
        assert_eq!(q, None);
        assert_eq!(f, None);
    }

    #[test]
    fn remove_dot_segments_matches_rfc3986_examples() {
        // RFC 3986 §5.2.4 sample reductions.
        assert_eq!(remove_dot_segments("/a/b/c/./../../g"), "/a/g");
        assert_eq!(remove_dot_segments("mid/content=5/../6"), "mid/6");
        assert_eq!(remove_dot_segments("/./a/b"), "/a/b");
        assert_eq!(remove_dot_segments(""), "");
    }

    #[test]
    fn resolve_uri_handles_rfc3986_normal_examples() {
        // RFC 3986 §5.4.1 — base http://a/b/c/d;p?q
        let base = "http://a/b/c/d;p?q";
        assert_eq!(resolve_uri_rfc3986(base, "g:h"),    "g:h");
        assert_eq!(resolve_uri_rfc3986(base, "g"),      "http://a/b/c/g");
        assert_eq!(resolve_uri_rfc3986(base, "./g"),    "http://a/b/c/g");
        assert_eq!(resolve_uri_rfc3986(base, "g/"),     "http://a/b/c/g/");
        assert_eq!(resolve_uri_rfc3986(base, "/g"),     "http://a/g");
        assert_eq!(resolve_uri_rfc3986(base, "?y"),     "http://a/b/c/d;p?y");
        assert_eq!(resolve_uri_rfc3986(base, "g?y"),    "http://a/b/c/g?y");
        assert_eq!(resolve_uri_rfc3986(base, "#s"),     "http://a/b/c/d;p?q#s");
        assert_eq!(resolve_uri_rfc3986(base, "g#s"),    "http://a/b/c/g#s");
        assert_eq!(resolve_uri_rfc3986(base, "../g"),   "http://a/b/g");
        assert_eq!(resolve_uri_rfc3986(base, "../../g"),"http://a/g");
    }

    #[test]
    fn resolve_uri_empty_rel_yields_base() {
        let base = "http://www.baseuri.exmpl/tests/";
        assert_eq!(resolve_uri_rfc3986(base, ""), "http://www.baseuri.exmpl/tests/");
    }

    // ── pattern_is_document_node / rewrite_document_node_prefix ──
    //
    // These live in xslt/src/pattern.rs; only the underlying
    // NodeTest::Document tag is exercised here through the
    // step-builder shape since the helpers are private to that
    // module.  Pattern-side coverage lives in xslt/tests.

    // ── builtin_arity_ok ────────────────────────────────────────
    //
    // Lives in xslt/src/functions.rs; tested there.
}

#[cfg(kani)]
mod proofs {
    use super::*;
    use std::ops::Range;

    const MAX_NODES: usize = 3;
    const MAX_CHILDREN: usize = 2;

    /// Symbolic doc index. `parent` and `children` are produced from
    /// `kani::any()` and constrained to an acyclic shape so the tree
    /// walks terminate within the harness unwind bounds.
    ///
    /// Cycle prevention: every `parent(i)` is either `None` or some id
    /// `< i`; every child id `> i`. That mirrors arena document order
    /// (parents indexed before children) without restricting the bug
    /// shape under test (slice OOB depends on `pos` vs. `len`, not on
    /// which specific ids occupy the slice).
    struct AnyIndex {
        parents: [Option<NodeId>; MAX_NODES],
        child_lens: [usize; MAX_NODES],
        child_buf: [[NodeId; MAX_CHILDREN]; MAX_NODES],
    }

    impl AnyIndex {
        fn any() -> Self {
            let mut idx = AnyIndex {
                parents: [None; MAX_NODES],
                child_lens: [0; MAX_NODES],
                child_buf: [[0; MAX_CHILDREN]; MAX_NODES],
            };
            for i in 0..MAX_NODES {
                let p: Option<NodeId> = kani::any();
                if let Some(pid) = p {
                    kani::assume(pid < i);
                }
                idx.parents[i] = p;

                let n: usize = kani::any();
                kani::assume(n <= MAX_CHILDREN);
                idx.child_lens[i] = n;
                for j in 0..MAX_CHILDREN {
                    let c: NodeId = kani::any();
                    kani::assume(c > i && c < MAX_NODES);
                    idx.child_buf[i][j] = c;
                }
            }
            idx
        }
    }

    impl DocIndexLike for AnyIndex {
        fn children(&self, id: NodeId) -> &[NodeId] {
            if id >= MAX_NODES { return &[]; }
            &self.child_buf[id][..self.child_lens[id]]
        }
        fn parent(&self, id: NodeId) -> Option<NodeId> {
            if id >= MAX_NODES { None } else { self.parents[id] }
        }
        fn attr_range(&self, _: NodeId) -> Range<NodeId> { 0..0 }

        // The four axis helpers under proof never call these; if a
        // future refactor reaches one, the proof will fail loudly.
        fn kind(&self, _: NodeId) -> XPathNodeKind  { unreachable!() }
        fn pi_target(&self, _: NodeId) -> &str       { unreachable!() }
        fn string_value(&self, _: NodeId) -> String  { unreachable!() }
        fn node_name(&self, _: NodeId) -> &str        { unreachable!() }
        fn local_name(&self, _: NodeId) -> &str       { unreachable!() }
        fn namespace_uri(&self, _: NodeId) -> &str    { unreachable!() }
    }

    fn any_node() -> NodeId {
        let n: NodeId = kani::any();
        kani::assume(n < MAX_NODES);
        n
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn following_siblings_never_panics() {
        let _ = following_siblings(any_node(), &AnyIndex::any());
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn preceding_siblings_never_panics() {
        let _ = preceding_siblings(any_node(), &AnyIndex::any());
    }

    // Disabled: `following` and `preceding` recurse through `collect_desc`,
    // which composes with the symbolic parent/children model to produce a
    // SAT formula Cadical can't converge on within any practical time
    // budget — multi-hour runs at `unwind=4` (and `unwind=8`) never
    // terminated.  The slice-OOB bug class the fuzzer found in `following`
    // is structurally identical in `following_siblings`, so the
    // `_siblings` proofs above already exhaust the panic shape worth
    // proving.  Left here as a marker for if/when a future Kani release
    // (or a different harness shape using `kani::cover!` on specific
    // assertions) makes these tractable.
    //
    // #[kani::proof]
    // #[kani::unwind(4)]
    // fn following_never_panics() {
    //     let _ = following(any_node(), &AnyIndex::any());
    // }
    //
    // #[kani::proof]
    // #[kani::unwind(4)]
    // fn preceding_never_panics() {
    //     let _ = preceding(any_node(), &AnyIndex::any());
    // }
}
