---
title: XPath
description: Evaluate XPath expressions against a parsed document — XPath 1.0 by default, XPath 2.0+ via the static-context flag.
---

## Versions

SupXML ships **XPath 1.0** by default and **XPath 2.0** (including
substantial XPath 3.0+ syntax — `let`, `for`, quantified expressions,
inline functions, maps, arrays, `||`, simple-map `!`) gated behind a
static-context flag. XPath 2.0+ is what the XSLT 2.0 engine runs
internally; you can opt in directly via:

```rust
use sup_xml::{XPathContext, XPathOptions};

let ctx = XPathContext::new_with(&doc, XPathOptions {
    xpath_2_0: true,
    ..Default::default()
});
let n: f64 = ctx.eval_num("count(for $x in 1 to 10 return $x * $x)")?;
assert_eq!(n, 10.0);
```

The default `XPathContext::new` stays on 1.0 so existing pipelines see
no behaviour change.

## Basic queries

```rust
use sup_xml::{parse_str, XPathContext};

let doc = parse_str("<catalog><book id='b1'/><book id='b2'/></catalog>", &Default::default())?;
let ctx = XPathContext::new(&doc);

let n: usize  = ctx.eval_count("/catalog/book")?;             // node-set size
let total: f64 = ctx.eval_num("count(/catalog/book)")?;       // numeric result
let s: String = ctx.eval_str("string(/catalog/book[1]/@id)")?;
let b: bool   = ctx.eval_bool("/catalog/book")?;              // non-empty node-set?
```

`eval_count` returns the size of a node-set, so pass the path itself
(`/catalog/book`), not `count(...)`. For the numeric value of a
`count(...)` / `sum(...)` expression, use `eval_num`.

## Namespaces

Prefix→URI bindings ride on an `XPathBindingsBuilder`, which you pass to
`eval_with` (the binding-aware evaluation entry point). Every prefix used
in the expression must be bound, or evaluation returns an
undefined-prefix error before any tree walk.

```rust
use sup_xml::{XPathContext, XPathValue, XPathBindingsBuilder};

let ctx = XPathContext::new(&doc);

let mut bindings = XPathBindingsBuilder::new();
bindings.namespace("ns", "http://example.com/ns");

// `eval_with(expr, context_node, bindings)` — 0 is the document root.
let id = match ctx.eval_with("string(/ns:catalog/ns:book/@id)", 0, &bindings)? {
    XPathValue::String(s) => s,
    _ => String::new(),
};
```

The same builder also carries `bind_variable` (`$name`) and `function`
(custom `ns:fn(...)` extensions) — see `XPathBindingsBuilder`.

## Custom context node

By default an expression is evaluated against the document root. To
evaluate relative to a specific node, get its `NodeId` from a prior
node-set result and pass it to `eval_at`:

```rust
use sup_xml::{XPathContext, XPathValue};

let ctx = XPathContext::new(&doc);

let book = match ctx.eval("/catalog/book[1]")? {
    XPathValue::NodeSet(nodes) => nodes[0],   // NodeId
    _ => return Ok(()),
};
let id = match ctx.eval_at("string(@id)", book)? {
    XPathValue::String(s) => s,
    _ => String::new(),
};
```

## Functions

XPath 1.0 functions plus all of EXSLT (math, date, string, set) are
always available — no registration needed.

```rust
ctx.eval_str("math:max(/values/v)")?;          // EXSLT math
ctx.eval_str("str:tokenize('a,b,c', ',')")?;   // EXSLT strings
ctx.eval_str("date:year()")?;                  // EXSLT dates
```

## Reusing the context

`XPathContext` is reusable across queries against the same document — keep
one around instead of creating a new one per query.

## Bounding untrusted expressions

Every evaluation is capped by a step budget (default 20M) so an
adversarial expression aborts instead of spinning. If you evaluate XPath
that comes from users, tighten it via `XPathOptions::max_eval_steps`:

```rust
let opts = XPathOptions { max_eval_steps: 1_000_000, ..Default::default() };
let ctx = XPathContext::new_with(&doc, opts);
```

XPath authored by your own code needs no change. See
[Security → XPath evaluation budget](/reference/security/#xpath-evaluation-budget).
