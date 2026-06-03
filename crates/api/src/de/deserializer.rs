//! `XmlDeserializer` — drives the serde Visitor API from `XmlReader` events.

use std::borrow::Cow;

use serde::de::{self, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess, SeqAccess, VariantAccess, Visitor};

use sup_xml_core::{EventInto, XmlReader};

use super::{DeError, DeOptions};

// ── lookahead-buffered event source ──────────────────────────────────────────

/// One pending event, decoded into a borrow-friendly form so the reader is
/// free for the next call while a buffered event is alive.
#[derive(Debug)]
enum Pending<'src> {
    Start { name: Cow<'src, str>, attrs: Vec<(Cow<'src, str>, Cow<'src, str>)> },
    End,
    Text(Cow<'src, str>),
    CData(Cow<'src, str>),
    Eof,
}

/// Low-level serde XML deserializer.
///
/// Most callers should use the convenience entry points
/// [`from_str`](super::from_str) / [`from_bytes`](super::from_bytes)
/// rather than constructing this directly.  Use `XmlDeserializer` when you
/// want to drive the serde [`Deserializer`](serde::de::Deserializer) trait
/// against XML events yourself — for example, plugging into a custom
/// visitor or chaining with a `seed`-based `DeserializeSeed` flow.
///
/// Holds an [`XmlReader`] and a one-event lookahead so peeking is cheap.
pub struct XmlDeserializer<'de> {
    reader:    XmlReader<'de>,
    opts:      DeOptions,
    /// Single-event lookahead.
    lookahead: Option<Pending<'de>>,
    /// Reusable scratch buffer for eager attribute reads.
    attr_buf:  Vec<sup_xml_core::Attr<'de>>,
}

impl<'de> XmlDeserializer<'de> {
    /// Build a deserializer over a string slice with default options.
    pub fn from_str(input: &'de str) -> Self {
        Self::from_str_opts(input, DeOptions::default())
    }

    /// Build a deserializer over a string slice with caller-supplied
    /// [`DeOptions`].  The options are forwarded to the underlying
    /// [`XmlReader`](crate::XmlReader) and govern naming conventions
    /// (`@` prefix, `$text` / `$value` field names) and behaviour
    /// (`xsi:nil`, unknown-field handling).
    pub fn from_str_opts(input: &'de str, opts: DeOptions) -> Self {
        let reader = XmlReader::from_str(input).with_options(opts.parse.clone());
        Self {
            reader,
            opts,
            lookahead: None,
            attr_buf: Vec::new(),
        }
    }

    // ── event source ─────────────────────────────────────────────────────────

    fn pull(&mut self) -> Result<Pending<'de>, DeError> {
        loop {
            let ev = self.reader.next_into(&mut self.attr_buf)?;
            match ev {
                EventInto::Comment(_) | EventInto::Pi { .. } => continue,
                EventInto::StartElement { name } => {
                    let attrs = self.attr_buf.drain(..)
                        // a.name is `&'de str` (lazy reader); wrap in
                        // Cow::Borrowed to keep `Pending::Start`'s
                        // shape unchanged for the rest of the
                        // deserializer.
                        .map(|a| (Cow::Borrowed(a.name), a.value))
                        .collect();
                    return Ok(Pending::Start { name, attrs });
                }
                EventInto::EndElement { .. }   => return Ok(Pending::End),
                EventInto::Text(t)             => return Ok(Pending::Text(t)),
                EventInto::CData(s)            => return Ok(Pending::CData(s)),
                EventInto::EntityRef { name }  => {
                    // EntityRef events only appear under
                    // `resolve_entities: false`.  Typed deserialization
                    // can't make sense of unresolved `&name;` markers
                    // — they have no text value to coerce into a Rust
                    // type — so surface a clear error instead of
                    // silently dropping data.  Configure the parser
                    // with `resolve_entities: true` (the default) for
                    // typed deserialization to work.
                    return Err(DeError::msg(format!(
                        "typed deserialization requires `resolve_entities: true`; \
                         encountered unresolved entity reference `&{name};`"
                    )));
                }
                EventInto::Eof                 => return Ok(Pending::Eof),
            }
        }
    }

    fn peek(&mut self) -> Result<&Pending<'de>, DeError> {
        if self.lookahead.is_none() {
            let p = self.pull()?;
            self.lookahead = Some(p);
        }
        Ok(self.lookahead.as_ref().unwrap())
    }

    fn advance(&mut self) -> Result<Pending<'de>, DeError> {
        if let Some(p) = self.lookahead.take() {
            return Ok(p);
        }
        self.pull()
    }

    /// At top level, discard whitespace-only text events.
    fn skip_leading_ws(&mut self) -> Result<(), DeError> {
        loop {
            match self.peek()? {
                Pending::Text(t) if t.chars().all(char::is_whitespace) => {
                    let _ = self.advance()?;
                }
                _ => return Ok(()),
            }
        }
    }

    // ── content collection ───────────────────────────────────────────────────

    /// After a Start has been consumed, gather all text/CDATA in its body
    /// into one string, consuming the matching EndElement.  Errors on a
    /// nested element.
    fn read_text_body(&mut self) -> Result<String, DeError> {
        let mut out = String::new();
        loop {
            match self.advance()? {
                Pending::Text(t) | Pending::CData(t) => out.push_str(&t),
                Pending::End => return Ok(out),
                Pending::Start { name, .. } => return Err(DeError::msg(format!(
                    "expected scalar content but found nested element <{}>", name
                ))),
                Pending::Eof => return Err(DeError::msg(
                    "unexpected EOF while reading element text content"
                )),
            }
        }
    }

    /// Skip the body of an entered element (including nested elements)
    /// through its matching EndElement.
    fn skip_body(&mut self) -> Result<(), DeError> {
        let mut depth = 1usize;
        while depth > 0 {
            match self.advance()? {
                Pending::Start { .. } => depth += 1,
                Pending::End   => depth -= 1,
                Pending::Eof          => return Err(DeError::msg("unexpected EOF in skip_body")),
                _ => {}
            }
        }
        Ok(())
    }

    /// Pull a scalar element's text body — used by every scalar deserializer.
    fn scalar_text(&mut self) -> Result<String, DeError> {
        self.skip_leading_ws()?;
        match self.advance()? {
            Pending::Start { .. } => self.read_text_body(),
            Pending::Text(t) | Pending::CData(t) => Ok(t.into_owned()),
            other => Err(DeError::msg(format!(
                "expected scalar element, got {other:?}"
            ))),
        }
    }
}

