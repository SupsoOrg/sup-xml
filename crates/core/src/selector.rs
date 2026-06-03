//! Minimal CSS-subset selector for streaming-parser emit modes.
//!
//! # Supported syntax
//!
//! | Syntax              | Meaning                                                |
//! |---------------------|--------------------------------------------------------|
//! | `tag`               | element name matches `tag`                             |
//! | `*`                 | any element                                            |
//! | `.class`            | `class` attribute contains the token (HTML semantics)  |
//! | `#id`               | `id` attribute equals                                  |
//! | `[attr]`            | element has the attribute                              |
//! | `[attr=value]`      | attribute equals (unquoted, `"…"`, or `'…'`)           |
//! | `A B`               | `B` is a descendant of `A`                             |
//! | `A > B`             | `B` is a direct child of `A`                           |
//!
//! Simple selectors combine: `tag.class.other#id[attr][a=b]`.  Combinators
//! chain: `feed > entry[type="article"]`.
//!
//! # Intentionally unsupported
//!
//! * `:pseudo-classes` like `:nth-child` — most need lookahead the streaming
//!   parser cannot provide.
//! * `~` and `+` sibling combinators — same reason.
//! * Attribute operators other than `=`: no `~=`, `|=`, `^=`, `$=`, `*=`.
//! * Comma-separated multi-selectors (`a, b`).
//!
//! # Anchoring
//!
//! Like real CSS, selectors are **not anchored to the document root**.  `item`
//! matches an `<item>` element anywhere, including nested ones.  Use a
//! path-based emit mode on the streaming parser when you need exact
//! root-anchored matching with guaranteed memory bounding.

use sup_xml_tree::dom::Node as ArenaNode;

/// A parsed selector.  Construct with [`Selector::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Part {
    sel: SimpleSelector,
    /// How to relate this part to the next-leftward part.  `Start` on `parts[0]`.
    combinator_to_left: Combinator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Combinator {
    Start,
    Descendant,
    Child,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SimpleSelector {
    /// `None` for `*` or when the simple selector starts with `.`/`#`/`[`.
    tag:     Option<String>,
    id:      Option<String>,
    classes: Vec<String>,
    attrs:   Vec<AttrFilter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttrFilter {
    Has(String),
    Equals(String, String),
}

/// Error parsing a selector string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseSelectorError {
    message: String,
}

impl std::fmt::Display for ParseSelectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid selector: {}", self.message)
    }
}

impl std::error::Error for ParseSelectorError {}

impl Selector {
    /// Parse a selector string.  See the module docs for the supported subset.
    pub fn parse(input: &str) -> Result<Self, ParseSelectorError> {
        let mut p = Parser { input, pos: 0 };
        p.skip_ws();
        let parts = p.parse_chain()?;
        if parts.is_empty() {
            return Err(p.err("empty selector"));
        }
        Ok(Selector { parts })
    }

    /// Match this selector against the just-closed arena node `popped`,
    /// given its ancestor chain `stack` (root at index 0).
    ///
    /// `stack` is the ancestor chain by reference: `stack[0]` is the root,
    /// `stack[stack.len() - 1]` is the immediate parent of `popped`.  All
    /// references typically come from one [`sup_xml_tree::dom::Document`]
    /// but the matcher doesn't enforce this — it only reads `name` and
    /// `attributes()`.
    pub fn matches(&self, popped: &ArenaNode<'_>, stack: &[&ArenaNode<'_>]) -> bool {
        let n = self.parts.len();
        if n == 0 || !self.parts[n - 1].sel.matches(popped) {
            return false;
        }
        if n == 1 {
            return true;
        }
        self.match_left(n - 1, stack.len(), stack)
    }

    fn match_left(&self, right_idx: usize, cursor: usize, stack: &[&ArenaNode<'_>]) -> bool {
        let comb     = self.parts[right_idx].combinator_to_left;
        let left_idx = right_idx - 1;
        let left_sel = &self.parts[left_idx].sel;
        match comb {
            Combinator::Child => {
                if cursor == 0 { return false; }
                let pos = cursor - 1;
                if !left_sel.matches(stack[pos]) { return false; }
                if left_idx == 0 { return true; }
                self.match_left(left_idx, pos, stack)
            }
            Combinator::Descendant => {
                for pos in (0..cursor).rev() {
                    if !left_sel.matches(stack[pos]) { continue; }
                    if left_idx == 0 { return true; }
                    if self.match_left(left_idx, pos, stack) { return true; }
                }
                false
            }
            Combinator::Start => unreachable!("Start only at parts[0]"),
        }
    }
}

