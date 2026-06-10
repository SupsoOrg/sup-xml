/// XPath 1.0 abstract syntax tree.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Ancestor,
    AncestorOrSelf,
    Attribute,
    Child,
    Descendant,
    DescendantOrSelf,
    Following,
    FollowingSibling,
    Namespace,
    Parent,
    Preceding,
    PrecedingSibling,
    Self_,
}

impl Axis {
    pub fn is_reverse(&self) -> bool {
        matches!(
            self,
            Axis::Ancestor | Axis::AncestorOrSelf | Axis::Preceding | Axis::PrecedingSibling
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeTest {
    /// node() — any node on the axis
    AnyNode,
    /// text()
    Text,
    /// comment()
    Comment,
    /// processing-instruction() or processing-instruction('target')
    PI(Option<String>),
    /// document-node() — matches document nodes (XPath 2.0 §2.5.4).
    /// Distinct from `AnyNode` so XSLT `match="document-node()"`
    /// patterns don't accidentally cover element / text / etc.
    ///
    /// The optional inner test carries the element name test of a
    /// `document-node(element(N))` / `document-node(element(*))` form
    /// (XPath 2.0 §2.5.4.3): the node matches only when it is a
    /// document node whose document element satisfies that test.
    /// `None` is the bare `document-node()`.
    Document(Option<Box<NodeTest>>),
    /// * — any element/attribute
    Wildcard,
    /// prefix:* — any element/attribute in this namespace prefix
    PrefixWildcard(String),
    /// *:localname — any namespace, specific local name (XPath 2.0
    /// §2.5.5.3 — `WildcardName ::= NCName ":" "*" | "*" ":" NCName`).
    LocalNameOnly(String),
    /// localname (no prefix)
    LocalName(String),
    /// prefix:localname
    QName(String, String),
    /// Unprefixed name that resolves through the XSLT 2.0
    /// `xpath-default-namespace` attribute (XSLT 2.0 §5.1.1).
    /// The XPath parser produces `LocalName`; the XSLT compiler
    /// rewrites those into this variant when an ancestor of the
    /// stylesheet element declares a non-empty default URI for
    /// element name tests.  Stored as expanded URI + local part so
    /// runtime matching is a pair-compare without binding lookup.
    DefaultNamespaceName { uri: String, local: String },
}

#[derive(Debug, Clone)]
pub struct Step {
    pub axis: Axis,
    pub node_test: NodeTest,
    pub predicates: Vec<Expr>,
    /// XPath 2.0 `StepExpr ::= AxisStep | FilterExpr`.  When
    /// `Some`, the step is a FilterExpr (a primary expression
    /// like a function call or parenthesised expression) that
    /// produces its own sequence per input node — `axis` and
    /// `node_test` are unused for the eval but kept at their
    /// default (`Self_` / `AnyNode`) so existing code that
    /// destructures `Step` directly still type-checks.
    /// `path/key('x', 'y')` and `path/(expr)` parse into this
    /// shape.
    pub filter: Option<Box<Expr>>,
}

#[derive(Debug, Clone)]
pub enum LocationPath {
    Absolute(Vec<Step>),
    Relative(Vec<Step>),
}

#[derive(Debug, Clone)]
pub enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    /// General comparison (`=`, `!=`, `<`, `>`, `<=`, `>=`) — existential
    /// over the cartesian product of the two operand sequences
    /// (XPath 2.0 §3.5.2).
    Eq(Box<Expr>, Box<Expr>),
    Ne(Box<Expr>, Box<Expr>),
    Lt(Box<Expr>, Box<Expr>),
    Gt(Box<Expr>, Box<Expr>),
    Le(Box<Expr>, Box<Expr>),
    Ge(Box<Expr>, Box<Expr>),
    /// Value comparison (`eq`, `ne`, `lt`, `gt`, `le`, `ge`) — single-
    /// item operands, returns the empty sequence when either side is
    /// empty, raises a type error when either side has more than one
    /// item (XPath 2.0 §3.5.1).
    ValueEq(Box<Expr>, Box<Expr>),
    ValueNe(Box<Expr>, Box<Expr>),
    ValueLt(Box<Expr>, Box<Expr>),
    ValueGt(Box<Expr>, Box<Expr>),
    ValueLe(Box<Expr>, Box<Expr>),
    ValueGe(Box<Expr>, Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
    Mod(Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    Union(Box<Expr>, Box<Expr>),
    Path(LocationPath),
    /// Primary expression with optional predicates and additional steps.
    FilterPath {
        primary: Box<Expr>,
        predicates: Vec<Expr>,
        steps: Vec<Step>,
    },
    FunctionCall(String, Vec<Expr>),
    Variable(String),
    Literal(String),
    /// Integer literal — no `.` and no exponent (`42`), an `xs:integer`.
    /// A literal too large for `i64` is lexed as a [`Expr::Decimal`].
    Integer(i64),
    /// Decimal literal — a `.` but no exponent (`3.14`), an `xs:decimal`.
    /// Carries an exact [`rust_decimal::Decimal`] parsed from the
    /// source lexical form (so `0.1` is 1/10 exactly, not the f64
    /// nearest neighbour).  Stringifies in decimal form, never
    /// scientific.
    Decimal(rust_decimal::Decimal),
    /// Numeric literal with an exponent (`1.5e0`) — an `xs:double`.
    /// In a 2.0 host this evaluates to a typed double so it takes the
    /// F&O scientific string form.
    Double(f64),
    /// XPath 2.0 `if (cond) then a else b`.  Both branches are
    /// `ExprSingle` — already parsed as full expressions.
    IfThenElse {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    /// XPath 2.0 `for $v in seq return body`, with chained bindings
    /// (`for $a in A, $b in B return ...`) flattened into the
    /// `bindings` list in source order.
    For {
        bindings: Vec<(String, Expr)>,
        body:     Box<Expr>,
    },
    /// XPath 3.0 `let $v := expr return body`, with chained bindings
    /// (`let $a := A, $b := B return ...`) flattened into the
    /// `bindings` list in source order.  Each binding is evaluated
    /// once and is visible to later bindings and the body.
    Let {
        bindings: Vec<(String, Expr)>,
        body:     Box<Expr>,
    },
    /// XPath 2.0 range `m to n` — yields the integer sequence m, m+1,
    /// …, n (inclusive).  Empty when m > n.  Atomic non-integer
    /// operands round to integers before the range materialises.
    Range(Box<Expr>, Box<Expr>),
    /// XPath 3.0 simple-map operator `E1 ! E2` — evaluates `E2` with
    /// each item of `E1` as the context item; concatenates results
    /// in iteration order (no document-order sort).  Distinct from
    /// `/` in that the right-hand side need not be a node-step:
    /// `(1, 2, 3) ! (. * 2)` yields (2, 4, 6).
    SimpleMap(Box<Expr>, Box<Expr>),
    /// XPath 2.0 node-comparison `$a << $b` — true iff `$a`
    /// precedes `$b` in document order.  Empty operands yield
    /// the empty sequence.
    NodeBefore(Box<Expr>, Box<Expr>),
    /// XPath 2.0 node-comparison `$a >> $b` — node-after.
    NodeAfter(Box<Expr>, Box<Expr>),
    /// XPath 2.0 node-comparison `$a is $b` — true iff both operands
    /// are the same node.  Each operand must atomise to at most one
    /// node; an empty operand yields the empty sequence (§3.5.3).
    NodeIs(Box<Expr>, Box<Expr>),
    /// XPath 2.0 parenthesised sequence literal `(a, b, c)` — at least
    /// two elements (a singleton is just a parenthesised expression).
    /// Evaluation concatenates each item; atomics become synthetic
    /// text nodes so the result is uniformly a NodeSet.
    Sequence(Vec<Expr>),
    /// XPath 2.0 quantified expression `some $v in seq satisfies test`
    /// / `every $v in seq satisfies test`.  Boolean result: any /
    /// all items of the sequence satisfy the predicate.
    Quantified {
        kind:     QuantifierKind,
        bindings: Vec<(String, Expr)>,
        test:     Box<Expr>,
    },
    /// XPath 2.0 `lhs idiv rhs` — integer quotient with truncation
    /// towards zero (XPath 2.0 § 3.4).  Distinct from `div`, which
    /// always produces a float.
    IDiv(Box<Expr>, Box<Expr>),
    /// XPath 2.0 `lhs intersect rhs` — node-set intersection in
    /// document order.
    Intersect(Box<Expr>, Box<Expr>),
    /// XPath 2.0 `lhs except rhs` — nodes in `lhs` not present in
    /// `rhs`, document order preserved.
    Except(Box<Expr>, Box<Expr>),
    /// XPath 2.0 `expr instance of SequenceType` — boolean predicate.
    InstanceOf(Box<Expr>, SequenceType),
    /// XPath 2.0 `expr cast as SingleType` — value conversion;
    /// raises a runtime error when the source value can't be cast.
    CastAs(Box<Expr>, SingleType),
    /// XPath 3.1 `try { TryExpr } catch <NameTest>* { CatchExpr } …`
    /// — evaluate `body`; on dynamic error, walk catches and
    /// evaluate the first whose name-tests match the caught
    /// error's QName.  Inside the catch handler, `$err:code` /
    /// `$err:description` / etc. are bound to the error's
    /// metadata.
    TryCatch {
        body:    Box<Expr>,
        catches: Vec<XPathCatch>,
    },
    /// XPath 2.0 `expr castable as SingleType` — boolean predicate
    /// for "can this value cast without error".
    CastableAs(Box<Expr>, SingleType),
    /// XPath 2.0 `expr treat as SequenceType` — assertion that the
    /// runtime value already conforms; raises an error otherwise.
    TreatAs(Box<Expr>, SequenceType),
    /// Synthetic — not produced by the XPath parser.  The XSLT
    /// compiler wraps each top-level expression whose static context
    /// declares a non-codepoint `[xsl:]default-collation` so the
    /// runtime can install that URI on a thread-local before
    /// evaluating `inner`.  Value-comparison operators (`eq`, `ne`,
    /// `lt`, …) consult that thread-local when both operands are
    /// strings/untyped — XPath 2.0 §3.5.2 says they use the static
    /// default collation in that case.
    WithDefaultCollation(String, Box<Expr>),
    /// Synthetic — not produced by the XPath parser.  The XSLT
    /// compiler wraps each top-level expression that sits in an
    /// XPath-1.0 backwards-compatibility scope (a `[xsl:]version="1.0"`
    /// ancestor inside a 2.0 stylesheet, XSLT 2.0 §3.8).  The runtime
    /// installs a thread-local flag before evaluating `inner` so the
    /// XPath-1.0-compat conversion rules (XPath 2.0 §B.1) apply:
    /// arithmetic operands are atomised to xs:double, and `to`-range
    /// bounds use the first item of a sequence.
    BackwardsCompat(Box<Expr>),
    /// XPath 3.1 §3.11.1 map constructor `map { k1: v1, k2: v2, … }`.
    /// Each entry is `(key-expr, value-expr)`; keys evaluate to a
    /// single atomic value, values to an arbitrary sequence.
    MapConstructor(Vec<(Expr, Expr)>),
    /// XPath 3.1 §3.11.2 array constructors — `[ a, b, c ]` (square:
    /// one member per comma-separated expression) or `array { … }`
    /// (curly: one member per item of the contained sequence).
    ArrayConstructor { members: Vec<Expr>, square: bool },
    /// XPath 3.1 §3.11.3 postfix lookup `E ? K` — indexes into the map
    /// or array produced by `E`.
    Lookup(Box<Expr>, LookupKey),
    /// XPath 3.1 unary lookup `? K` — indexes into the context item.
    UnaryLookup(LookupKey),
    /// XPath 3.1 §3.12 inline function `function($p, …) { body }`.
    /// Parameter types and the return type are accepted but not
    /// enforced; only the parameter names matter for binding.
    InlineFunction {
        params: Vec<String>,
        /// Declared signature (parameter and return types, `item()*` where
        /// omitted) — used for function subtyping in `instance of`.
        sig:    Box<FunctionSig>,
        body:   Box<Expr>,
    },
    /// XPath 3.1 §3.1.6 named function reference `name#arity`.
    NamedFunctionRef { name: String, arity: usize },
    /// XPath 3.1 §3.2.2 dynamic function call `F(args)` where `F` is
    /// an expression yielding a function item.  The `?` placeholder
    /// (partial application) appears as [`Expr::Placeholder`] in args.
    DynamicCall { func: Box<Expr>, args: Vec<Expr> },
    /// XPath 3.1 partial-application argument placeholder (`?`).
    Placeholder,
}

/// The key selector of an XPath 3.1 lookup expression (`?K`).
#[derive(Debug, Clone)]
pub enum LookupKey {
    /// `?*` — all values of the map / all members of the array.
    Wildcard,
    /// `?name` — the entry whose key is the string `name`.
    Name(String),
    /// `?123` — integer key (map) or 1-based position (array).
    Integer(i64),
    /// `?(expr)` — the key(s) computed by the parenthesised expression.
    Expr(Box<Expr>),
}

/// XPath 2.0 SequenceType — limited to what we recognise.  Anything
/// outside `xs:string` / `xs:integer` / `xs:decimal` / `xs:double`
/// / `xs:boolean` / `xs:date` / `xs:dateTime` / `xs:time` /
/// `xs:anyAtomicType` / `xs:anyURI` (atomic) or `item()` / `node()`
/// / `element()` / `attribute()` / `text()` / `comment()` /
/// `processing-instruction()` / `document-node()` (kind) is parsed
/// but flagged as unsupported at eval time.
#[derive(Debug, Clone, PartialEq)]
pub struct SequenceType {
    pub item:        ItemType,
    pub occurrence:  Occurrence,
}

/// The signature of a specific function test `function(T1, …, Tn) as R`
/// (XPath 3.1 §2.5.4.3).  Drives the function-subtyping rules applied by
/// `instance of` / `treat as`: the parameter types are contravariant and
/// the return type is covariant.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionSig {
    pub params: Vec<SequenceType>,
    pub ret:    SequenceType,
}

/// `SingleType` — a `SequenceType` with implicit `?` (one or zero).
/// Used in `cast as` / `castable as`.
pub type SingleType = SequenceType;

#[derive(Debug, Clone, PartialEq)]
pub enum ItemType {
    /// `item()` — any item.
    Any,
    /// `xs:foo` atomic type test.  Stored verbatim as the local name
    /// (with `xs:` stripped); eval uses the name to pick a string-to-
    /// value coercion.
    Atomic(String),
    /// `node()` — any node.
    AnyNode,
    /// Specific kind tests with optional name.  Name is `None` for
    /// the bare-paren form (e.g. `element()`); `Some` for
    /// `element(foo)`.
    Element(Option<String>),
    Attribute(Option<String>),
    Text,
    Comment,
    PI(Option<String>),
    Document,
    /// Function test (XPath 3.1 §2.5.4.3).  `None` is `function(*)` (any
    /// function item); `Some` carries a specific `function(T1, …, Tn) as R`
    /// signature, matched by function subtyping.
    Function(Option<Box<FunctionSig>>),
    /// `empty-sequence()` — matches only the empty sequence.  As an
    /// item test it matches no individual item; the empty case is
    /// admitted by the cardinality check alone.
    EmptySequence,
}

/// Occurrence indicator (XPath 2.0 § 2.5.3) attached to a SequenceType.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occurrence {
    /// Exactly one (default — no indicator).
    One,
    /// `?` — zero or one.
    Optional,
    /// `+` — one or more.
    OneOrMore,
    /// `*` — zero or more.
    ZeroOrMore,
}

/// Distinguishes `some` (∃) from `every` (∀) quantified expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantifierKind { Some, Every }

/// One catch clause of an [`Expr::TryCatch`].  The grammar is
/// `"catch" NameTest ("|" NameTest)* "{" Expr "}"`, with `"*"`
/// allowed as a catch-all in place of a specific QName.
#[derive(Debug, Clone)]
pub struct XPathCatch {
    /// QName matchers from the `catch` clause's name list.  An
    /// empty list (or a single `Any`) is catch-all.
    pub matchers: Vec<CatchNameTest>,
    /// Body expression to evaluate when this clause matches.
    pub body:     Expr,
}

/// One name-test in an XPath try/catch `catch` clause name list.
#[derive(Debug, Clone)]
pub enum CatchNameTest {
    /// `*` — any error matches.
    Any,
    /// `*:NCName` — matches any-namespace, specific local name.
    LocalNameOnly(String),
    /// `prefix:*` — namespace wildcard.
    PrefixWildcard(String),
    /// `prefix:local` / `local` — fully-qualified.
    QName { prefix: Option<String>, local: String },
}

/// Maximum depth of predicates-within-predicates anywhere in `expr`.
///
/// A single `Step` carrying `[p]` predicates is depth 1; if `p` itself
/// contains a `Path` whose steps carry their own `[q]` predicates,
/// that's depth 2; and so on.  This is the *semantic* nesting that
/// drives the evaluator's N^k blow-up — distinct from the parser's
/// grammar-production recursion depth (which the parser already bounds
/// with `MAX_PARSE_DEPTH`).
///
/// Used to reject pathological inputs like
/// `//*[//*[//*[//*[//*[.='x']]]]]` at parse time, before the
/// evaluator's step budget burns ~500k charges to reach the same
/// conclusion at much higher latency.
pub fn max_predicate_nesting(expr: &Expr) -> u32 {
    fn expr_depth(e: &Expr) -> u32 {
        match e {
            Expr::Or(a, b)  | Expr::And(a, b)
            | Expr::Eq(a, b) | Expr::Ne(a, b)
            | Expr::Lt(a, b) | Expr::Gt(a, b) | Expr::Le(a, b) | Expr::Ge(a, b)
            | Expr::ValueEq(a, b) | Expr::ValueNe(a, b)
            | Expr::ValueLt(a, b) | Expr::ValueGt(a, b)
            | Expr::ValueLe(a, b) | Expr::ValueGe(a, b)
            | Expr::Add(a, b) | Expr::Sub(a, b)
            | Expr::Mul(a, b) | Expr::Div(a, b) | Expr::Mod(a, b)
            | Expr::Union(a, b)
            | Expr::NodeBefore(a, b) | Expr::NodeAfter(a, b) | Expr::NodeIs(a, b) => expr_depth(a).max(expr_depth(b)),
            Expr::Neg(a) => expr_depth(a),
            Expr::Path(p) => path_depth(p),
            Expr::FilterPath { primary, predicates, steps } => {
                let primary_d = expr_depth(primary);
                let pred_d = predicates.iter()
                    .map(|p| 1 + expr_depth(p))
                    .max()
                    .unwrap_or(0);
                let step_d = steps.iter().map(step_depth).max().unwrap_or(0);
                primary_d.max(pred_d).max(step_d)
            }
            Expr::FunctionCall(_, args) =>
                args.iter().map(expr_depth).max().unwrap_or(0),
            Expr::Variable(_) | Expr::Literal(_)
            | Expr::Integer(_) | Expr::Decimal(_) | Expr::Double(_) => 0,
            Expr::IfThenElse { cond, then_branch, else_branch } =>
                expr_depth(cond)
                    .max(expr_depth(then_branch))
                    .max(expr_depth(else_branch)),
            Expr::For { bindings, body } | Expr::Let { bindings, body } => {
                let body_d = expr_depth(body);
                bindings.iter().map(|(_, e)| expr_depth(e)).max().unwrap_or(0).max(body_d)
            }
            Expr::Range(a, b)     => expr_depth(a).max(expr_depth(b)),
            Expr::SimpleMap(a, b) => expr_depth(a).max(expr_depth(b)),
            Expr::Sequence(items) =>
                items.iter().map(expr_depth).max().unwrap_or(0),
            Expr::Quantified { bindings, test, .. } => {
                let t = expr_depth(test);
                bindings.iter().map(|(_, e)| expr_depth(e)).max().unwrap_or(0).max(t)
            }
            Expr::IDiv(a, b) | Expr::Intersect(a, b) | Expr::Except(a, b)
                => expr_depth(a).max(expr_depth(b)),
            Expr::InstanceOf(a, _) | Expr::CastAs(a, _) | Expr::CastableAs(a, _)
            | Expr::TreatAs(a, _) => expr_depth(a),
            Expr::TryCatch { body, catches } => {
                let b = expr_depth(body);
                catches.iter().map(|c| expr_depth(&c.body)).max().unwrap_or(0).max(b)
            }
            Expr::WithDefaultCollation(_, inner) => expr_depth(inner),
            Expr::BackwardsCompat(inner) => expr_depth(inner),
            Expr::MapConstructor(entries) => entries.iter()
                .map(|(k, v)| expr_depth(k).max(expr_depth(v))).max().unwrap_or(0),
            Expr::ArrayConstructor { members, .. } =>
                members.iter().map(expr_depth).max().unwrap_or(0),
            Expr::Lookup(base, key) => expr_depth(base).max(lookup_key_depth(key)),
            Expr::UnaryLookup(key) => lookup_key_depth(key),
            Expr::InlineFunction { body, .. } => expr_depth(body),
            Expr::NamedFunctionRef { .. } | Expr::Placeholder => 0,
            Expr::DynamicCall { func, args } => expr_depth(func)
                .max(args.iter().map(expr_depth).max().unwrap_or(0)),
        }
    }
    fn lookup_key_depth(k: &LookupKey) -> u32 {
        match k {
            LookupKey::Expr(e) => expr_depth(e),
            _ => 0,
        }
    }
    fn step_depth(s: &Step) -> u32 {
        s.predicates.iter()
            .map(|p| 1 + expr_depth(p))
            .max()
            .unwrap_or(0)
    }
    fn path_depth(p: &LocationPath) -> u32 {
        let steps = match p {
            LocationPath::Absolute(s) | LocationPath::Relative(s) => s,
        };
        steps.iter().map(step_depth).max().unwrap_or(0)
    }
    expr_depth(expr)
}
