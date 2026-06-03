//! A small subset of XPath 1.0 sized for fast per-node matching.
//!
//! This is the engine behind libxml2's `xmlPattern` family — a stripped
//! XPath dialect that the SAX/pull-parser side of libxml2 uses for
//! questions like "does this node match `//book[@id]`?" during a
//! traversal.  It's much faster than firing the full XPath engine
//! per node, and the grammar covers most real-world streaming
//! selectors.
//!
//! # Grammar
//!
//! ```text
//! Pattern    := Branch ('|' Branch)*
//! Branch     := '/'?  Step ('/' Step | '//' Step)*
//! Step       := AxisStep Predicate*
//! AxisStep   := '@' NCName
//!             | '@' '*'
//!             | NCName
//!             | '*'
//! Predicate  := '[' PredExpr ']'
//! PredExpr   := Integer                     -- positional, 1-based
//!             | 'last' '(' ')'              -- last position
//!             | '@' NCName                  -- attribute exists
//!             | '@' NCName '=' Literal      -- attribute equals
//! Literal    := '"' …no " … '"' | "'" …no ' … "'"
//! ```
//!
//! Not supported (intentional — fall outside libxml2's pattern subset):
//! XPath function calls in predicates beyond `last()`, other axes
//! (`parent::`, `ancestor::`, …), numeric expressions, variable
//! references.  Patterns that use them won't compile.
//!
//! # Two evaluation modes
//!
//! - **Backward walk**: [`Pattern::matches`] — given a node with parent
//!   pointers, walk upward through ancestors checking each step.
//!   Random access; works on any DOM tree.
//! - **Forward streaming**: [`Pattern::streaming`] — TODO.  An NFA-shaped
//!   state machine that tracks pattern match progress as SAX-style
//!   events flow past, without needing parent pointers.

use sup_xml_tree::dom::{Node, NodeKind};

/// A compiled libxml2-flavour pattern.  Match nodes via
/// [`Self::matches`].
#[derive(Debug, Clone)]
pub struct Pattern {
    /// One or more `|`-separated branches.  A node matches the pattern
    /// iff it matches at least one branch.
    branches: Vec<Branch>,
}

#[derive(Debug, Clone)]
struct Branch {
    /// Steps in right-to-left order: `steps[0]` matches the candidate
    /// node; `steps[i]` (for `i > 0`) matches the ancestor reached
    /// from `steps[i-1]` via `links[i-1]`.
    steps: Vec<Step>,
    /// Length is `steps.len() - 1`.  `links[i]` says how step `i`
    /// connects to step `i+1` (one parent up, or any ancestor up).
    links: Vec<Link>,
    /// True iff the branch begins with `/`.  Anchored to the document
    /// root — after consuming every step, the cursor must be the
    /// document node.
    absolute: bool,
}

#[derive(Debug, Clone)]
struct Step {
    test:       Test,
    predicates: Vec<Predicate>,
}

#[derive(Debug, Clone)]
enum Test {
    /// `foo` — element name match.
    Element(String),
    /// `*` — any element.
    AnyElement,
    /// `@foo` — attribute name match.  Step targets an attribute node.
    Attribute(String),
    /// `@*` — any attribute.
    AnyAttribute,
}

#[derive(Debug, Clone)]
enum Predicate {
    /// `[N]` — 1-based positional, evaluated against same-name siblings.
    Position(usize),
    /// `[last()]` — last among same-name siblings.
    Last,
    /// `[@foo]` — attribute exists.
    AttrExists(String),
    /// `[@foo='x']` or `[@foo="x"]` — attribute equals literal.
    AttrEquals(String, String),
}

#[derive(Debug, Clone, Copy)]
enum Link {
    /// `/` — the next step must match the immediate parent.
    Parent,
    /// `//` — the next step must match some ancestor (any distance).
    Ancestor,
}