// ── serde Deserializer impl ──────────────────────────────────────────────────

macro_rules! deserialize_int {
    ($($method:ident => $visit:ident => $ty:ty),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
            let s = self.scalar_text()?;
            let n: $ty = s.trim().parse()
                .map_err(|e| DeError::msg(format!("invalid {}: {e}", stringify!($ty))))?;
            v.$visit(n)
        }
    )*};
}

impl<'de, 'a> de::Deserializer<'de> for &'a mut XmlDeserializer<'de> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, _v: V) -> Result<V::Value, DeError> {
        Err(DeError::msg(
            "deserialize_any is not supported — XML lacks the type metadata for it; \
             use a typed Deserialize impl instead",
        ))
    }

    // ── string scalars ───────────────────────────────────────────────────────

    fn deserialize_str<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        self.deserialize_string(v)
    }

    fn deserialize_string<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        let s = self.scalar_text()?;
        v.visit_string(s)
    }

    // ── numeric scalars ──────────────────────────────────────────────────────

    deserialize_int! {
        deserialize_i8   => visit_i8   => i8,
        deserialize_i16  => visit_i16  => i16,
        deserialize_i32  => visit_i32  => i32,
        deserialize_i64  => visit_i64  => i64,
        deserialize_i128 => visit_i128 => i128,
        deserialize_u8   => visit_u8   => u8,
        deserialize_u16  => visit_u16  => u16,
        deserialize_u32  => visit_u32  => u32,
        deserialize_u64  => visit_u64  => u64,
        deserialize_u128 => visit_u128 => u128,
        deserialize_f32  => visit_f32  => f32,
        deserialize_f64  => visit_f64  => f64,
    }

    fn deserialize_bool<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        let s = self.scalar_text()?;
        match s.trim() {
            "true"  | "1" => v.visit_bool(true),
            "false" | "0" => v.visit_bool(false),
            other => Err(DeError::msg(format!("invalid bool: {other:?}"))),
        }
    }

    fn deserialize_char<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        let s = self.scalar_text()?;
        let mut it = s.chars();
        match (it.next(), it.next()) {
            (Some(c), None) => v.visit_char(c),
            _ => Err(DeError::msg(format!("expected single char, got {s:?}"))),
        }
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        let s = self.scalar_text()?;
        v.visit_byte_buf(s.into_bytes())
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        self.deserialize_bytes(v)
    }

    // ── unit ─────────────────────────────────────────────────────────────────

    fn deserialize_unit<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        self.skip_leading_ws()?;
        match self.advance()? {
            Pending::Start { .. } => { self.skip_body()?; v.visit_unit() }
            Pending::Eof => v.visit_unit(),
            other => Err(DeError::msg(format!("expected element for unit, got {other:?}"))),
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(self, _name: &'static str, v: V)
        -> Result<V::Value, DeError>
    {
        self.deserialize_unit(v)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, v: V)
        -> Result<V::Value, DeError>
    {
        v.visit_newtype_struct(self)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        self.skip_leading_ws()?;
        if let Pending::Start { .. } = self.peek()? {
            let _ = self.advance()?;
            self.skip_body()?;
        }
        v.visit_unit()
    }

    // ── struct ───────────────────────────────────────────────────────────────

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        v: V,
    ) -> Result<V::Value, DeError> {
        self.skip_leading_ws()?;
        let attrs = match self.advance()? {
            Pending::Start { attrs, .. } => attrs,
            other => return Err(DeError::msg(format!(
                "expected element start for struct, got {other:?}"
            ))),
        };
        let map = StructMap::new(self, attrs, fields);
        v.visit_map(map)
    }

    // ── seq ──────────────────────────────────────────────────────────────────

    fn deserialize_seq<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        // Capture the element name from the first peeked StartElement; the
        // sequence yields one item per consecutive same-name element.  This
        // is invoked from inside StructMap's value step, where the next
        // event is the StartElement we yielded the key for.
        self.skip_leading_ws()?;
        let tag: Option<String> = match self.peek()? {
            Pending::Start { name, .. } => Some(name.clone().into_owned()),
            // No matching element — empty seq.
            _ => None,
        };
        v.visit_seq(ElementSeq { de: self, tag })
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, v: V) -> Result<V::Value, DeError> {
        self.deserialize_seq(v)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        v: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_seq(v)
    }

    fn deserialize_map<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        // Same machinery as struct: enter the element, then iterate
        // attrs + child elements, with each element name surfacing as a
        // key.  HashMap<String, T> works out of the box.
        self.skip_leading_ws()?;
        let attrs = match self.advance()? {
            Pending::Start { attrs, .. } => attrs,
            other => return Err(DeError::msg(format!(
                "expected element start for map, got {other:?}"
            ))),
        };
        v.visit_map(StructMap::new(self, attrs, &[]))
    }

    fn deserialize_option<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        self.skip_leading_ws()?;
        // xsi:nil="true" → None.  We check the *peeked* element's attrs;
        // if nil, consume the empty/ignored element and visit_none.
        if self.opts.honor_xsi_nil {
            let nil = matches!(self.peek()?, Pending::Start { attrs, .. } if is_xsi_nil(attrs));
            if nil {
                let _ = self.advance()?;
                self.skip_body()?;
                return v.visit_none();
            }
        }
        // The struct-map layer only yields a key for present elements, so
        // by the time this fires the element is always present.
        v.visit_some(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        v: V,
    ) -> Result<V::Value, DeError> {
        self.skip_leading_ws()?;
        let variant = match self.peek()? {
            Pending::Start { name, .. } => name.clone().into_owned(),
            other => return Err(DeError::msg(format!(
                "expected element start for enum, got {other:?}"
            ))),
        };
        v.visit_enum(EnumElement { de: self, variant })
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        self.deserialize_str(v)
    }
}