impl SimpleSelector {
    /// Same rules as CSS — tag-name equality, id/class/attr filters — but
    /// iterates the linked-list attribute list.  Non-elements never match.
    fn matches(&self, el: &ArenaNode<'_>) -> bool {
        if !el.is_element() { return false; }
        if let Some(t) = &self.tag {
            if el.name() != t.as_str() { return false; }
        }
        if let Some(id) = &self.id {
            let has = el.attributes().any(|a| a.name() == "id" && a.value() == *id);
            if !has { return false; }
        }
        for class in &self.classes {
            let has = el.attributes().any(|a| {
                a.name() == "class"
                    && a.value().split_ascii_whitespace().any(|tok| tok == class)
            });
            if !has { return false; }
        }
        for filter in &self.attrs {
            let ok = match filter {
                AttrFilter::Has(n)       => el.attributes().any(|a| a.name() == n.as_str()),
                AttrFilter::Equals(n, v) => el.attributes().any(|a| a.name() == n.as_str() && a.value() == *v),
            };
            if !ok { return false; }
        }
        true
    }
}

// ── parser ───────────────────────────────────────────────────────────────────

struct Parser<'a> {
    input: &'a str,
    pos:   usize,
}

impl<'a> Parser<'a> {
    fn err(&self, msg: impl Into<String>) -> ParseSelectorError {
        ParseSelectorError { message: format!("{} (at offset {})", msg.into(), self.pos) }
    }