impl Pattern {
    /// Compile a pattern source string into a matcher.
    ///
    /// Returns `Err` on syntax errors or grammar features outside the
    /// libxml2 pattern subset.
    pub fn compile(src: &str) -> Result<Self, String> {
        let mut p = Parser::new(src);
        let pat = p.parse_pattern()?;
        p.skip_ws();
        if !p.at_eof() {
            return Err(format!("unexpected trailing input near {:?}", &p.src[p.pos..]));
        }
        Ok(pat)
    }

    /// Does `node` match this pattern?  Walks upward via parent
    /// pointers, so the node must be attached to a tree.
    pub fn matches(&self, node: &Node<'_>) -> bool {
        self.branches.iter().any(|b| b.matches(node))
    }
}

// ── matching ────────────────────────────────────────────────────────────

impl Branch {
    fn matches(&self, node: &Node<'_>) -> bool {
        // `cursor` starts on the candidate node.  After matching step 0
        // we use links[0] to move up to the node matching step 1, etc.
        let Some(mut cursor) = Some(node) else { return false; };

        for (i, step) in self.steps.iter().enumerate() {
            // Step's test applies to whatever the cursor currently points at.
            if !step.test.matches(cursor) {
                return false;
            }
            if !predicates_hold(&step.predicates, cursor) {
                return false;
            }

            // After the rightmost step there's no further link.
            let Some(link) = self.links.get(i) else { break; };

            cursor = match link {
                Link::Parent => match cursor.parent.get() {
                    Some(p) => p,
                    None    => return false,
                },
                Link::Ancestor => {
                    // We need an ancestor that satisfies the NEXT
                    // step's test+predicates.  Walk upward until we
                    // find one (or run out of ancestors).
                    let Some(next_step) = self.steps.get(i + 1) else {
                        return false;
                    };
                    let mut found = None;
                    let mut up = cursor.parent.get();
                    while let Some(anc) = up {
                        if next_step.test.matches(anc) && predicates_hold(&next_step.predicates, anc) {
                            found = Some(anc);
                            break;
                        }
                        up = anc.parent.get();
                    }
                    match found {
                        Some(a) => {
                            // We already matched the test+predicates;
                            // skip the test step in the next iteration.
                            // Easiest way: short-circuit by overwriting
                            // cursor and continuing.
                            //
                            // But the outer for loop will re-test in
                            // the next iteration, which is correct AND
                            // already true.  No duplicate work hazard.
                            a
                        }
                        None => return false,
                    }
                }
            };
        }

        if self.absolute {
            // After matching the leftmost step, the cursor's parent
            // must be the document root — i.e. the leftmost step is at
            // depth 1.  Equivalently: cursor has no element parent.
            match cursor.parent.get() {
                None    => true,
                Some(p) => matches!(p.kind, NodeKind::Document),
            }
        } else {
            true
        }
    }
}

impl Test {
    fn matches(&self, node: &Node<'_>) -> bool {
        match self {
            Test::Element(name) => {
                matches!(node.kind, NodeKind::Element) && node.name() == name.as_str()
            }
            Test::AnyElement => matches!(node.kind, NodeKind::Element),
            // Under the `c-abi` layout an `xmlAttr*` is passed as an
            // `xmlNode*` carrying `NodeKind::Attribute` (offset-8 `type`),
            // and `parent` points at the owning element — so an attribute
            // step matches it directly and the parent-walk continues
            // upward.  In the pure-Rust DOM attributes are never nodes, so
            // these arms are simply never satisfied there.
            Test::Attribute(name) => {
                matches!(node.kind, NodeKind::Attribute) && node.name() == name.as_str()
            }
            Test::AnyAttribute => matches!(node.kind, NodeKind::Attribute),
        }
    }
}

fn predicates_hold(preds: &[Predicate], node: &Node<'_>) -> bool {
    preds.iter().all(|p| predicate_holds(p, node))
}