// ── element-sequence SeqAccess ───────────────────────────────────────────────

/// Yields one value per consecutive StartElement whose name matches `tag`.
///
/// Stops as soon as the next event is anything else — typically the parent's
/// next child element (different name) or its EndElement.
struct ElementSeq<'a, 'de> {
    de:  &'a mut XmlDeserializer<'de>,
    tag: Option<String>,
}

impl<'de> SeqAccess<'de> for ElementSeq<'_, 'de> {
    type Error = DeError;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T)
        -> Result<Option<T::Value>, DeError>
    {
        let tag = match &self.tag {
            Some(t) => t.as_str(),
            None    => return Ok(None),
        };
        // Skip whitespace-only text between sibling elements.
        loop {
            match self.de.peek()? {
                Pending::Text(t) if t.chars().all(char::is_whitespace) => {
                    let _ = self.de.advance()?;
                }
                Pending::Start { name, .. } if name.as_ref() == tag => {
                    return seed.deserialize(&mut *self.de).map(Some);
                }
                _ => return Ok(None),
            }
        }
    }
}

// ── enum access ──────────────────────────────────────────────────────────────

/// Drives serde's enum machinery from an XML element whose tag names a
/// variant.  `variant_seed` returns the variant name without consuming the
/// XML element; the corresponding `VariantAccess` method then drives the
/// element body.
struct EnumElement<'a, 'de> {
    de:      &'a mut XmlDeserializer<'de>,
    variant: String,
}