    fn peek(&self) -> Option<char> { self.input[self.pos..].chars().next() }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    /// Skip whitespace; return whether any was skipped (relevant for the
    /// descendant combinator, which IS whitespace).
    fn skip_ws(&mut self) -> bool {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_whitespace() { self.bump(); } else { break; }
        }
        self.pos != start
    }

    fn parse_chain(&mut self) -> Result<Vec<Part>, ParseSelectorError> {
        let mut parts = Vec::new();
        if self.peek().is_none() {
            return Ok(parts);
        }
        let sel = self.parse_simple()?;
        parts.push(Part { sel, combinator_to_left: Combinator::Start });
        loop {
            let had_ws = self.skip_ws();
            match self.peek() {
                None => break,
                Some('>') => {
                    self.bump();
                    self.skip_ws();
                    let sel = self.parse_simple()?;
                    parts.push(Part { sel, combinator_to_left: Combinator::Child });
                }
                Some(_) => {
                    if !had_ws {
                        return Err(self.err("expected combinator (` ` or `>`) or end of selector"));
                    }
                    let sel = self.parse_simple()?;
                    parts.push(Part { sel, combinator_to_left: Combinator::Descendant });
                }
            }
        }
        Ok(parts)
    }

    fn parse_simple(&mut self) -> Result<SimpleSelector, ParseSelectorError> {
        let mut sel        = SimpleSelector::default();
        let mut had_prefix = false;  // saw `*` or a tag name
        match self.peek() {
            Some('*') => { self.bump(); had_prefix = true; }
            Some(c) if is_ident_start(c) => {
                sel.tag = Some(self.parse_ident()?);
                had_prefix = true;
            }
            _ => {}
        }
        loop {
            match self.peek() {
                Some('.') => {
                    self.bump();
                    sel.classes.push(self.parse_ident()?);
                }
                Some('#') => {
                    self.bump();
                    if sel.id.is_some() {
                        return Err(self.err("multiple #id selectors"));
                    }
                    sel.id = Some(self.parse_ident()?);
                }
                Some('[') => {
                    self.bump();
                    sel.attrs.push(self.parse_attr_filter()?);
                }
                _ => break,
            }
        }
        if !had_prefix && sel.id.is_none() && sel.classes.is_empty() && sel.attrs.is_empty() {
            return Err(self.err("expected element name, `*`, `.class`, `#id`, or `[attr]`"));
        }
        Ok(sel)
    }

    fn parse_ident(&mut self) -> Result<String, ParseSelectorError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if is_ident_continue(c) { self.bump(); } else { break; }
        }
        if start == self.pos {
            return Err(self.err("expected identifier"));
        }
        Ok(self.input[start..self.pos].to_owned())
    }

    fn parse_attr_filter(&mut self) -> Result<AttrFilter, ParseSelectorError> {
        self.skip_ws();
        let name = self.parse_ident()?;
        self.skip_ws();
        match self.peek() {
            Some(']') => { self.bump(); Ok(AttrFilter::Has(name)) }
            Some('=') => {
                self.bump();
                self.skip_ws();
                let value = self.parse_attr_value()?;
                self.skip_ws();
                if self.peek() != Some(']') {
                    return Err(self.err("expected `]`"));
                }
                self.bump();
                Ok(AttrFilter::Equals(name, value))
            }
            _ => Err(self.err("expected `=` or `]` after attribute name")),
        }
    }

    fn parse_attr_value(&mut self) -> Result<String, ParseSelectorError> {
        let quote = match self.peek() {
            Some('"') | Some('\'') => self.bump().unwrap(),
            _ => return self.parse_ident(),
        };
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == quote { break; }
            self.bump();
        }
        if self.peek() != Some(quote) {
            return Err(self.err("unterminated attribute value"));
        }
        let value = self.input[start..self.pos].to_owned();
        self.bump();
        Ok(value)
    }
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    // XML names can contain `:` (namespace prefix), `-`, digits.  `.` is the
    // class-selector delimiter so we exclude it; tags with `.` in the name are
    // not addressable via this subset (use `[name=…]` if you need that).
    c == '_' || c == '-' || c == ':' || c.is_alphanumeric()
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sup_xml_tree::dom::{DocumentBuilder, Node};

    /// Build a standalone arena element + return the builder so refs stay alive.
    fn make_el<'a>(b: &'a DocumentBuilder, name: &str, attrs: &[(&str, &str)]) -> &'a Node<'a> {
        let n  = b.alloc_str(name);
        let el = b.new_element(n);
        for (an, av) in attrs {
            let attr = b.new_attribute(b.alloc_str(an), b.alloc_str(av));
            b.append_attribute(el, attr);
        }
        el
    }

    // ── parser ─────────────────────────────────────────────────────────────

    #[test]
    fn parse_simple_tag() {
        let s = Selector::parse("foo").unwrap();
        assert_eq!(s.parts.len(), 1);
        assert_eq!(s.parts[0].sel.tag.as_deref(), Some("foo"));
    }

    #[test]
    fn parse_wildcard() {
        let s = Selector::parse("*").unwrap();
        assert_eq!(s.parts[0].sel.tag, None);
    }

    #[test]
    fn parse_class_id_attrs() {
        let s = Selector::parse(r#"div.foo.bar#main[hidden][type="x"]"#).unwrap();
        let p = &s.parts[0].sel;
        assert_eq!(p.tag.as_deref(), Some("div"));
        assert_eq!(p.id.as_deref(), Some("main"));
        assert_eq!(p.classes, vec!["foo".to_string(), "bar".into()]);
        assert_eq!(p.attrs, vec![
            AttrFilter::Has("hidden".into()),
            AttrFilter::Equals("type".into(), "x".into()),
        ]);
    }

    #[test]
    fn parse_child_combinator() {
        let s = Selector::parse("a > b > c").unwrap();
        assert_eq!(s.parts.len(), 3);
        assert_eq!(s.parts[0].combinator_to_left, Combinator::Start);
        assert_eq!(s.parts[1].combinator_to_left, Combinator::Child);
        assert_eq!(s.parts[2].combinator_to_left, Combinator::Child);
    }

    #[test]
    fn parse_descendant_combinator() {
        let s = Selector::parse("a b").unwrap();
        assert_eq!(s.parts.len(), 2);
        assert_eq!(s.parts[1].combinator_to_left, Combinator::Descendant);
    }

    #[test]
    fn parse_mixed_combinators() {
        let s = Selector::parse("a b > c").unwrap();
        assert_eq!(s.parts[1].combinator_to_left, Combinator::Descendant);
        assert_eq!(s.parts[2].combinator_to_left, Combinator::Child);
    }

    #[test]
    fn parse_no_space_around_child_combinator() {
        let s = Selector::parse("a>b").unwrap();
        assert_eq!(s.parts.len(), 2);
        assert_eq!(s.parts[1].combinator_to_left, Combinator::Child);
    }

    #[test]
    fn parse_namespaced_name_with_colon() {
        let s = Selector::parse("atom:entry").unwrap();
        assert_eq!(s.parts[0].sel.tag.as_deref(), Some("atom:entry"));
    }

    #[test]
    fn parse_unquoted_attr_value() {
        let s = Selector::parse("[rel=next]").unwrap();
        assert_eq!(s.parts[0].sel.attrs[0], AttrFilter::Equals("rel".into(), "next".into()));
    }

    #[test]
    fn parse_single_quoted_attr_value() {
        let s = Selector::parse("[a='hello world']").unwrap();
        assert_eq!(s.parts[0].sel.attrs[0], AttrFilter::Equals("a".into(), "hello world".into()));
    }

    #[test]
    fn parse_errors() {
        assert!(Selector::parse("").is_err());
        assert!(Selector::parse("   ").is_err());
        assert!(Selector::parse("a,b").is_err(), "comma not supported");
        // Note: `:` is treated as a namespace separator (e.g. `atom:entry`),
        // so `a:hover` parses as the tag name `a:hover`.  Pseudo-classes are
        // unsupported in the sense of "no semantic, just a tag-name char."
        assert!(Selector::parse("[a").is_err(), "unterminated attr");
        assert!(Selector::parse("[a=").is_err(), "missing value");
        assert!(Selector::parse("[a='unterminated").is_err());
        assert!(Selector::parse("#one#two").is_err(), "multiple ids");
        assert!(Selector::parse("> foo").is_err(), "leading combinator");
    }

    // ── simple match ───────────────────────────────────────────────────────

    #[test]
    fn match_tag() {
        let s = Selector::parse("item").unwrap();
        let b = DocumentBuilder::new();
        assert!( s.matches(make_el(&b, "item",  &[]), &[]));
        assert!(!s.matches(make_el(&b, "entry", &[]), &[]));
    }

    #[test]
    fn match_wildcard_any() {
        let s = Selector::parse("*").unwrap();
        let b = DocumentBuilder::new();
        assert!(s.matches(make_el(&b, "anything", &[]), &[]));
    }

    #[test]
    fn match_class_token() {
        let s = Selector::parse(".bar").unwrap();
        let b = DocumentBuilder::new();
        assert!( s.matches(make_el(&b, "div", &[("class", "foo bar baz")]), &[]));
        assert!(!s.matches(make_el(&b, "div", &[("class", "foo baz")]),     &[]));
    }

    #[test]
    fn match_id_equals() {
        let s = Selector::parse("#main").unwrap();
        let b = DocumentBuilder::new();
        assert!( s.matches(make_el(&b, "div", &[("id", "main")]),  &[]));
        assert!(!s.matches(make_el(&b, "div", &[("id", "main2")]), &[]));
    }

    #[test]
    fn match_attr_has_and_equals() {
        let has = Selector::parse("[hidden]").unwrap();
        let eq  = Selector::parse("[rel=next]").unwrap();
        let b = DocumentBuilder::new();
        assert!( has.matches(make_el(&b, "a", &[("hidden", "")]),  &[]));
        assert!(!has.matches(make_el(&b, "a", &[("rel", "next")]), &[]));
        assert!( eq.matches (make_el(&b, "a", &[("rel", "next")]), &[]));
        assert!(!eq.matches (make_el(&b, "a", &[("rel", "prev")]), &[]));
    }

    // ── combinator match ───────────────────────────────────────────────────

    #[test]
    fn match_child_combinator() {
        let s = Selector::parse("channel > item").unwrap();
        let b = DocumentBuilder::new();
        let rss     = make_el(&b, "rss",     &[]);
        let channel = make_el(&b, "channel", &[]);
        let item    = make_el(&b, "item",    &[]);
        assert!( s.matches(item, &[rss, channel]));
        // wrong parent
        assert!(!s.matches(item, &[rss]));
    }

    #[test]
    fn match_descendant_combinator() {
        let s = Selector::parse("rss item").unwrap();
        let b = DocumentBuilder::new();
        let rss     = make_el(&b, "rss",     &[]);
        let channel = make_el(&b, "channel", &[]);
        let item    = make_el(&b, "item",    &[]);
        assert!(s.matches(item, &[rss, channel]));

        let feed     = make_el(&b, "feed",    &[]);
        let channel2 = make_el(&b, "channel", &[]);
        assert!(!s.matches(item, &[feed, channel2]));
    }

    #[test]
    fn match_descendant_backtracks() {
        // `div p` against <div><div><p/></div></div> — outer div, inner div, p.
        let s = Selector::parse("div p").unwrap();
        let b = DocumentBuilder::new();
        let outer_div = make_el(&b, "div", &[]);
        let inner_div = make_el(&b, "div", &[]);
        let p         = make_el(&b, "p",   &[]);
        assert!(s.matches(p, &[outer_div, inner_div]));
    }

    #[test]
    fn match_mixed_combinators() {
        let s = Selector::parse("feed > entry > title").unwrap();
        let b = DocumentBuilder::new();
        let feed  = make_el(&b, "feed",  &[]);
        let entry = make_el(&b, "entry", &[]);
        let title = make_el(&b, "title", &[]);
        assert!( s.matches(title, &[feed, entry]));

        let section = make_el(&b, "section", &[]);
        assert!(!s.matches(title, &[feed, section]));
    }

    #[test]
    fn match_complex_simple_selector_compound() {
        let s = Selector::parse(r#"entry[type="article"].published"#).unwrap();
        let b = DocumentBuilder::new();
        assert!( s.matches(
            make_el(&b, "entry", &[("type", "article"), ("class", "published")]), &[]));
        assert!(!s.matches(
            make_el(&b, "entry", &[("type", "draft"),   ("class", "published")]), &[]));
        assert!(!s.matches(
            make_el(&b, "entry", &[("type", "article"), ("class", "unpublished")]), &[]));
    }

    #[test]
    fn non_element_never_matches() {
        // `*` shouldn't match a text node.
        let s = Selector::parse("*").unwrap();
        let b = DocumentBuilder::new();
        let text = b.new_text(b.alloc_str("hello"));
        assert!(!s.matches(text, &[]));
    }
}