fn predicate_holds(pred: &Predicate, node: &Node<'_>) -> bool {
    match pred {
        Predicate::Position(want) => sibling_position(node) == Some(*want),
        Predicate::Last => {
            let pos = sibling_position(node);
            let cnt = sibling_count(node);
            matches!((pos, cnt), (Some(p), Some(c)) if p == c)
        }
        Predicate::AttrExists(name) => node.attributes().any(|a| a.name() == name.as_str()),
        Predicate::AttrEquals(name, val) => node
            .attributes()
            .any(|a| a.name() == name.as_str() && a.value() == val.as_str()),
    }
}

/// 1-based position of `node` among siblings of the same kind+name.
fn sibling_position(node: &Node<'_>) -> Option<usize> {
    let parent = node.parent.get()?;
    let target_name = node.name();
    let mut idx = 0usize;
    for sib in parent.children() {
        if !matches!(sib.kind, NodeKind::Element) { continue; }
        if sib.name() != target_name { continue; }
        idx += 1;
        if std::ptr::eq(sib as *const _, node as *const _) {
            return Some(idx);
        }
    }
    None
}

/// Count of same-name element siblings (inclusive of `node`).
fn sibling_count(node: &Node<'_>) -> Option<usize> {
    let parent = node.parent.get()?;
    let target_name = node.name();
    let mut n = 0usize;
    for sib in parent.children() {
        if !matches!(sib.kind, NodeKind::Element) { continue; }
        if sib.name() != target_name { continue; }
        n += 1;
    }
    Some(n)
}