impl<'de> EnumAccess<'de> for EnumElement<'_, 'de> {
    type Error   = DeError;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V)
        -> Result<(V::Value, Self::Variant), DeError>
    {
        let de: serde::de::value::StringDeserializer<DeError> =
            self.variant.clone().into_deserializer();
        let value = seed.deserialize(de)?;
        Ok((value, self))
    }
}

impl<'de> VariantAccess<'de> for EnumElement<'_, 'de> {
    type Error = DeError;

    fn unit_variant(self) -> Result<(), DeError> {
        // Consume `<Variant/>`.  Body must be empty (or whitespace).
        match self.de.advance()? {
            Pending::Start { .. } => self.de.skip_body(),
            other => Err(DeError::msg(format!(
                "expected element for unit variant, got {other:?}"
            ))),
        }
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T)
        -> Result<T::Value, DeError>
    {
        // The Start is still queued; T's deserializer consumes the whole
        // element (e.g. <Variant>42</Variant> deserializes the body as an
        // i32, with scalar_text walking through the End tag).
        seed.deserialize(&mut *self.de)
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, _v: V) -> Result<V::Value, DeError> {
        Err(DeError::msg("tuple variants are not supported in v1"))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        v: V,
    ) -> Result<V::Value, DeError> {
        // Recurse into the element as a struct.  The Start is still queued.
        de::Deserializer::deserialize_struct(&mut *self.de, "", fields, v)
    }
}

// ── struct MapAccess ─────────────────────────────────────────────────────────

/// Yields struct keys in order: attributes first (each named `@name`),
/// then child elements as they appear (the element name is the key — or
/// the value-field name, if `$value` is declared and the element name
/// isn't a known field), and finally `$text` if any non-whitespace text
/// was collected.
struct StructMap<'a, 'de> {
    de:                 &'a mut XmlDeserializer<'de>,
    attrs:              std::vec::IntoIter<(Cow<'de, str>, Cow<'de, str>)>,
    pending_attr_value: Option<Cow<'de, str>>,
    in_body:            bool,
    body_done:          bool,
    text_buf:           String,
    text_yielded:       bool,
    text_value_pending: bool,
    /// True once a `$value` key has been yielded so we route the value
    /// path through `ValueSeqDeserializer`.
    value_pending:      bool,
    /// Field names declared on the struct.  Used to decide whether a
    /// child element should yield its element name as the key, or be
    /// batched under `$value`.  Empty when called from `deserialize_map`
    /// (which surfaces every element by name).
    fields:             &'static [&'static str],
    /// True when `$value` is declared on the struct.
    has_value_field:    bool,
    attr_prefix:        char,
    text_field_name:    &'static str,
    value_field_name:   &'static str,
}

impl<'a, 'de> StructMap<'a, 'de> {
    fn new(
        de: &'a mut XmlDeserializer<'de>,
        attrs: Vec<(Cow<'de, str>, Cow<'de, str>)>,
        fields: &'static [&'static str],
    ) -> Self {
        let attr_prefix      = de.opts.attribute_prefix;
        let text_field_name  = de.opts.text_field_name;
        let value_field_name = de.opts.value_field_name;
        let has_value_field  = fields.contains(&value_field_name);
        Self {
            de,
            attrs: attrs.into_iter(),
            pending_attr_value: None,
            in_body: false,
            body_done: false,
            text_buf: String::new(),
            text_yielded: false,
            text_value_pending: false,
            value_pending: false,
            fields,
            has_value_field,
            attr_prefix,
            text_field_name,
            value_field_name,
        }
    }

    /// True if this element name is one of the struct's declared field
    /// names — meaning we surface it as that key rather than batching it
    /// under `$value`.
    fn is_named_field(&self, name: &str) -> bool {
        self.fields.iter().any(|f| *f == name)
    }
}

impl<'de> MapAccess<'de> for StructMap<'_, 'de> {
    type Error = DeError;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K)
        -> Result<Option<K::Value>, DeError>
    {
        // Phase 1: attributes.
        if !self.in_body {
            if let Some((name, value)) = self.attrs.next() {
                self.pending_attr_value = Some(value);
                let key = format!("{}{}", self.attr_prefix, name);
                return seed.deserialize(key.into_deserializer()).map(Some);
            }
            self.in_body = true;
        }

        // Phase 2: walk body events.
        if !self.body_done {
            loop {
                // Decision: surface as $value or by element name?  We need
                // the element name for the check, so peek (without holding
                // the borrow across the seed call).
                let peek_kind = match self.de.peek()? {
                    Pending::End   => PeekKind::End,
                    Pending::Eof          => PeekKind::Eof,
                    Pending::Text(_) | Pending::CData(_) => PeekKind::Text,
                    Pending::Start { name, .. } => PeekKind::Start(name.clone().into_owned()),
                };
                match peek_kind {
                    PeekKind::End => {
                        let _ = self.de.advance()?;
                        self.body_done = true;
                        break;
                    }
                    PeekKind::Eof => {
                        return Err(DeError::msg("unexpected EOF inside struct"));
                    }
                    PeekKind::Text => {
                        match self.de.advance()? {
                            Pending::Text(t) | Pending::CData(t) => self.text_buf.push_str(&t),
                            _ => unreachable!(),
                        }
                    }
                    PeekKind::Start(name) => {
                        if self.has_value_field && !self.is_named_field(&name) {
                            // Heterogeneous element body — yield the
                            // value-field key once; its value is a
                            // sequence collecting *all remaining* body
                            // elements (until our matching End).
                            self.value_pending = true;
                            return seed
                                .deserialize(self.value_field_name.into_deserializer())
                                .map(Some);
                        }
                        return seed.deserialize(name.into_deserializer()).map(Some);
                    }
                }
            }
        }

        // Phase 3: $text.
        if !self.text_yielded && !self.text_buf.trim().is_empty() {
            self.text_yielded = true;
            self.text_value_pending = true;
            return seed.deserialize(self.text_field_name.into_deserializer()).map(Some);
        }

        Ok(None)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V)
        -> Result<V::Value, DeError>
    {
        if let Some(val) = self.pending_attr_value.take() {
            return seed.deserialize(ScalarStrDeserializer { value: val });
        }

        if self.text_value_pending {
            self.text_value_pending = false;
            let text = std::mem::take(&mut self.text_buf);
            return seed.deserialize(ScalarStrDeserializer { value: Cow::Owned(text) });
        }

        if self.value_pending {
            self.value_pending = false;
            // Hand the visitor a deserializer that, when asked for a seq,
            // collects every remaining body element regardless of name.
            return seed.deserialize(ValueSeqDeserializer { de: &mut *self.de });
        }

        // Element value: recurse — the next event is the StartElement we
        // peeked at; the child consumes through its matching EndElement.
        seed.deserialize(&mut *self.de)
    }
}

enum PeekKind {
    Start(String),
    End,
    Text,
    Eof,
}

// ── $value support ───────────────────────────────────────────────────────────

/// Deserializer for a `$value` field.  When asked for a `seq` (the common
/// case), yields each remaining body element until the parent's End — the
/// element name picks the variant for enum item types.
struct ValueSeqDeserializer<'a, 'de> {
    de: &'a mut XmlDeserializer<'de>,
}

impl<'de> de::Deserializer<'de> for ValueSeqDeserializer<'_, 'de> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        // Default to seq behaviour — that's what `$value` is for.
        self.deserialize_seq(v)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        v.visit_seq(AnyElementSeq { de: self.de })
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, v: V) -> Result<V::Value, DeError> {
        self.deserialize_seq(v)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        v: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_seq(v)
    }

    // For non-seq types (e.g. user wrote `$value: SingleBlock`), forward
    // to the underlying deserializer to consume one element.
    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, v: V)
        -> Result<V::Value, DeError>
    {
        v.visit_newtype_struct(self)
    }

    fn deserialize_option<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        v.visit_some(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64
        char str string bytes byte_buf unit unit_struct
        map struct enum identifier ignored_any
    }
}

/// SeqAccess that yields one value per remaining body element (any name)
/// until the parent's matching EndElement is encountered.  Used by
/// `ValueSeqDeserializer`.
struct AnyElementSeq<'a, 'de> {
    de: &'a mut XmlDeserializer<'de>,
}