// ── parsing ─────────────────────────────────────────────────────────────

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self { Self { src, pos: 0 } }

    fn at_eof(&self) -> bool { self.pos >= self.src.len() }

    fn peek(&self) -> Option<char> { self.src[self.pos..].chars().next() }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() { self.bump(); } else { break; }
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.src[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            true
        } else { false }
    }

    fn parse_pattern(&mut self) -> Result<Pattern, String> {
        let mut branches = vec![self.parse_branch()?];
        loop {
            self.skip_ws();
            if !self.eat("|") { break; }
            branches.push(self.parse_branch()?);
        }
        Ok(Pattern { branches })
    }

    fn parse_branch(&mut self) -> Result<Branch, String> {
        self.skip_ws();
        let absolute_or_descendant_root = self.peek() == Some('/');
        let mut leading_descendant = false;
        if absolute_or_descendant_root {
            self.bump();
            if self.peek() == Some('/') {
                self.bump();
                leading_descendant = true;
            }
        }
        // Source-order steps (leftmost first).
        let mut steps_lr: Vec<Step> = vec![self.parse_step()?];
        // Inter-step links, source-order.  links_lr[i] connects steps_lr[i] to steps_lr[i+1].
        let mut links_lr: Vec<Link> = Vec::new();
        loop {
            self.skip_ws();
            if !self.eat("/") {
                break;
            }
            let link = if self.peek() == Some('/') { self.bump(); Link::Ancestor } else { Link::Parent };
            links_lr.push(link);
            steps_lr.push(self.parse_step()?);
        }

        // Reverse so steps[0] is the rightmost (matches candidate).
        steps_lr.reverse();
        let mut links: Vec<Link> = links_lr.into_iter().rev().collect();

        // A leading `//` means the leftmost step is reached from the
        // doc root via Ancestor, but since we're walking backward this
        // is equivalent to "no constraint on what's above the leftmost
        // step."  Drop the absolute flag in that case.
        let absolute = if leading_descendant {
            false
        } else {
            absolute_or_descendant_root
        };

        // Validate: any attribute step must be the rightmost (libxml2
        // pattern grammar restriction — attributes can't have child
        // steps below them).
        if steps_lr.len() > 1 {
            for s in &steps_lr[1..] {
                if matches!(s.test, Test::Attribute(_) | Test::AnyAttribute) {
                    return Err("attribute step must be the rightmost step".into());
                }
            }
        }

        // Belt-and-braces: trim trailing Link that has no destination
        // (shouldn't happen if parser is correct).
        links.truncate(steps_lr.len().saturating_sub(1));

        Ok(Branch { steps: steps_lr, links, absolute })
    }

    fn parse_step(&mut self) -> Result<Step, String> {
        self.skip_ws();
        let test = if self.peek() == Some('@') {
            self.bump();
            if self.eat("*") { Test::AnyAttribute }
            else {
                let n = self.parse_ncname()?;
                Test::Attribute(n)
            }
        } else if self.eat("*") {
            Test::AnyElement
        } else {
            let n = self.parse_ncname()?;
            Test::Element(n)
        };
        let mut predicates = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() != Some('[') { break; }
            predicates.push(self.parse_predicate()?);
        }
        Ok(Step { test, predicates })
    }

    fn parse_ncname(&mut self) -> Result<String, String> {
        // Accepts NCName (ASCII-friendly form for the libxml2 pattern
        // subset).  Permissive: letter | '_' to start, then word chars
        // plus '-'.  Also accept `prefix:local` (treated as a single
        // name for matching).
        let start = self.pos;
        let first = match self.peek() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => c,
            _ => return Err(format!("expected NCName at offset {}", self.pos)),
        };
        self.bump();
        let _ = first;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == ':' {
                self.bump();
            } else { break; }
        }
        Ok(self.src[start..self.pos].to_string())
    }

    fn parse_predicate(&mut self) -> Result<Predicate, String> {
        if !self.eat("[") { return Err("expected '['".into()); }
        self.skip_ws();
        let pred = if self.peek().is_some_and(|c| c.is_ascii_digit()) {
            let start = self.pos;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() { self.bump(); } else { break; }
            }
            let n: usize = self.src[start..self.pos].parse().map_err(|e| format!("bad position: {e}"))?;
            Predicate::Position(n)
        } else if self.eat("last") {
            self.skip_ws();
            if !self.eat("(") || { self.skip_ws(); !self.eat(")") } {
                return Err("expected 'last()'".into());
            }
            Predicate::Last
        } else if self.peek() == Some('@') {
            self.bump();
            let name = self.parse_ncname()?;
            self.skip_ws();
            if self.eat("=") {
                self.skip_ws();
                let val = self.parse_string_literal()?;
                Predicate::AttrEquals(name, val)
            } else {
                Predicate::AttrExists(name)
            }
        } else {
            return Err(format!("unsupported predicate at offset {}", self.pos));
        };
        self.skip_ws();
        if !self.eat("]") { return Err("expected ']'".into()); }
        Ok(pred)
    }

    fn parse_string_literal(&mut self) -> Result<String, String> {
        let q = self.bump().ok_or("expected quoted string")?;
        if q != '"' && q != '\'' {
            return Err(format!("expected quote, got {q:?}"));
        }
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == q { break; }
            self.bump();
        }
        let s = self.src[start..self.pos].to_string();
        if !self.eat(&q.to_string()) {
            return Err("unterminated string literal".into());
        }
        Ok(s)
    }
}

// ── tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_str, ParseOptions};

    fn parse(s: &str) -> sup_xml_tree::dom::Document {
        parse_str(s, &ParseOptions::default()).unwrap()
    }

    fn root<'a>(d: &'a sup_xml_tree::dom::Document) -> &'a Node<'a> {
        d.root()
    }

    fn nth_child<'a>(parent: &'a Node<'a>, i: usize) -> &'a Node<'a> {
        parent.children().nth(i).unwrap()
    }

    #[test]
    fn compile_basic_shapes() {
        for src in [
            "foo",
            "*",
            "@id",
            "@*",
            "foo/bar",
            "foo//bar",
            "//book",
            "/catalog/book",
            "foo[1]",
            "foo[@id]",
            "foo[@id='x']",
            "foo[last()]",
            "a | b | c",
        ] {
            Pattern::compile(src).unwrap_or_else(|e| panic!("failed on {src:?}: {e}"));
        }
    }

    #[test]
    fn rejects_unsupported() {
        for src in ["foo[contains(@id,'x')]", "parent::*", "..", "foo[bar=1]"] {
            assert!(Pattern::compile(src).is_err(), "should reject {src:?}");
        }
    }

    #[test]
    fn match_simple_element() {
        let d = parse("<catalog><book/></catalog>");
        let book = nth_child(root(&d), 0);
        assert!(Pattern::compile("book").unwrap().matches(book));
        assert!(!Pattern::compile("catalog").unwrap().matches(book));
    }

    #[test]
    fn match_child_chain() {
        let d = parse("<catalog><book/></catalog>");
        let book = nth_child(root(&d), 0);
        assert!(Pattern::compile("catalog/book").unwrap().matches(book));
        assert!(!Pattern::compile("library/book").unwrap().matches(book));
    }

    #[test]
    fn match_descendant() {
        let d = parse("<r><a><b><c/></b></a></r>");
        let c = nth_child(nth_child(nth_child(root(&d), 0), 0), 0);
        assert!(Pattern::compile("//c").unwrap().matches(c));
        assert!(Pattern::compile("r//c").unwrap().matches(c));
        assert!(Pattern::compile("a//c").unwrap().matches(c));
        assert!(!Pattern::compile("a/c").unwrap().matches(c));
    }

    #[test]
    fn match_wildcard() {
        let d = parse("<r><a/></r>");
        let a = nth_child(root(&d), 0);
        assert!(Pattern::compile("*").unwrap().matches(a));
        assert!(Pattern::compile("r/*").unwrap().matches(a));
    }

    #[test]
    fn match_absolute() {
        let d = parse("<r><a/></r>");
        let r = root(&d);
        let a = nth_child(r, 0);
        assert!(Pattern::compile("/r").unwrap().matches(r));
        assert!(Pattern::compile("/r/a").unwrap().matches(a));
        // Absolute path: candidate's ancestry must reach the doc root with
        // no extra ancestors above the leftmost step.
        assert!(!Pattern::compile("/a").unwrap().matches(a));
    }

    #[test]
    fn match_position_predicate() {
        let d = parse("<catalog><book/><book/><book/></catalog>");
        let books: Vec<_> = root(&d).children().collect();
        assert!(Pattern::compile("book[1]").unwrap().matches(books[0]));
        assert!(!Pattern::compile("book[1]").unwrap().matches(books[1]));
        assert!(Pattern::compile("book[2]").unwrap().matches(books[1]));
        assert!(Pattern::compile("book[last()]").unwrap().matches(books[2]));
        assert!(!Pattern::compile("book[last()]").unwrap().matches(books[0]));
    }

    #[test]
    fn match_attr_predicate() {
        let d = parse(r#"<catalog><book id="b1"/><book/></catalog>"#);
        let books: Vec<_> = root(&d).children().collect();
        assert!(Pattern::compile("book[@id]").unwrap().matches(books[0]));
        assert!(!Pattern::compile("book[@id]").unwrap().matches(books[1]));
        assert!(Pattern::compile(r#"book[@id="b1"]"#).unwrap().matches(books[0]));
        assert!(!Pattern::compile(r#"book[@id="b2"]"#).unwrap().matches(books[0]));
    }

    #[test]
    fn match_union() {
        let d = parse("<r><a/><b/></r>");
        let a = nth_child(root(&d), 0);
        let b = nth_child(root(&d), 1);
        let p = Pattern::compile("a | b").unwrap();
        assert!(p.matches(a));
        assert!(p.matches(b));
        let p2 = Pattern::compile("a | c").unwrap();
        assert!(p2.matches(a));
        assert!(!p2.matches(b));
    }
}