impl<'de> SeqAccess<'de> for AnyElementSeq<'_, 'de> {
    type Error = DeError;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T)
        -> Result<Option<T::Value>, DeError>
    {
        loop {
            match self.de.peek()? {
                Pending::End | Pending::Eof => return Ok(None),
                Pending::Text(t) | Pending::CData(t) if t.chars().all(char::is_whitespace) => {
                    let _ = self.de.advance()?;
                }
                Pending::Text(_) | Pending::CData(_) => {
                    // Non-whitespace text inside a $value field — drop on
                    // the floor (matches quick-xml's behaviour: $value is
                    // for elements, $text is for text).
                    let _ = self.de.advance()?;
                }
                Pending::Start { .. } => {
                    return seed.deserialize(&mut *self.de).map(Some);
                }
            }
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn is_xsi_nil(attrs: &[(Cow<'_, str>, Cow<'_, str>)]) -> bool {
    attrs.iter().any(|(n, v)|
        n.as_ref() == "xsi:nil" && matches!(v.trim(), "true" | "1")
    )
}

// ── scalar-string Deserializer ───────────────────────────────────────────────

/// Deserializer over a single owned/borrowed string.  Used for attribute
/// values and the `$text` field — neither of which is an XML element.
struct ScalarStrDeserializer<'de> {
    value: Cow<'de, str>,
}

macro_rules! sv_int {
    ($($method:ident => $visit:ident => $ty:ty),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
            let n: $ty = self.value.trim().parse()
                .map_err(|e| DeError::msg(format!("invalid {}: {e}", stringify!($ty))))?;
            v.$visit(n)
        }
    )*};
}

impl<'de> de::Deserializer<'de> for ScalarStrDeserializer<'de> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        match self.value {
            Cow::Borrowed(s) => v.visit_borrowed_str(s),
            Cow::Owned(s)    => v.visit_string(s),
        }
    }

    fn deserialize_str<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        match self.value {
            Cow::Borrowed(s) => v.visit_borrowed_str(s),
            Cow::Owned(s)    => v.visit_string(s),
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        self.deserialize_str(v)
    }

    fn deserialize_bool<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        match self.value.trim() {
            "true"  | "1" => v.visit_bool(true),
            "false" | "0" => v.visit_bool(false),
            other => Err(DeError::msg(format!("invalid bool: {other:?}"))),
        }
    }

    fn deserialize_char<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        let mut it = self.value.chars();
        match (it.next(), it.next()) {
            (Some(c), None) => v.visit_char(c),
            _ => Err(DeError::msg(format!("expected single char, got {:?}", self.value))),
        }
    }

    sv_int! {
        deserialize_i8   => visit_i8   => i8,
        deserialize_i16  => visit_i16  => i16,
        deserialize_i32  => visit_i32  => i32,
        deserialize_i64  => visit_i64  => i64,
        deserialize_i128 => visit_i128 => i128,
        deserialize_u8   => visit_u8   => u8,
        deserialize_u16  => visit_u16  => u16,
        deserialize_u32  => visit_u32  => u32,
        deserialize_u64  => visit_u64  => u64,
        deserialize_u128 => visit_u128 => u128,
        deserialize_f32  => visit_f32  => f32,
        deserialize_f64  => visit_f64  => f64,
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        match self.value {
            Cow::Borrowed(s) => v.visit_borrowed_bytes(s.as_bytes()),
            Cow::Owned(s)    => v.visit_byte_buf(s.into_bytes()),
        }
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        self.deserialize_bytes(v)
    }

    fn deserialize_option<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        v.visit_some(self)
    }

    fn deserialize_unit<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        v.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(self, _name: &'static str, v: V)
        -> Result<V::Value, DeError>
    {
        v.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, v: V)
        -> Result<V::Value, DeError>
    {
        v.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, _v: V) -> Result<V::Value, DeError> {
        Err(DeError::msg("cannot deserialize attribute/text value as a seq"))
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, _v: V) -> Result<V::Value, DeError> {
        Err(DeError::msg("cannot deserialize attribute/text value as a tuple"))
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(self, _n: &'static str, _l: usize, _v: V)
        -> Result<V::Value, DeError>
    {
        Err(DeError::msg("cannot deserialize attribute/text value as a tuple struct"))
    }

    fn deserialize_map<V: Visitor<'de>>(self, _v: V) -> Result<V::Value, DeError> {
        Err(DeError::msg("cannot deserialize attribute/text value as a map"))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        _v: V,
    ) -> Result<V::Value, DeError> {
        Err(DeError::msg("cannot deserialize attribute/text value as a struct"))
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        v: V,
    ) -> Result<V::Value, DeError> {
        v.visit_enum(self.value.into_owned().into_deserializer())
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        self.deserialize_str(v)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, DeError> {
        v.visit_unit()
    }
}
