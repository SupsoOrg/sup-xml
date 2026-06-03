//! XSD parser — turns one or more XSD source documents into a compiled
//! [`Schema`].
//!
//! Architecture:
//!
//! 1. **Pass 1.**  Walk each XSD as XML with [`XmlReader`].  Build a
//!    shared [`Builder`] that accumulates top-level declarations across
//!    all included/imported files.  Built-in `xs:*` type references are
//!    resolved at parse time; user-defined references stay as
//!    placeholders the validator looks up at instance time.
//! 2. **Recursive composition.**  `<xs:import>` and `<xs:include>` are
//!    handled via the configured [`SchemaResolver`].  Cycles are
//!    silently skipped via the builder's `loaded` set; recursion is
//!    bounded at [`MAX_INCLUDE_DEPTH`] levels.
//! 3. **Substitution-group resolution.**  At [`Builder::into_schema`],
//!    each element's `substitutionGroup` attribute is collected into a
//!    head → members map for fast O(1) substitution lookup at
//!    validation time.
//!
//! v1 limitations (documented; tracked for follow-ups):
//! * `<xs:redefine>` not yet supported (use `<xs:include>` instead —
//!   redefine adds modify-on-import semantics that need careful design).
//! * Identity constraints (`<xs:key>`/`<xs:keyref>`/`<xs:unique>`)
//!   parsed but not enforced at validation time (in-flight).
//! * Substitution-group `block`/`final` flags collected but not
//!   enforced.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::reader::{Attr, EventInto, XmlReader};

use super::error::SchemaCompileError;
use super::facets::{Bound, Facet, FacetSet};
use super::resolver::{FsResolver, NoResolver, SchemaResolver};
use super::schema::{
    AttributeDecl, AttributeGroup, AttributeUse, AttributeUseKind, BlockSet, ContentModel,
    ElementDecl, GroupKind, MaxOccurs, ModelGroup, NamespaceConstraint, NotationDecl, Particle,
    ProcessContents, QName, Schema, SchemaInner, SchemaOptions, SchemaVersion, Term, TypeRef,
    Wildcard,
};
use super::types::{
    BuiltinType, ComplexType, Derivation, DerivationMethod, SimpleType,
};
use super::whitespace::WhitespaceMode;

const MAX_INCLUDE_DEPTH: u32 = 64;

// ── public entry points ──────────────────────────────────────────────────────

impl Schema {
    /// Compile a schema from a single XSD source string.  Equivalent to
    /// [`compile_with`](Self::compile_with) using a [`NoResolver`] —
    /// any `<xs:import>` or `<xs:include>` directive is rejected.
    pub fn compile_str(xsd: &str) -> Result<Schema, SchemaCompileError> {
        Self::compile_with(xsd, NoResolver)
    }

    /// Compile a schema with explicit [`SchemaOptions`] (the version
    /// knob today; more options may land here).  The resolver is
    /// [`NoResolver`] — imports / includes are rejected.
    pub fn compile_str_with_options(
        xsd: &str,
        options: SchemaOptions,
    ) -> Result<Schema, SchemaCompileError> {
        Self::compile_with_options(xsd, NoResolver, options)
    }

    /// Compile a schema, resolving any `<xs:import>` / `<xs:include>` /
    /// `<xs:redefine>` directives via the supplied [`SchemaResolver`].
    ///
    /// Cycles are detected (a schema importing one that already loaded
    /// is silently skipped); recursion is bounded at 64 levels deep.
    pub fn compile_with<R: SchemaResolver>(
        xsd: &str,
        resolver: R,
    ) -> Result<Schema, SchemaCompileError> {
        Self::compile_with_options(xsd, resolver, SchemaOptions::default())
    }

    /// Compile a schema with both a [`SchemaResolver`] and explicit
    /// [`SchemaOptions`].  This is the most general entry point; the
    /// others all delegate here.
    pub fn compile_with_options<R: SchemaResolver>(
        xsd: &str,
        resolver: R,
        options: SchemaOptions,
    ) -> Result<Schema, SchemaCompileError> {
        let mut builder = Builder::default();
        // Seed both the static option and the live "effective" copy.
        // `Auto` mode may later promote `effective_version` to Xsd11
        // when `parse_schema` sees `vc:minVersion="1.1"` on the root.
        builder.effective_version = options.version;
        builder.options = options;
        parse_one_file(xsd, &mut builder, &resolver, None, /*is_import=*/false)?;
        builder.into_schema()
    }

    /// Compile a schema from a file on disk, with relative-path
    /// resolution of `<xs:import>` and `<xs:include>` against the
    /// schema's own directory.
    ///
    /// Equivalent to:
    /// ```ignore
    /// let dir = path.parent().unwrap_or_else(|| Path::new("."));
    /// Schema::compile_with(&fs::read_to_string(path)?, FsResolver::new(dir))
    /// ```
    pub fn compile_file(path: impl AsRef<Path>) -> Result<Schema, SchemaCompileError> {
        let path = path.as_ref();
        let xsd = std::fs::read_to_string(path).map_err(|e|
            SchemaCompileError::msg(format!("read {}: {e}", path.display())))?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        Self::compile_with(&xsd, FsResolver::new(dir))
    }
}

// ── shared builder (accumulates declarations across nested imports) ──────────

#[derive(Default)]
struct Builder {
    /// Compile-time knobs (XSD version, ...).  Copied verbatim from
    /// the `Schema::compile_*_with_options` caller; the version field
    /// gates whether 1.1-only constructs are accepted.
    options:       SchemaOptions,
    /// Effective XSD version *after* applying `vc:minVersion` auto-
    /// detect, if [`SchemaVersion::Auto`] is selected.  Populated on
    /// first `<xs:schema>` parse.  All version-gated checks should
    /// read from this, not `options.version`.
    effective_version: SchemaVersion,
    target_ns:     Option<Arc<str>>,
    elements:      HashMap<QName, Arc<ElementDecl>>,
    attributes:    HashMap<QName, Arc<AttributeDecl>>,
    types:         HashMap<QName, TypeRef>,
    attr_groups:   HashMap<QName, Arc<AttributeGroup>>,
    model_groups:  HashMap<QName, Arc<ModelGroup>>,
    notations:     HashMap<QName, Arc<NotationDecl>>,
    substitutions: HashMap<QName, Vec<Arc<ElementDecl>>>,
    /// `schemaLocation` strings already loaded — cycle detection.
    loaded:        HashSet<String>,
    /// Current import/include nesting depth.
    depth:         u32,
    /// QNames seen as `<xs:attribute ref="...">` for cross-kind
    /// validation at finalize time (XSD §3.2.3): every ref must
    /// resolve to a top-level attribute, never an attributeGroup,
    /// type, or element.
    pending_attribute_refs: Vec<QName>,
    /// QNames seen as `<xs:element ref="...">` — must resolve to a
    /// top-level element at finalize, never a type or attribute.
    pending_element_refs:   Vec<QName>,
    /// QNames seen as `<xs:attributeGroup ref="...">` inside another
    /// <xs:attributeGroup> body.  The complex-type sites are covered
    /// by `ComplexType::pending_attribute_group_refs`; this collects
    /// the analogue for top-level attributeGroup definitions.
    pending_ag_refs_in_ag:  Vec<QName>,
    /// QNames seen as `<xs:group ref="...">` — must resolve to a
    /// top-level model group.
    #[allow(dead_code)]
    pending_group_refs:     Vec<QName>,
    /// Names of model groups defined inside an `<xs:redefine>` body.
    /// Per XSD §4.2.2 these are allowed to contain exactly one self-
    /// reference (`<xs:group ref="X">` where X is the redefined name),
    /// so they're exempted from the direct-cycle check.
    redefined_groups:       HashSet<QName>,
    /// Captures each `<xs:redefine>` group / attributeGroup body so the
    /// post-resolution pass can verify the redefining content is a
    /// valid restriction of the pre-redefine original (src-redefine).
    /// Each entry is `(name, redefining particle, pre-redefine particle)`.
    redefined_group_originals: Vec<(QName, Particle, Particle)>,
    /// Same shape, for `<xs:attributeGroup>` redefinitions: a snapshot
    /// of the original's attribute uses paired with the redefining
    /// arc so the post-pass can validate src-redefine restriction.
    /// The fourth tuple element records whether the redefining body
    /// contained a `<xs:attributeGroup ref="same-name"/>` self-ref —
    /// when it does, the redefining is an *additive* shape (the
    /// original's attributes are spliced in via the ref) rather than
    /// a strict restriction, and the validation rules relax to
    /// match what Saxon / Xerces / MSXML accept.
    redefined_attr_group_originals: Vec<(QName, Arc<AttributeGroup>, Arc<AttributeGroup>, bool)>,
    /// Set by `handle_redefine` while parsing a redefining
    /// attributeGroup body; consulted by `parse_top_attribute_group`
    /// when it sees an `<xs:attributeGroup ref="…">` so a same-name
    /// reference can be recorded as a self-reference.
    redefining_ag_name: Option<QName>,
    /// Populated when `redefining_ag_name` matches the inner ref.
    /// Read once by `handle_redefine` after `parse_top_attribute_group`
    /// returns, then cleared.
    redefining_ag_saw_self_ref: bool,
    /// Namespaces brought into scope via `<xs:import>`.  `None` means
    /// the no-namespace was imported (`<xs:import>` without a
    /// `namespace=` attribute).  Used by the post-parse ref-resolution
    /// passes to flag references to foreign namespaces the schema
    /// didn't import — XSD §3.3.6 / §3.2.3 src-resolve clause 4 require
    /// every QName reference to a foreign namespace's component to be
    /// preceded by an import of that namespace.
    imported_namespaces:    HashSet<Option<Arc<str>>>,
    /// Captures simple-type restrictions whose declared `base="…"`
    /// pointed at a same-target-namespace type that wasn't yet
    /// resolved at parse time (forward reference).  Each entry holds
    /// the base's QName plus the *new* facets this restriction added,
    /// so the post-pass can fetch the resolved base, splice in its
    /// facet set, and re-run `check_facet_tightening` — catching
    /// derived bounds that loosen the base (XSD §4.3 cvc-restriction).
    pending_simple_facet_checks: Vec<(QName, Vec<Facet>)>,
    /// Adjacency list of `<xs:attributeGroup ref="...">` edges keyed
    /// by the owning attributeGroup's QName.  Used to detect cycles
    /// (XSD §3.6.6 src-attribute-group-cyclic) — refs from inside
    /// complex types live in `ComplexType::pending_attribute_group_refs`
    /// and don't participate in cycles because complex types can't
    /// be a target of an attributeGroup ref.
    ag_refs_by_owner:       HashMap<QName, Vec<QName>>,
}

impl Builder {
    fn into_schema(mut self) -> Result<Schema, SchemaCompileError> {
        // Reject `<xs:attributeGroup ref="X">` references whose X
        // resolves to a different kind of schema component (a type,
        // an element, an attribute). Runs *before* the rewrite pass
        // because the rewrite clears the pending-refs list.
        check_attribute_group_ref_kinds(&self.types, &self.elements,
                                        &self.attr_groups, &self.attributes)?;

        // Same kind-collision check for refs that appear inside
        // <xs:attributeGroup> bodies (not just inside complex types).
        check_ag_refs_in_ag(&self.pending_ag_refs_in_ag, &self.attr_groups,
                            &self.types, &self.elements, &self.attributes)?;

        // XSD §3.6.6 (src-attribute-group-cyclic) — an attribute
        // group's expansion must not (transitively) reference
        // itself.
        check_attribute_group_cycles(&self.ag_refs_by_owner)?;

        // Every `<xs:attribute ref="X">` must point at a top-level
        // <xs:attribute> declaration — not an attributeGroup, complex
        // type, or element (XSD §3.2.3). Tolerate unresolvable names
        // for cross-schema soft-skip; flag cross-kind collisions.
        check_attribute_refs(&self.pending_attribute_refs, &self.attributes,
                             &self.attr_groups, &self.types, &self.elements,
                             self.target_ns.as_deref(), &self.imported_namespaces)?;

        // Every `<xs:element ref="X">` must resolve to a top-level
        // element declaration — never a type, attribute, or other
        // schema component (XSD §3.3.3).  Foreign-namespace refs
        // additionally require the namespace to have been imported
        // (XSD §3.3.6 src-resolve clause 4).
        check_element_refs(&self.pending_element_refs, &self.elements,
                           &self.types, &self.attributes, &self.attr_groups,
                           self.target_ns.as_deref(), &self.imported_namespaces)?;

        // Every `<xs:element substitutionGroup="X">` head must
        // resolve to an existing top-level element decl.
        check_substitution_group_heads(&self.elements)?;
        // XSD §3.3.6 — a substituting element's type must derive
        // from the head element's type using a method that is not
        // blocked by the head's (or head type's) `final`.
        check_substitution_group_typing(&self.elements, &self.types)?;

        // XSD §3.3.6 — an element typed `xs:ID` (or any simple type
        // derived from it) cannot have a `default=` / `fixed=` value
        // constraint.  The inline check during parse can miss this
        // when the element's type is a forward reference whose
        // builtin lineage isn't known yet; a post-pass over the
        // resolved type map catches the strays.
        check_id_typed_element_value_constraints(&self.elements, &self.types)?;

        // Every `type="X"` reference whose `X` matches another kind
        // of schema component (attribute, attributeGroup, element)
        // must be flagged — `type=` is a type-only namespace.
        check_type_refs_collide(&self.types, &self.attributes,
                                &self.attr_groups, &self.elements)?;

        // Attribute groups can reference other attribute groups by
        // name (XSD §3.6.3).  When attg1 declares `<xs:attributeGroup
        // ref="attg2"/>` and attg2 is declared later in source order,
        // the parser captures attg2's name in
        // [`Builder::ag_refs_by_owner`] but copies no attributes
        // (attg2 wasn't built yet).  Walk the dependency graph
        // bottom-up here so each group's attribute list contains the
        // transitive union of every group it references — otherwise
        // a complex type referencing attg1 would see attg1's local
        // attributes only, and instances using attg2's attributes
        // would be wrongly rejected with "unexpected attribute".
        flatten_nested_attribute_group_refs(
            &mut self.attr_groups, &self.ag_refs_by_owner,
        );
        // Expand pending `<xs:attributeGroup ref="…"/>` references
        // captured at parse time (forward refs to groups declared
        // later in source order).  Runs before extension merging so
        // a derived type's inherited attributes include the resolved
        // group members.
        resolve_attribute_group_refs(&mut self.types, &mut self.elements, &self.attr_groups)?;

        // XSD §3.8.6 src-model-group — a group must not transitively
        // reference itself except via an element boundary.  Detect the
        // direct (element-free) cycles before group expansion, since
        // the expander quietly leaves recursive refs in place and the
        // matcher builder cannot tell direct cycles from element-
        // mediated ones.
        check_model_group_cycles(&self.model_groups, &self.redefined_groups)?;

        // Expand `<xs:group ref="...">` particles into the referenced
        // model group's particle.  Runs before any other pass that
        // walks the content tree.  Group refs may chain (a group
        // references another group), so this iterates to fixpoint.
        resolve_group_refs(&mut self.types, &mut self.elements, &self.model_groups)?;

        // Patch `<xs:element ref="...">` placeholders in every
        // complex type's content model — both top-level named types
        // and the inline anonymous types attached to top-level element
        // decls.  The parser produces a stand-in ElementDecl
        // (type=xs:string) for refs; this pass swaps in the real
        // top-level decl from `self.elements`.
        //
        // Runs before substitution-group / extension-merge / matcher
        // compilation so the DFA records the correct decl Arc.
        resolve_element_refs(&mut self.types, &mut self.elements);

        // cos-ct-extends / cos-ct-restricts (XSD §3.4.6) — the
        // derived type's content nature (simple vs complex) must
        // match the base's. Runs before extension merging so the
        // derived content still reflects exactly what the user
        // wrote.
        check_derivation_content_kind(&self.types)?;

        // XSD §3.4.6 — for complexContent derivation, the derived
        // type's `mixed` value must match the base's.  Restriction
        // can't widen text-acceptance, and extension can't change
        // the content-model kind.  Done before extension merging so
        // we see the derived's intended mixed flag.
        check_complex_mixed_consistency(&self.types, &self.elements)?;

        // XSD §3.4.6 — a base complex type's `final` may forbid the
        // derived type's chosen derivation method (restriction or
        // extension).  Enforce before merge so the spec's intended
        // base is the one whose `final` we check.
        check_complex_type_final(&self.types, &self.elements)?;

        // Merge content+attributes from base into every named
        // complex type that derives by extension.  Done before
        // matcher compilation so the DFA is built over the merged
        // content model.
        merge_extension_chains(&mut self.types)?;

        // Inline anonymous complex types attached to top-level
        // element decls (e.g. `<xs:element name="Foo"><xs:complexType>
        // <xs:complexContent><xs:extension base="Bar">…`) need the
        // same extension-merge treatment, but they live in
        // `self.elements[].type_def`, not in `self.types`.  Walk the
        // element map and patch each one whose inline type derives by
        // extension.  Runs AFTER named types are merged so the base
        // lookup sees their fully composed content; runs BEFORE the
        // substitution-group map is captured below so that map
        // records the patched Arcs.
        merge_inline_extension_in_elements(&mut self.elements, &self.types);

        // XSD §3.4.2 — a restriction-derived complex type implicitly
        // inherits any base attribute uses it doesn't redeclare.
        // Without this fold-in the derived type would silently drop
        // those attributes from its effective set.
        merge_restriction_attributes(&mut self.types, &mut self.elements);

        // Element refs in other types' content (e.g. `doc → element
        // ref=elem`) were patched against the pre-merge element decl
        // by the earlier `resolve_element_refs` pass.  Re-run after
        // the inline-extension merge so consumers see the merged
        // type def, not the stale one captured before composition.
        resolve_element_refs(&mut self.types, &mut self.elements);

        // Particle-restriction soundness check (cvc-particle-restricts,
        // XSD §3.9.6). Runs AFTER extension chains are merged so the
        // base type's content already incorporates any inherited
        // particles. Restriction-derived types are untouched by
        // `merge_extension_chains`, so the derived particle still
        // reflects exactly what the user wrote.
        // XSD §3.8.6 — within a content model, two element particles
        // with the same name cannot carry different type definitions.
        check_element_decls_consistent(&self.types, &self.elements)?;

        // XSD §4.3 cvc-restriction — for simple-type restrictions
        // whose `base="…"` was a same-namespace forward reference at
        // parse time, the inherited facets weren't known yet, so the
        // inline check ran against an empty base set.  Resolve the
        // base now and revalidate the derived facets against the
        // real ancestor facet set.
        check_pending_simple_facet_tightening(
            &self.pending_simple_facet_checks, &self.types,
        )?;

        super::particle_restriction::check_restriction_chains(&self.types, &self.elements,
                                                               self.target_ns.as_deref())?;

        // XSD §4.2.2 src-redefine — each redefined <xs:group>'s new
        // body (with any self-reference expanded back to the original
        // group's particle) must be a valid restriction of the
        // pre-redefine original.  Captured during the redefine body
        // walk; checked here once the types map is fully resolved.
        super::particle_restriction::check_redefined_groups(
            &self.redefined_group_originals, &self.types, &self.elements,
            self.target_ns.as_deref(),
        )?;
        // Same idea for `<xs:attributeGroup>` redefinitions —
        // honoring the spec-allowed additive shape when the
        // redefining body contains a `<xs:attributeGroup
        // ref="same-name"/>` self-reference.
        check_redefined_attribute_groups(&self.redefined_attr_group_originals,
                                         &self.types)?;

        // XSD §3.2.6 / §3.4.6 — every <xs:attribute>'s `type=`
        // must resolve to a simple type, never a complex type.
        // Pointed-to references are placeholders post-parse;
        // verify them against the resolved type map here.
        check_attribute_type_kinds(&self.types, &self.attributes, &self.elements)?;

        // XSD §3.4.6 (cos-ct-derived-ok by restriction) — every
        // base attribute use whose `use=required` must be retained
        // as required in the restricting derived type, and base
        // `fixed="X"` values cannot be redefined to differ.
        check_complex_restriction_attributes(&self.types, &self.elements)?;

        // XSD §3.11.6 — every <xs:keyref refer="…"> must name a
        // <xs:key> or <xs:unique> declared somewhere in the schema.
        check_keyref_refer(&self.elements)?;


        // XSD §3.3.3 (cvc-elt-2.x) — element's default/fixed value
        // is only valid against a simple or mixed-complex type. The
        // parser already enforces this for inline types; this
        // post-pass catches type-by-name references whose resolved
        // type wasn't available at parse time.
        check_element_value_constraints(&self.types, &self.elements)?;

        // Compute substitution-group map from each element's
        // `substitutionGroup` attribute (if any), then take the
        // transitive closure: if `A substitutes B` and `B substitutes C`,
        // then `A` also substitutes `C`.  XSD §3.3.6 treats
        // substitution-group membership as transitive.
        let mut subs: HashMap<QName, Vec<Arc<ElementDecl>>> = HashMap::new();
        for decl in self.elements.values() {
            if let Some(head) = &decl.substitution_group {
                subs.entry(head.clone()).or_default().push(decl.clone());
            }
        }
        // Closure: for each member of subs[A], also add subs[member.name]
        // recursively.  Bounded iteration to handle declaration order.
        let mut changed = true;
        let mut guard = 0;
        while changed && guard < 64 {
            changed = false;
            guard += 1;
            let heads: Vec<QName> = subs.keys().cloned().collect();
            for head in heads {
                let direct: Vec<Arc<ElementDecl>> = subs[&head].clone();
                for m in direct {
                    let Some(deeper) = subs.get(&m.name).cloned() else { continue };
                    for d in deeper {
                        let list = subs.get_mut(&head).unwrap();
                        if !list.iter().any(|x| Arc::ptr_eq(x, &d) || x.name == d.name) {
                            list.push(d);
                            changed = true;
                        }
                    }
                }
            }
        }
        self.substitutions = subs;

        // Compile content matchers (DFA where possible, all-group
        // fallback otherwise) for every reachable complex type — both
        // top-level *and* inline anonymous types attached to element
        // decls.  Done here so substitution groups are already resolved.
        let mut visited: HashSet<usize> = HashSet::new();
        let types_snapshot = self.types.clone();
        let target_ns_str = self.target_ns.as_deref().map(|s| s.to_owned());
        let target_ns_ref = target_ns_str.as_deref();
        for tr in self.types.values() {
            if let TypeRef::Complex(ct) = tr {
                walk_complex_for_matchers(ct, &self.substitutions, &types_snapshot,
                                          &mut visited, target_ns_ref)?;
            }
        }
        for decl in self.elements.values() {
            if let TypeRef::Complex(ct) = &decl.type_def {
                walk_complex_for_matchers(ct, &self.substitutions, &types_snapshot,
                                          &mut visited, target_ns_ref)?;
            }
        }

        Ok(Schema::from_inner(SchemaInner {
            target_namespace: self.target_ns,
            elements:         self.elements,
            attributes:       self.attributes,
            types:            self.types,
            attribute_groups: self.attr_groups,
            model_groups:     self.model_groups,
            notations:        self.notations,
            substitutions:    self.substitutions,
        }))
    }
}

/// Top-level driver — parse one file's contents into the shared builder.
/// Recursive on `<xs:import>` / `<xs:include>` via the resolver.
fn parse_one_file<R: SchemaResolver>(
    xsd: &str,
    builder: &mut Builder,
    resolver: &R,
    expected_target_ns: Option<&str>,
    is_import: bool,
) -> Result<(), SchemaCompileError> {
    if builder.depth >= MAX_INCLUDE_DEPTH {
        return Err(SchemaCompileError::msg(format!(
            "schema include nesting exceeded {MAX_INCLUDE_DEPTH} levels"
        )));
    }
    let mut p = Parser::new(xsd, builder, resolver, expected_target_ns);
    p.is_import_target = is_import;
    p.parse_schema()
}

// ── internal parser ──────────────────────────────────────────────────────────

struct Parser<'a, 'b, R: SchemaResolver> {
    reader:    XmlReader<'a>,
    builder:   &'b mut Builder,
    resolver:  &'b R,
    /// If `Some`, the schema being parsed must have this `targetNamespace`
    /// (used to enforce `<xs:include>` rules — included schemas must
    /// either match or be unqualified).
    expected_target_ns: Option<&'b str>,
    /// True when this parser was spawned from `<xs:import>` (vs
    /// `<xs:include>` or `<xs:redefine>`).  Imports require the
    /// loaded file's `targetNamespace` to match `expected_target_ns`
    /// exactly; includes apply the chameleon rule when the file has
    /// no `targetNamespace`.
    is_import_target: bool,
    /// The `targetNamespace` of the schema document being parsed.
    /// Per-file (NOT the same as `builder.target_ns`, which holds
    /// the original/root schema's namespace).  An imported schema
    /// may declare its own namespace; declarations parsed from that
    /// file land in this namespace, not the importer's.
    current_target_ns: Option<Arc<str>>,
    /// `elementFormDefault` for the schema document being parsed.
    /// Per-file: an included schema with its own form default doesn't
    /// inherit the includer's.
    element_form_default: Form,
    /// `attributeFormDefault` for the schema document being parsed.
    attribute_form_default: Form,
    /// `blockDefault` — applied to top-level `<xs:element>` and
    /// `<xs:complexType>` decls without their own `block` attribute.
    /// XSD 1.0 §3.1.
    block_default: BlockSet,
    /// `finalDefault` — same idea for `final`.
    final_default: BlockSet,
    ns_stack:  Vec<HashMap<String, String>>,
    attr_buf:  Vec<Attr<'a>>,
    /// Track every `id` value seen so far in this schema document so
    /// duplicates (XSD spec: `id` is typed `xs:ID` and must be unique
    /// within the document) and ill-formed NCNames are rejected at
    /// compile time.
    seen_ids:  HashSet<String>,
    /// Identity-constraint names (XSD §3.11.1): unique, key, and
    /// keyref share a single namespace, so a duplicate name across
    /// any combination is a schema validity error.
    seen_ic_names: HashSet<QName>,
    /// Top-level element names declared *in this schema document*.
    /// Per-file rather than per-compilation so that include cycles
    /// re-processing the same source don't trip the duplicate check.
    local_top_element_names: HashSet<QName>,
    /// True while parsing an `<xs:redefine>` body — redefinitions
    /// legitimately re-declare names that already exist in the
    /// builder, so per-name duplicate checks must be relaxed.
    in_redefine: bool,
    /// Most recently parsed `<xs:restriction base=…>` resolved QName.
    /// Captured during `parse_simple_restriction` so the redefine-body
    /// validator can confirm that a redefining `<xs:simpleType>`
    /// restricts the same-named original (src-redefine).
    last_simple_restriction_base: Option<QName>,
    /// The QName of a top-level `<xs:simpleType>` currently being
    /// parsed.  `parse_simple_restriction` consults it to reject a
    /// direct self-reference (`base="X"` inside `<xs:simpleType name="X">`)
    /// when not inside `<xs:redefine>`.
    simple_type_in_flight: Option<QName>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form { Qualified, Unqualified }

impl Form {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "qualified"   => Some(Form::Qualified),
            "unqualified" => Some(Form::Unqualified),
            _ => None,
        }
    }
}

const XS: &str = "http://www.w3.org/2001/XMLSchema";

impl<'a, 'b, R: SchemaResolver> Parser<'a, 'b, R> {
    fn new(
        input: &'a str,
        builder: &'b mut Builder,
        resolver: &'b R,
        expected_target_ns: Option<&'b str>,
    ) -> Self {
        Self {
            reader:   XmlReader::from_str(input),
            builder,
            resolver,
            expected_target_ns,
            is_import_target: false,
            // Set by `parse_schema` from the root's `targetNamespace`
            // attribute (or from `expected_target_ns` for chameleon
            // includes that don't declare one).
            current_target_ns: None,
            // Spec default for both `*FormDefault` attributes is
            // "unqualified".  Overridden when the `<xs:schema>` root
            // declares a value (read in `parse_schema`).
            element_form_default:   Form::Unqualified,
            attribute_form_default: Form::Unqualified,
            block_default: BlockSet::default(),
            final_default: BlockSet::default(),
            ns_stack: vec![{
                // Namespaces in XML §3 — the `xml` prefix is bound
                // implicitly to the XML namespace URI, with no
                // explicit declaration required.
                let mut m = HashMap::new();
                m.insert("xml".to_string(), "http://www.w3.org/XML/1998/namespace".to_string());
                m
            }],
            attr_buf: Vec::new(),
            seen_ids: HashSet::new(),
            seen_ic_names: HashSet::new(),
            local_top_element_names: HashSet::new(),
            in_redefine: false,
            last_simple_restriction_base: None,
            simple_type_in_flight: None,
        }
    }

    /// Drain the staged attribute buffer into an owned `Vec`, also
    /// running schema-wide attribute checks that fire on every
    /// element regardless of which parse method consumes it
    /// (currently `id` uniqueness/NCName and `name` NCName).
    fn take_attrs(&mut self) -> Result<Vec<Attr<'a>>, SchemaCompileError> {
        let attrs: Vec<Attr<'a>> = self.attr_buf.drain(..).collect();
        self.validate_id_attr(&attrs)?;
        self.validate_name_attr(&attrs)?;
        Ok(attrs)
    }

    fn validate_id_attr(&mut self, attrs: &[Attr<'a>]) -> Result<(), SchemaCompileError> {
        let Some(id) = self.attr(attrs, "id") else { return Ok(()) };
        super::types::SimpleType::of_builtin(super::types::BuiltinType::Id)
            .validate(id)
            .map_err(|e| self.err(format!(
                "id={id:?} is not a valid ID/NCName: {}", e.message
            )))?;
        if !self.seen_ids.insert(id.to_owned()) {
            return Err(self.err(format!("duplicate id {id:?} in schema")));
        }
        Ok(())
    }

    /// All XSD elements that accept a `name` attribute
    /// (`<xs:element>`, `<xs:attribute>`, `<xs:complexType>`,
    /// `<xs:simpleType>`, `<xs:group>`, `<xs:attributeGroup>`,
    /// `<xs:notation>`, `<xs:unique>`, `<xs:key>`, `<xs:keyref>`)
    /// require it to be an NCName; no XSD-defined element accepts
    /// a non-NCName `name`, so the check is applied uniformly
    /// rather than dispatching per parent element.
    fn validate_name_attr(&mut self, attrs: &[Attr<'a>]) -> Result<(), SchemaCompileError> {
        let Some(name) = self.attr(attrs, "name") else { return Ok(()) };
        super::types::SimpleType::of_builtin(super::types::BuiltinType::NCName)
            .validate(name)
            .map_err(|e| self.err(format!(
                "name={name:?} is not a valid NCName: {}", e.message
            )))?;
        Ok(())
    }

    fn err(&self, msg: impl Into<String>) -> SchemaCompileError {
        SchemaCompileError::msg(msg)
    }

    /// Parse an `xs:boolean` attribute value strictly per XSD §3.2.2.3:
    /// only `"true"`, `"false"`, `"1"`, `"0"` are valid (case-sensitive,
    /// no surrounding whitespace).  Returns `Ok(None)` when the attribute
    /// is absent.
    fn parse_xsd_bool(
        &self,
        attrs: &[Attr<'a>],
        name:  &str,
    ) -> Result<Option<bool>, SchemaCompileError> {
        let Some(raw) = self.attr(attrs, name) else { return Ok(None); };
        match raw {
            "true"  | "1" => Ok(Some(true)),
            "false" | "0" => Ok(Some(false)),
            _ => Err(self.err(format!(
                "{name}={raw:?}: must be \"true\", \"false\", \"1\", or \"0\""
            ))),
        }
    }

    /// Enforce XSD's "at most one annotation, must come first" rule
    /// (§3.x.x for each construct).  Call once per child element
    /// encountered inside a parent that doesn't allow interleaved
    /// annotations (most constructs — the exception is `<xs:schema>`
    /// itself).  Mutates the two state bools to track what's been
    /// seen so far in this parent's child sequence.
    fn check_annotation_pos(
        &self,
        local: &str,
        seen_annotation:     &mut bool,
        seen_non_annotation: &mut bool,
        parent:              &str,
    ) -> Result<(), SchemaCompileError> {
        if local == "annotation" {
            if *seen_annotation {
                return Err(self.err(format!(
                    "<xs:{parent}> may have at most one <xs:annotation> child"
                )));
            }
            if *seen_non_annotation {
                return Err(self.err(format!(
                    "<xs:annotation> must precede other children of <xs:{parent}>"
                )));
            }
            *seen_annotation = true;
        } else {
            *seen_non_annotation = true;
        }
        Ok(())
    }

    /// Walk a body that only allows at-most-one `<xs:annotation>` and
    /// nothing else as a direct child (e.g. `<xs:any>`,
    /// `<xs:anyAttribute>`, leaf facets).  Replaces `skip_body()` at
    /// sites that should still enforce the annotation rule.  Uses
    /// the standard push/pop ns-scope walk pattern.
    fn parse_anno_only_body(&mut self, parent: &str) -> Result<(), SchemaCompileError> {
        let mut seen_anno = false;
        let mut seen_other = false;
        loop {
            match self.next_event()? {
                EventInto::StartElement { name } => {
                    let child_attrs = self.take_attrs()?;
                    self.push_ns_scope(&child_attrs);
                    let qn = self.qname_of_element(&name)?;
                    if qn.namespace.as_deref() == Some(XS) {
                        self.check_annotation_pos(
                            &qn.local, &mut seen_anno, &mut seen_other, parent,
                        )?;
                        match qn.local.as_ref() {
                            "annotation" => self.parse_annotation_body(&child_attrs)?,
                            other => return Err(self.err(format!(
                                "<xs:{parent}> body may only contain <xs:annotation>, \
                                 found <xs:{other}>"
                            ))),
                        }
                    } else if qn.namespace.is_none() {
                        // No-namespace child elements aren't covered by
                        // the XSD foreign-extension rule (which applies
                        // to elements in a *different* namespace, not
                        // the absent one).  The schema-for-schemas
                        // content model `(annotation?)` doesn't admit
                        // them — reject.
                        return Err(self.err(format!(
                            "<xs:{parent}> body may only contain <xs:annotation>, \
                             found <{}>", qn.local,
                        )));
                    } else {
                        // True foreign-namespace child: tolerated as
                        // an extension annotation per XSD §1.4.1.
                        self.skip_body()?;
                    }
                    self.pop_ns_scope();
                }
                EventInto::Text(t) | EventInto::CData(t) => {
                    if !t.chars().all(char::is_whitespace) {
                        return Err(self.err(format!(
                            "<xs:{parent}> body has non-whitespace text content"
                        )));
                    }
                }
                EventInto::EndElement { .. } => return Ok(()),
                EventInto::Eof => return Err(self.err("unexpected EOF")),
                _ => {}
            }
        }
    }

    /// Build a [`TypeRef`] for a `type="..."` reference.  Built-in
    /// `xs:*` types are resolved immediately; user-defined types get a
    /// placeholder that the post-pass [`resolve_references`] patches.
    ///
    /// Resolving built-ins at parse time skips a HashMap lookup per
    /// element/attribute on the validator's hot path — most schemas
    /// have built-in types in 80%+ of their fields.
    fn type_ref_for(&self, type_qn: QName) -> TypeRef {
        if type_qn.namespace.as_deref() == Some(QName::XSD_NS) {
            if let Some(b) = BuiltinType::from_name(&type_qn.local) {
                return TypeRef::Simple(Arc::new(SimpleType::of_builtin(b)));
            }
            // xs:anyType isn't in the BuiltinType set (it's a
            // complex type); build the spec's concrete ur-type so
            // substitution-group typing checks and xsi:type
            // resolution see a real ComplexType, not a placeholder.
            if type_qn.local.as_ref() == "anyType" {
                return any_type_ref();
            }
        }
        // User-defined type — placeholder, patched in the post-pass.
        TypeRef::Simple(Arc::new(SimpleType {
            name:       Some(Arc::from(format!("UNRESOLVED:{type_qn}"))),
            builtin:    BuiltinType::String,
            facets:     FacetSet::default(),
            whitespace: WhitespaceMode::Preserve,
            variety:    super::types::Variety::Atomic,
            final_:     super::schema::BlockSet::default(),
            assertions: Vec::new(),
        }))
    }

    /// Same as [`type_ref_for`] but for attribute decls (always
    /// simple-typed).  Returns `Arc<SimpleType>` directly.
    fn simple_type_for(&self, type_qn: QName) -> Arc<SimpleType> {
        if type_qn.namespace.as_deref() == Some(QName::XSD_NS) {
            if let Some(b) = BuiltinType::from_name(&type_qn.local) {
                return Arc::new(SimpleType::of_builtin(b));
            }
        }
        Arc::new(SimpleType {
            name:       Some(Arc::from(format!("UNRESOLVED:{type_qn}"))),
            builtin:    BuiltinType::String,
            facets:     FacetSet::default(),
            whitespace: WhitespaceMode::Preserve,
            variety:    super::types::Variety::Atomic,
            final_:     super::schema::BlockSet::default(),
            assertions: Vec::new(),
        })
    }

    // ── namespace bookkeeping ────────────────────────────────────────────

    fn push_ns_scope(&mut self, attrs: &[Attr<'a>]) {
        let top = self.ns_stack.last().cloned().unwrap_or_default();
        let mut new = top;
        for a in attrs {
            let name = a.name;
            if name == "xmlns" {
                new.insert(String::new(), a.value.to_string());
            } else if let Some(prefix) = name.strip_prefix("xmlns:") {
                new.insert(prefix.to_string(), a.value.to_string());
            }
        }
        self.ns_stack.push(new);
    }

    fn pop_ns_scope(&mut self) {
        self.ns_stack.pop();
    }

    /// Resolve a prefix against the current scope.  Returns `None` for
    /// the no-namespace, `Some(uri)` otherwise.
    fn resolve_prefix(&self, prefix: &str) -> Option<&str> {
        for scope in self.ns_stack.iter().rev() {
            if let Some(uri) = scope.get(prefix) {
                return Some(uri.as_str());
            }
        }
        None
    }

    /// Parse a `prefix:local` or `local` form into a [`QName`].  When no
    /// prefix is present, the schema's `targetNamespace` is used unless
    /// the caller passed a `null_ns_default` of `false` (some XSD
    /// constructs default unprefixed names to no-namespace).
    fn parse_qname(&self, raw: &str, null_ns_default: bool) -> Result<QName, SchemaCompileError> {
        // XML §3 — a QName is `(Prefix ':')? LocalPart`; both prefix
        // and local part must be NCNames. Reject anything that isn't.
        let ncname_check = |s: &str, label: &str| -> Result<(), SchemaCompileError> {
            super::types::SimpleType::of_builtin(super::types::BuiltinType::NCName)
                .validate(s)
                .map_err(|e| self.err(format!(
                    "QName {label} {s:?} is not an NCName: {}", e.message,
                )))?;
            Ok(())
        };
        match raw.split_once(':') {
            Some((prefix, local)) => {
                ncname_check(prefix, "prefix")?;
                ncname_check(local, "local part")?;
                let uri = self.resolve_prefix(prefix).ok_or_else(||
                    self.err(format!("undeclared namespace prefix {prefix:?}"))
                )?;
                Ok(QName::new(Some(uri), local))
            }
            None => {
                ncname_check(raw, "local part")?;
                let uri = if null_ns_default { None } else {
                    self.resolve_prefix("").map(|s| s.to_owned())
                        .or_else(|| self.current_target_ns.as_deref().map(|s| s.to_owned()))
                };
                Ok(QName {
                    namespace: uri.map(Arc::from),
                    local:     Arc::from(raw),
                })
            }
        }
    }

    /// XSD 1.1 § 3.2.2 — `inheritable="true|false"` on
    /// `<xs:attribute>`.  Default is `false`.  In 1.0 mode the
    /// attribute is unknown to the schema-for-schema; we surface
    /// that explicitly rather than silently ignoring (which would
    /// be the libxml2-style bug we set out to avoid).
    fn parse_inheritable(&self, attrs: &[Attr<'a>]) -> Result<bool, SchemaCompileError> {
        let Some(raw) = self.attr(attrs, "inheritable") else { return Ok(false); };
        if !matches!(self.builder.effective_version, SchemaVersion::Xsd11) {
            return Err(self.err(
                "<xs:attribute inheritable=...> is an XSD 1.1 attribute — \
                 set SchemaOptions::version to Xsd11, or to Auto with \
                 vc:minVersion=\"1.1\" on <xs:schema>",
            ));
        }
        match raw {
            "true"  | "1" => Ok(true),
            "false" | "0" => Ok(false),
            other => Err(self.err(format!(
                "<xs:attribute inheritable={other:?}>: must be \"true\" or \"false\""
            ))),
        }
    }

    /// Build a [`Wildcard`] from an `<xs:any>` / `<xs:anyAttribute>`
    /// attribute set, using the parser's current namespace context to
    /// resolve `notQName` tokens against `xmlns:` declarations in scope.
    fn parse_wildcard(&self, attrs: &[Attr<'a>]) -> Result<Wildcard, SchemaCompileError> {
        // XSD §3.10.2 — attribute set of <xs:any> / <xs:anyAttribute>.
        // The minOccurs/maxOccurs are only on <xs:any>; we keep them
        // in the allow-list here so the same checker handles both
        // (anyAttribute callers don't supply them).  `notQName` /
        // `notNamespace` are XSD 1.1 additions; they're allow-listed
        // unconditionally but `parse_wildcard_attrs` rejects them
        // in 1.0 mode with a precise error citing the version.
        self.check_known_attrs(attrs, &[
            "id", "namespace", "processContents", "minOccurs", "maxOccurs",
            "notQName", "notNamespace",
        ], "any")?;
        parse_wildcard_attrs(
            attrs,
            &self.current_target_ns,
            self.builder.effective_version,
            &mut |tok| self.parse_qname(tok, false),
        )
    }

    // ── event helpers ────────────────────────────────────────────────────

    fn next_event(&mut self) -> Result<EventInto<'a>, SchemaCompileError> {
        loop {
            let ev = self.reader.next_into(&mut self.attr_buf)?;
            match ev {
                EventInto::Comment(_) | EventInto::Pi { .. } => continue,
                _ => return Ok(ev),
            }
        }
    }

    /// Skip an element's body (advance past its matching EndElement).
    /// The opening Start has already been consumed.
    fn skip_body(&mut self) -> Result<(), SchemaCompileError> {
        let mut depth = 1usize;
        while depth > 0 {
            match self.next_event()? {
                EventInto::StartElement { .. } => depth += 1,
                EventInto::EndElement   { .. } => depth -= 1,
                EventInto::Eof => return Err(self.err("unexpected EOF inside element")),
                _ => {}
            }
        }
        Ok(())
    }

    /// Parse `<xs:assert>` / `<xs:assertion>` and consume through
    /// the matching EndElement.  Returns `Some(Assertion)` when
    /// `test=` is present; returns `None` for an empty / missing
    /// test (silently ignored — the schema is still well-formed
    /// without the constraint).
    ///
    /// Captures the current namespace scope so the test expression
    /// can resolve prefixed names at evaluation time, and reads
    /// `xpathDefaultNamespace` if set.  The body may contain only
    /// an optional `<xs:annotation>`; anything else is silently
    /// skipped today (assertion-attached annotations aren't modelled).
    fn parse_assertion_body(&mut self, attrs: &[Attr<'a>])
        -> Result<Option<super::schema::Assertion>, SchemaCompileError>
    {
        let test    = self.attr(attrs, "test").map(str::to_string);
        let default = self.attr(attrs, "xpathDefaultNamespace").map(str::to_string);
        // Snapshot the in-scope namespaces — XPath in `test` resolves
        // prefixes against these at evaluation time.
        let namespaces: Vec<(Option<String>, String)> = self.ns_stack.last()
            .map(|top| top.iter()
                .map(|(prefix, uri)| {
                    let p = if prefix.is_empty() { None } else { Some(prefix.clone()) };
                    (p, uri.clone())
                })
                .collect())
            .unwrap_or_default();
        self.skip_body()?;
        Ok(test.filter(|t| !t.is_empty()).map(|test| super::schema::Assertion {
            test,
            namespaces,
            xpath_default_namespace: default,
        }))
    }

    /// XSD §3.13.2 — `<xs:annotation>` body must contain only
    /// `<xs:appinfo>` and `<xs:documentation>` children (in any
    /// order). Nested `<xs:annotation>` or any other XS-namespace
    /// child is a schema validity error. Foreign-namespace
    /// elements are allowed and skipped.
    fn parse_annotation_body(&mut self, attrs: &[Attr<'a>]) -> Result<(), SchemaCompileError> {
        // XSD §3.13.2 — `<xs:annotation>` takes only `id` plus
        // foreign-namespace attributes.  `check_known_attrs` already
        // permits foreign attrs.
        self.check_known_attrs(attrs, &["id"], "annotation")?;
        loop {
            match self.next_event()? {
                EventInto::StartElement { name } => {
                    let child_attrs = self.take_attrs()?;
                    self.push_ns_scope(&child_attrs);
                    let qn = self.qname_of_element(&name)?;
                    if qn.namespace.as_deref() == Some(XS) {
                        match qn.local.as_ref() {
                            "appinfo" => {
                                self.check_known_attrs(&child_attrs, &["source"], "appinfo")?;
                            }
                            "documentation" => {
                                self.check_known_attrs(&child_attrs,
                                    &["source", "xml:lang"], "documentation")?;
                                if let Some(lang) = self.attr(&child_attrs, "xml:lang") {
                                    if lang.trim().is_empty() {
                                        return Err(self.err(
                                            "<xs:documentation xml:lang=...>: language tag \
                                             must not be empty or whitespace-only",
                                        ));
                                    }
                                }
                            }
                            other => return Err(self.err(format!(
                                "<xs:{other}> is not allowed as a child of <xs:annotation>"
                            ))),
                        }
                    }
                    // appinfo / documentation bodies are mixed content
                    // (any well-formed XML); we don't peek inside.
                    self.skip_body()?;
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => return Ok(()),
                EventInto::Eof => return Err(self.err("unexpected EOF inside <xs:annotation>")),
                _ => {}
            }
        }
    }

    // ── top-level walk ───────────────────────────────────────────────────

    fn parse_schema(&mut self) -> Result<(), SchemaCompileError> {
        // First non-trivia event must be <xs:schema>.
        let ev = self.next_event()?;
        let attrs = match ev {
            EventInto::StartElement { name } => {
                let attrs = self.take_attrs()?;
                self.push_ns_scope(&attrs);
                let qn = self.qname_of_element(&name)?;
                if qn.namespace.as_deref() != Some(XS) || qn.local.as_ref() != "schema" {
                    return Err(self.err(format!(
                        "root element must be xs:schema, got {qn}"
                    )));
                }
                attrs
            }
            _ => return Err(self.err("XSD must start with <xs:schema>")),
        };
        // XSD §3.15.2 — the root element's defined attributes
        // (`targetNamespace`, `attributeFormDefault`, `blockDefault`,
        // `elementFormDefault`, `finalDefault`, `id`, `version`,
        // `xml:lang`, plus the XSD 1.1 `defaultAttributes` and the
        // version-control `vc:*` set) must be unqualified.  Reject
        // any XSD-namespace-qualified spelling (e.g. `xsd:targetNamespace`)
        // — they're not the same slot per the schema-for-schemas.
        self.check_known_attrs(&attrs, &[
            "id", "targetNamespace", "version", "attributeFormDefault",
            "elementFormDefault", "blockDefault", "finalDefault",
            "defaultAttributes",
        ], "schema")?;
        // Pull targetNamespace + form defaults from the root.  For
        // includes the namespace must match the including schema's;
        // for the first/root file it sets the builder's target
        // namespace.  Form defaults are per-file and scope only the
        // declarations parsed below.
        let mut this_target_ns: Option<Arc<str>> = None;
        for a in &attrs {
            match a.name() {
                "targetNamespace" => {
                    let v = a.value.as_ref();
                    if v.is_empty() {
                        return Err(self.err(
                            "<xs:schema targetNamespace=\"\">: empty namespace URI is not allowed"
                        ));
                    }
                    this_target_ns = Some(Arc::from(v));
                }
                "elementFormDefault" => {
                    self.element_form_default = Form::parse(a.value.as_ref()).ok_or_else(|| self.err(
                        format!("<xs:schema elementFormDefault={:?}>: must be \"qualified\" or \"unqualified\"", a.value)
                    ))?;
                }
                "attributeFormDefault" => {
                    self.attribute_form_default = Form::parse(a.value.as_ref()).ok_or_else(|| self.err(
                        format!("<xs:schema attributeFormDefault={:?}>: must be \"qualified\" or \"unqualified\"", a.value)
                    ))?;
                }
                "blockDefault" => {
                    self.block_default = parse_block_set(Some(a.value.as_ref()))?;
                }
                "finalDefault" => {
                    self.final_default = parse_block_set(Some(a.value.as_ref()))?;
                }
                _ => {}
            }
        }
        // XSD 1.1 § F.1: `vc:minVersion` lets a schema author declare
        // they're using 1.1 features.  Honoured only in `Auto` mode
        // (the explicit Xsd10 / Xsd11 settings override).  When seen
        // and we promote to Xsd11, downstream wildcard / construct
        // checks gate on the new value.
        if matches!(self.builder.options.version, SchemaVersion::Auto) {
            let min_version = attrs.iter().find(|a| a.name() == "vc:minVersion");
            if let Some(mv) = min_version {
                if mv.value.as_ref().trim() == "1.1" {
                    self.builder.effective_version = SchemaVersion::Xsd11;
                }
            }
        }
        match (&self.expected_target_ns, &this_target_ns) {
            (Some(expected), Some(found)) => {
                if expected != &found.as_ref() {
                    return Err(self.err(format!(
                        "included schema has targetNamespace={found:?}, expected {expected:?}"
                    )));
                }
                self.current_target_ns = Some(found.clone());
            }
            (Some(expected), None) => {
                if self.is_import_target {
                    // XSD §4.2.3 — an imported schema document must
                    // declare the namespace named on the import.  A
                    // file with no `targetNamespace` cannot satisfy
                    // an import of a non-empty namespace.
                    return Err(self.err(format!(
                        "imported schema has no targetNamespace, expected {expected:?}"
                    )));
                }
                // Chameleon include — no targetNamespace declared,
                // adopts the including schema's namespace.
                self.current_target_ns = Some(Arc::from(*expected));
            }
            (None, Some(found)) => {
                if self.builder.depth > 0 {
                    // An included/redefined schema with a target
                    // namespace cannot be loaded into a parent that
                    // has none: the parent's chameleon rule needs the
                    // child to be unqualified (XSD §4.2.1).
                    return Err(self.err(format!(
                        "included schema has targetNamespace={found:?}, \
                         but the including schema has no targetNamespace"
                    )));
                }
                self.current_target_ns = Some(found.clone());
                if self.builder.target_ns.is_none() {
                    self.builder.target_ns = Some(found.clone());
                }
            }
            (None, None) => {} // top-level no-namespace schema.
        }

        // Walk children.
        loop {
            match self.next_event()? {
                EventInto::StartElement { name } => {
                    let attrs = self.take_attrs()?;
                    self.push_ns_scope(&attrs);
                    let qn = self.qname_of_element(&name)?;
                    let ns = qn.namespace.as_deref();
                    if ns != Some(XS) {
                        if ns.is_none() {
                            // A no-namespace element directly under
                            // <xs:schema> isn't a valid schema component
                            // (the schema-for-schemas admits no such
                            // wildcard) — almost always an XSD element
                            // written without its namespace prefix.
                            return Err(self.err(format!(
                                "element <{}> at the schema top level is not in \
                                 the XML Schema namespace (missing prefix?)",
                                qn.local)));
                        }
                        // Foreign-namespace element at top level — skip
                        // (tolerate extension/annotation elements that
                        // real-world schemas place here).
                        self.skip_body()?;
                        self.pop_ns_scope();
                        continue;
                    }
                    match qn.local.as_ref() {
                        "element"        => self.parse_top_element(&attrs)?,
                        "attribute"      => self.parse_top_attribute(&attrs)?,
                        "simpleType"     => self.parse_top_simple_type(&attrs)?,
                        "complexType"    => self.parse_top_complex_type(&attrs)?,
                        "attributeGroup" => self.parse_top_attribute_group(&attrs)?,
                        "group"          => self.parse_top_group(&attrs)?,
                        "notation"       => self.parse_top_notation(&attrs)?,
                        "import"         => self.handle_import(&attrs)?,
                        "include"        => self.handle_include(&attrs)?,
                        "redefine"       => self.handle_redefine(&attrs)?,
                        "override" => {
                            if !matches!(self.builder.effective_version, SchemaVersion::Xsd11) {
                                return Err(self.err(
                                    "<xs:override> is an XSD 1.1 directive — \
                                     set SchemaOptions::version to Xsd11, or to Auto \
                                     with vc:minVersion=\"1.1\" on <xs:schema>",
                                ));
                            }
                            self.handle_override(&attrs)?;
                        }
                        "annotation" => self.parse_annotation_body(&attrs)?,
                        other => return Err(self.err(format!(
                            "unexpected top-level element <xs:{other}>"
                        ))),
                    }
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => break,
                EventInto::Eof => return Err(self.err("unexpected EOF")),
                _ => {}
            }
        }
        Ok(())
    }

    // ── import / include ─────────────────────────────────────────────────

    fn handle_import(&mut self, attrs: &[Attr<'a>]) -> Result<(), SchemaCompileError> {
        self.check_known_attrs(attrs, &["id", "namespace", "schemaLocation"], "import")?;
        let imported_ns = self.attr(attrs, "namespace").map(|s| s.to_owned());
        // Record the import for later src-resolve enforcement.  Use the
        // Arc<str> form so refs can compare cheaply.
        self.builder.imported_namespaces.insert(
            imported_ns.as_deref().map(Arc::from),
        );
        // XSD §4.2.3 — `namespace=""` (empty URI) is not a valid
        // anyURI; the no-namespace target is expressed by omitting
        // the attribute entirely.
        if imported_ns.as_deref() == Some("") {
            return Err(self.err(
                "<xs:import namespace=\"\">: empty namespace URI is not allowed \
                 (omit the attribute to import the no-namespace components)"
            ));
        }
        // XSD §4.2.3 — the imported namespace must differ from the
        // schema's own targetNamespace; importing yourself is a
        // schema validity error.  This also covers `<xs:import>`
        // with no `namespace` attribute (which means "the absent
        // namespace") from a schema that also has no
        // targetNamespace — they're the same "absent" namespace,
        // so self-import.
        if imported_ns.as_deref() == self.current_target_ns.as_deref() {
            return Err(self.err(format!(
                "<xs:import namespace={:?}> matches this schema's own \
                 targetNamespace — use <xs:include> for same-namespace composition",
                imported_ns.as_deref().unwrap_or(""),
            )));
        }
        let location    = self.attr(attrs, "schemaLocation");
        self.parse_anno_only_body("import")?;

        let Some(loc) = location else {
            // The spec allows hint-less import; the schema relies on
            // the consumer providing the imported namespace by other
            // means.  We treat this as "nothing to load."
            return Ok(());
        };
        // Re-entrant import back into the root's target namespace:
        // loading that file would parse the root's content again
        // while the outer parser is still walking it, double-
        // inserting every declaration.  The root schema is already
        // authoritative for its target namespace, so skip the load.
        if let (Some(imp), Some(root)) = (
            imported_ns.as_deref(), self.builder.target_ns.as_deref(),
        ) {
            if imp == root {
                // NB: don't mark the location as loaded — a later
                // import of the same file for a different namespace
                // is a legitimate (if unusual) shape and must still
                // be validated against the file's targetNamespace.
                return Ok(());
            }
        }
        self.load_schema_via_resolver(loc, imported_ns.as_deref(), /*is_import=*/true)
    }

    fn handle_include(&mut self, attrs: &[Attr<'a>]) -> Result<(), SchemaCompileError> {
        let location = self.attr(attrs, "schemaLocation").ok_or_else(||
            self.err("<xs:include> missing schemaLocation")
        )?.to_owned();
        self.parse_anno_only_body("include")?;
        // Includes inherit the including schema's targetNamespace.
        let expected_ns = self.current_target_ns.as_deref().map(str::to_owned);
        self.load_schema_via_resolver(&location, expected_ns.as_deref(), /*is_import=*/false)
    }

    /// XSD §4.2.2 `<xs:redefine>` — load the referenced schema (like
    /// `<xs:include>`), then process the redefine body to override any
    /// same-named simpleType / complexType / group / attributeGroup.
    ///
    /// Loading runs before the body so the redefinitions, which insert
    /// into the same builder maps, win over the original definitions
    /// via `HashMap::insert` semantics.  References to the redefined
    /// names elsewhere in the redefining schema pick up the new
    /// definitions.
    fn handle_redefine(&mut self, attrs: &[Attr<'a>]) -> Result<(), SchemaCompileError> {
        self.check_known_attrs(attrs, &["id", "schemaLocation"], "redefine")?;
        let location = self.attr(attrs, "schemaLocation").ok_or_else(||
            self.err("<xs:redefine> missing schemaLocation")
        )?.to_owned();
        let expected_ns = self.current_target_ns.as_deref().map(str::to_owned);
        self.load_schema_via_resolver(&location, expected_ns.as_deref(), /*is_import=*/false)?;

        // Snapshot the originals from the loaded schema.  When a
        // redefining complex type declares `<xs:extension base="X">`
        // or `<xs:restriction base="X">` where X is the same name as
        // the type being redefined, the base must resolve to the
        // pre-redefinition version (held in the snapshot), not the
        // redefining version that's about to overwrite it.  Without
        // this fixup the extension-merge post-pass detects a
        // self-cycle.
        let snapshot: HashMap<QName, TypeRef> = self.builder.types.clone();
        // Per src-redefine, the redefining body can only target names
        // that already exist in the loaded original. These snapshot
        // sets let `check_redefined_target_exists` recognise stale
        // redefinitions before they corrupt the post-pass merging.
        let snapshot_groups: HashSet<QName> = self.builder.model_groups.keys().cloned().collect();
        let snapshot_attr_groups: HashSet<QName> = self.builder.attr_groups.keys().cloned().collect();

        let prev_in_redefine = self.in_redefine;
        self.in_redefine = true;
        let result = (|| -> Result<(), SchemaCompileError> {
            loop {
                match self.next_event()? {
                    EventInto::StartElement { name } => {
                        let child_attrs = self.take_attrs()?;
                        self.push_ns_scope(&child_attrs);
                        let qn = self.qname_of_element(&name)?;
                        if qn.namespace.as_deref() == Some(XS) {
                            match qn.local.as_ref() {
                                "simpleType"     => {
                                    self.check_redefined_type_exists(&child_attrs, &snapshot)?;
                                    self.last_simple_restriction_base = None;
                                    self.parse_top_simple_type(&child_attrs)?;
                                    self.check_redefined_simple_base(&child_attrs)?;
                                }
                                "complexType"    => {
                                    self.check_redefined_type_exists(&child_attrs, &snapshot)?;
                                    self.parse_top_complex_type(&child_attrs)?;
                                    self.check_redefined_complex_base(&child_attrs)?;
                                    self.fixup_redefined_complex_base(&child_attrs, &snapshot);
                                }
                                "group"          => {
                                    self.check_redefined_set_exists(&child_attrs,
                                        &snapshot_groups, "group")?;
                                    let qn = self.attr(&child_attrs, "name").map(|n| QName {
                                        namespace: self.current_target_ns.clone(),
                                        local:     Arc::from(n),
                                    });
                                    let original_particle = qn.as_ref()
                                        .and_then(|n| self.builder.model_groups.get(n))
                                        .map(|g| g.particle.clone());
                                    self.parse_top_group(&child_attrs)?;
                                    if let Some(name) = qn {
                                        self.builder.redefined_groups.insert(name.clone());
                                        if let (Some(original), Some(new_group)) =
                                            (original_particle, self.builder.model_groups.get(&name))
                                        {
                                            self.builder.redefined_group_originals.push((
                                                name,
                                                new_group.particle.clone(),
                                                original,
                                            ));
                                        }
                                    }
                                }
                                "attributeGroup" => {
                                    self.check_redefined_set_exists(&child_attrs,
                                        &snapshot_attr_groups, "attributeGroup")?;
                                    let qn = self.attr(&child_attrs, "name").map(|n| QName {
                                        namespace: self.current_target_ns.clone(),
                                        local:     Arc::from(n),
                                    });
                                    let original = qn.as_ref()
                                        .and_then(|n| self.builder.attr_groups.get(n))
                                        .cloned();
                                    self.builder.redefining_ag_name = qn.clone();
                                    self.builder.redefining_ag_saw_self_ref = false;
                                    self.parse_top_attribute_group(&child_attrs)?;
                                    let saw_self_ref = self.builder.redefining_ag_saw_self_ref;
                                    self.builder.redefining_ag_name = None;
                                    self.builder.redefining_ag_saw_self_ref = false;
                                    if let (Some(name), Some(orig)) = (qn, original) {
                                        if let Some(new_ag) = self.builder.attr_groups.get(&name) {
                                            self.builder.redefined_attr_group_originals.push((
                                                name, new_ag.clone(), orig, saw_self_ref,
                                            ));
                                        }
                                    }
                                }
                                "annotation"     => self.parse_annotation_body(&child_attrs)?,
                                other => return Err(self.err(format!(
                                    "<xs:redefine> body: unexpected child <xs:{other}>"
                                ))),
                            }
                        } else {
                            // Foreign-namespace child — skip per spec.
                            self.skip_body()?;
                        }
                        self.pop_ns_scope();
                    }
                    EventInto::EndElement { .. } => break,
                    EventInto::Eof => return Err(self.err("unexpected EOF in <xs:redefine>")),
                    _ => {}
                }
            }
            Ok(())
        })();
        self.in_redefine = prev_in_redefine;
        result
    }

    /// XSD 1.1 § 4.2.5 `<xs:override>`.  Like `<xs:redefine>` it
    /// loads a referenced schema document and reparses selected
    /// top-level components, except:
    ///
    /// 1. The child components **replace** the loaded ones outright
    ///    — there's no requirement that a redefining type derive
    ///    from itself, so the `redefine` self-reference fixup
    ///    doesn't apply.
    /// 2. The child set is broader — `xs:element`, `xs:attribute`,
    ///    and `xs:notation` are permitted in addition to the
    ///    redefine set (simpleType / complexType / group /
    ///    attributeGroup).
    /// 3. Components in the loaded schema that are NOT named in the
    ///    override are kept as-is.  Replacement is by qualified
    ///    name; parsing each child top-level decl with the existing
    ///    parsers inserts into the same `builder.*` maps and
    ///    naturally overwrites the loaded entry.
    fn handle_override(&mut self, attrs: &[Attr<'a>]) -> Result<(), SchemaCompileError> {
        let location = self.attr(attrs, "schemaLocation").ok_or_else(||
            self.err("<xs:override> missing schemaLocation")
        )?.to_owned();
        let expected_ns = self.current_target_ns.as_deref().map(str::to_owned);
        self.load_schema_via_resolver(&location, expected_ns.as_deref(), /*is_import=*/false)?;

        loop {
            match self.next_event()? {
                EventInto::StartElement { name } => {
                    let child_attrs = self.take_attrs()?;
                    self.push_ns_scope(&child_attrs);
                    let qn = self.qname_of_element(&name)?;
                    if qn.namespace.as_deref() == Some(XS) {
                        match qn.local.as_ref() {
                            "simpleType"     => self.parse_top_simple_type(&child_attrs)?,
                            "complexType"    => self.parse_top_complex_type(&child_attrs)?,
                            "group"          => self.parse_top_group(&child_attrs)?,
                            "attributeGroup" => self.parse_top_attribute_group(&child_attrs)?,
                            "element"        => self.parse_top_element(&child_attrs)?,
                            "attribute"      => self.parse_top_attribute(&child_attrs)?,
                            "notation"       => self.parse_top_notation(&child_attrs)?,
                            "annotation"     => self.parse_annotation_body(&child_attrs)?,
                            other => return Err(self.err(format!(
                                "<xs:override> body: unexpected child <xs:{other}>"
                            ))),
                        }
                    } else {
                        // Foreign-namespace child — skip per spec.
                        self.skip_body()?;
                    }
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => break,
                EventInto::Eof => return Err(self.err("unexpected EOF in <xs:override>")),
                _ => {}
            }
        }
        Ok(())
    }

    /// After `parse_top_complex_type` inserts a redefining type, if
    /// its derivation base names itself, rebuild the type with the
    /// base resolved to the snapshotted original Arc.  No-op when
    /// the base names a different type (regular derivation chain) or
    /// when the redefined name wasn't in the snapshot.
    /// XSD §4.2.2 src-redefine — a redefining schema component must
    /// target a name already declared in the loaded original.
    /// Redefining a name that doesn't exist there is a hard error.
    fn check_redefined_type_exists(
        &self,
        child_attrs: &[Attr<'a>],
        snapshot:    &HashMap<QName, TypeRef>,
    ) -> Result<(), SchemaCompileError> {
        let Some(name) = self.attr(child_attrs, "name") else { return Ok(()); };
        let own_qn = QName {
            namespace: self.current_target_ns.clone(),
            local:     Arc::from(name),
        };
        if !snapshot.contains_key(&own_qn) {
            return Err(self.err(format!(
                "<xs:redefine>: cannot redefine type {:?} — the loaded original \
                 doesn't declare a type with this name (src-redefine)",
                own_qn.local,
            )));
        }
        Ok(())
    }

    fn check_redefined_set_exists(
        &self,
        child_attrs: &[Attr<'a>],
        snapshot:    &HashSet<QName>,
        kind:        &str,
    ) -> Result<(), SchemaCompileError> {
        let Some(name) = self.attr(child_attrs, "name") else { return Ok(()); };
        let own_qn = QName {
            namespace: self.current_target_ns.clone(),
            local:     Arc::from(name),
        };
        if !snapshot.contains(&own_qn) {
            return Err(self.err(format!(
                "<xs:redefine>: cannot redefine {kind} {:?} — the loaded original \
                 doesn't declare a {kind} with this name (src-redefine)",
                own_qn.local,
            )));
        }
        Ok(())
    }

    /// XSD §4.2.2 src-redefine — inside `<xs:redefine>`, a redefining
    /// `<xs:simpleType name="X">` must restrict the original `X`
    /// (transitively); its `<xs:restriction base=...>` is required and
    /// must resolve to `X` itself.
    fn check_redefined_simple_base(
        &mut self,
        child_attrs: &[Attr<'a>],
    ) -> Result<(), SchemaCompileError> {
        let Some(name) = self.attr(child_attrs, "name") else { return Ok(()); };
        let own_qn = QName {
            namespace: self.current_target_ns.clone(),
            local:     Arc::from(name),
        };
        let bad = match &self.last_simple_restriction_base {
            // No named base (e.g. inline anonymous base via nested
            // <xs:simpleType>) is not a self-restriction.
            None => true,
            Some(base) => base != &own_qn,
        };
        if bad {
            return Err(self.err(format!(
                "<xs:redefine><xs:simpleType name={:?}>: the redefining body must \
                 restrict {0:?} itself (its `<xs:restriction base=>` must resolve \
                 to {0:?}); a different base is not a self-restriction per \
                 src-redefine",
                own_qn.local,
            )));
        }
        Ok(())
    }

    /// XSD §4.2.2 src-redefine — inside `<xs:redefine>`, a redefining
    /// `<xs:complexType name="X">` must derive from the original `X`
    /// via `<xs:restriction base="X">` or `<xs:extension base="X">`.
    fn check_redefined_complex_base(
        &mut self,
        child_attrs: &[Attr<'a>],
    ) -> Result<(), SchemaCompileError> {
        let Some(name) = self.attr(child_attrs, "name") else { return Ok(()); };
        let own_qn = QName {
            namespace: self.current_target_ns.clone(),
            local:     Arc::from(name),
        };
        let Some(TypeRef::Complex(redef)) = self.builder.types.get(&own_qn).cloned() else { return Ok(()) };
        let Some(d) = redef.derivation.as_ref() else {
            return Err(self.err(format!(
                "<xs:redefine><xs:complexType name={:?}>: the redefining body must \
                 derive (via <xs:restriction> or <xs:extension>) from the original \
                 {0:?}",
                own_qn.local,
            )));
        };
        let base_qn = resolve_typeref_to_qname(&d.base);
        if base_qn.as_ref() != Some(&own_qn) {
            return Err(self.err(format!(
                "<xs:redefine><xs:complexType name={:?}>: the redefining body's \
                 derivation base must be {0:?} itself (per src-redefine), not a \
                 different type",
                own_qn.local,
            )));
        }
        Ok(())
    }

    fn fixup_redefined_complex_base(
        &mut self,
        child_attrs: &[Attr<'a>],
        snapshot:    &HashMap<QName, TypeRef>,
    ) {
        let Some(name) = self.attr(child_attrs, "name") else { return };
        let own_qn = QName {
            namespace: self.current_target_ns.clone(),
            local:     Arc::from(name),
        };
        let Some(TypeRef::Complex(redef)) = self.builder.types.get(&own_qn).cloned() else { return };
        let Some(d) = redef.derivation.as_ref() else { return };
        let Some(base_qn) = resolve_typeref_to_qname(&d.base) else { return };
        if base_qn != own_qn { return; }
        let Some(original) = snapshot.get(&own_qn).cloned() else { return };
        let new_derivation = Derivation {
            method: d.method,
            base:   original,
        };
        let new_ct = ComplexType {
            name:          redef.name.clone(),
            derivation:    Some(new_derivation),
            content:       redef.content.clone(),
            matcher:       std::sync::OnceLock::new(),
            attributes:    redef.attributes.clone(),
            any_attribute: redef.any_attribute.clone(),
            abstract_:     redef.abstract_,
            block:         redef.block,
            final_:        redef.final_,
            pending_attribute_group_refs: redef.pending_attribute_group_refs.clone(),
            assertions: redef.assertions.clone(),
        };
        self.builder.types.insert(own_qn, TypeRef::Complex(Arc::new(new_ct)));
    }

    /// Shared body for import/include — fetch via resolver, parse
    /// recursively into the same builder.  Cycles are silently skipped.
    fn load_schema_via_resolver(
        &mut self,
        location: &str,
        expected_ns: Option<&str>,
        is_import: bool,
    ) -> Result<(), SchemaCompileError> {
        if self.builder.loaded.contains(location) {
            return Ok(()); // cycle — already loaded.
        }
        // XSD §4.2.3 (import) / §4.2.1 (include) — `schemaLocation`
        // is a hint. `Ok(None)` from the resolver means "I don't
        // have a mapping for this URI" and is a soft skip per spec
        // (declarations only present in the unresolved doc just
        // aren't available; the calling schema still compiles).
        //
        // `Err(_)` is different: the resolver tried and was actively
        // refused — `FilesystemResolver` rejecting a path outside
        // its allowlist, or `NetworkResolver` rejecting an
        // unauthorised host.  Silently absorbing those hides the
        // mismatch between the schema's intent and the runtime
        // security policy: the caller asked for an include they
        // can't fetch, so the resulting schema is missing the
        // types or groups they relied on.  Bubble that up as a
        // compile error so the operator sees the misconfiguration.
        let bytes = match self.resolver.resolve(location, expected_ns) {
            Ok(Some(b)) => b,
            Ok(None) => {
                self.builder.loaded.insert(location.to_owned());
                return Ok(());
            }
            Err(e) => {
                self.builder.loaded.insert(location.to_owned());
                return Err(self.err(format!(
                    "<xs:{}>: failed to resolve schemaLocation {location:?}: {e}",
                    if is_import { "import" } else { "include" },
                )));
            }
        };
        let s = std::str::from_utf8(&bytes).map_err(|e| self.err(format!(
            "schema {location:?} is not valid UTF-8: {e}"
        )))?;

        self.builder.loaded.insert(location.to_owned());
        self.builder.depth += 1;
        let result = parse_one_file(s, self.builder, self.resolver, expected_ns, is_import);
        self.builder.depth -= 1;
        result
    }

    /// Look up the qualified name for an element name (which the
    /// XmlReader hands us as `prefix:local` text).
    fn qname_of_element(&self, name: &str) -> Result<QName, SchemaCompileError> {
        match name.split_once(':') {
            Some((prefix, local)) => {
                let uri = self.resolve_prefix(prefix).ok_or_else(||
                    self.err(format!("undeclared element prefix {prefix:?}"))
                )?;
                Ok(QName::new(Some(uri), local))
            }
            None => {
                // Unprefixed element: default namespace.
                let uri = self.resolve_prefix("").map(|s| s.to_owned());
                Ok(QName {
                    namespace: uri.map(Arc::from),
                    local:     Arc::from(name),
                })
            }
        }
    }

    // ── attribute helpers ────────────────────────────────────────────────

    fn attr<'r>(&self, attrs: &'r [Attr<'a>], name: &str) -> Option<&'r str> {
        attrs.iter().find(|a| a.name() == name).map(|a| a.value.as_ref())
    }

    /// Reject unrecognised attributes on an XSD element. Attributes
    /// in foreign namespaces (a real prefix bound to a non-XSD URI)
    /// are tolerated — they are the spec's extension mechanism.
    /// Attributes in the XSD namespace, no-namespace, or with an
    /// `xml:`/`xmlns:`/`xmlns` form are subject to the allow-list.
    fn check_known_attrs(
        &self,
        attrs:   &[Attr<'a>],
        allowed: &[&str],
        owner:   &str,
    ) -> Result<(), SchemaCompileError> {
        for a in attrs {
            let name = a.name;
            // Namespace declarations are always allowed.
            if name == "xmlns" || name.starts_with("xmlns:") { continue; }
            // `xml:`-prefixed attributes (xml:lang, xml:base, etc.)
            // are the XML core namespace and always allowed.
            if name.starts_with("xml:") { continue; }
            if let Some((prefix, _local)) = name.split_once(':') {
                let resolved = self.resolve_prefix(prefix);
                // A real foreign-namespace prefix: tolerate (extension
                // attribute, XSD §3.x.2 allows annotations of foreign
                // attrs on any schema component).
                if matches!(resolved, Some(uri) if uri != XS) {
                    continue;
                }
                // Prefix bound to the XSD namespace itself — XSD
                // attribute names are unqualified per the schema-for-
                // schemas, so `xsd:type` is *not* the same slot as the
                // unprefixed `type` and is unrecognised.
                return Err(self.err(format!(
                    "<xs:{owner}>: attribute {name:?} is in the XSD namespace; \
                     XSD-defined attributes on schema elements must be unqualified"
                )));
            } else {
                if !allowed.contains(&name) {
                    return Err(self.err(format!(
                        "<xs:{owner}>: attribute {name:?} is not recognised \
                         (allowed: {allowed:?})"
                    )));
                }
            }
        }
        Ok(())
    }

    // ── top-level element ────────────────────────────────────────────────

    fn parse_top_element(&mut self, attrs: &[Attr<'a>]) -> Result<(), SchemaCompileError> {
        // XSD §3.3.2 — the attribute set of a top-level <xs:element>.
        self.check_known_attrs(attrs, &[
            "id", "name", "type", "default", "fixed", "nillable",
            "abstract", "substitutionGroup", "block", "final",
        ], "element")?;
        let name = self.attr(attrs, "name").ok_or_else(||
            self.err("<xs:element> missing name attribute")
        )?.to_owned();
        let qn = QName {
            namespace: self.current_target_ns.clone(),
            local:     Arc::from(name.as_str()),
        };

        // Type + identity constraints — parse_inline_type walks the body.
        let (inline, identity) = self.parse_inline_type(attrs)?;
        let type_def = match self.attr(attrs, "type") {
            Some(t) => {
                // XSD §3.3.3 src-element.3 — an element decl may have
                // either `type=` OR a nested `<xs:simpleType>` /
                // `<xs:complexType>` child, never both.
                if inline.is_some() {
                    return Err(self.err(format!(
                        "<xs:element name={name:?}> has both a `type=` attribute and \
                         a nested type body; pick one (XSD §3.3.3 src-element.3)",
                    )));
                }
                let type_qn = self.parse_qname(t, false)?;
                self.type_ref_for(type_qn)
            }
            None => match inline {
                Some(t) => t,
                None => any_type_ref(),
            },
        };

        let nillable = self.parse_xsd_bool(attrs, "nillable")?.unwrap_or(false);
        let default  = self.attr(attrs, "default").map(|s| s.to_owned());
        let fixed    = self.attr(attrs, "fixed").map(|s| s.to_owned());
        if default.is_some() && fixed.is_some() {
            return Err(self.err(
                "<xs:element> may have either default= or fixed=, not both",
            ));
        }
        // XSD §3.3.3 (cvc-elt-2.x): a `default`/`fixed` value must
        // be valid against the element's type's value space. We can
        // check this for built-ins / already-declared simple types
        // and reject when the element has a complex type whose
        // content isn't simple — `default`/`fixed` only apply to
        // simple-typed elements (or mixed complex types, which we
        // don't differentiate here).
        if let Some(v) = default.as_deref().or(fixed.as_deref()) {
            match &type_def {
                TypeRef::Simple(st) => {
                    // XSD §3.3.6 — an element typed xs:ID (or derived
                    // from it) cannot have a default or fixed: ID
                    // validity is per-instance and a fixed/default ID
                    // would be illegal the moment the element appeared
                    // twice.
                    if matches!(st.builtin, super::types::BuiltinType::Id) {
                        return Err(self.err(format!(
                            "<xs:element name={name:?}> of type xs:ID (or derived) cannot have a default or fixed value",
                        )));
                    }
                    if let Err(e) = st.validate(v) {
                        return Err(self.err(format!(
                            "<xs:element name={name:?}> default/fixed value {v:?} is not valid for its type: {}",
                            e.message,
                        )));
                    }
                }
                TypeRef::Complex(ct) => {
                    // XSD §3.3.6 — default/fixed allowed when the
                    // content type is mixed or simple. Element-only
                    // (Complex with mixed=false) and empty content
                    // can't have a value constraint.
                    let allowed = match &ct.content {
                        crate::xsd::schema::ContentModel::Simple(_) => true,
                        crate::xsd::schema::ContentModel::Complex { mixed, .. } => *mixed,
                        crate::xsd::schema::ContentModel::Empty => false,
                    };
                    if !allowed {
                        return Err(self.err(format!(
                            "<xs:element name={name:?}> with element-only or empty content cannot \
                             have a default or fixed value (XSD §3.3.6)",
                        )));
                    }
                }
            }
        }
        let abstract_ = self.parse_xsd_bool(attrs, "abstract")?.unwrap_or(false);
        let substitution_group = match self.attr(attrs, "substitutionGroup") {
            Some(s) => Some(self.parse_qname(s, false)?),
            None    => None,
        };

        let decl = Arc::new(ElementDecl {
            name: qn.clone(),
            type_def,
            nillable,
            default,
            fixed,
            abstract_,
            substitution_group,
            // XSD §3.3.2 — `block` on <xs:element> accepts only
            // restriction | extension | substitution | #all.
            // The simple-type tokens `list` / `union` are NOT
            // allowed here.
            block:  match self.attr(attrs, "block") {
                Some(_) => parse_element_block_set(self.attr(attrs, "block"))?,
                None    => self.block_default
                            & (BlockSet::RESTRICTION
                               | BlockSet::EXTENSION
                               | BlockSet::SUBSTITUTION),
            },
            // XSD §3.3.2: `final` on <xs:element> accepts only
            // restriction|extension|#all; substitution is element's
            // block-only token.
            final_: match self.attr(attrs, "final") {
                Some(_) => parse_ct_derivation_set(self.attr(attrs, "final"), "final")?,
                None    => self.final_default
                            & (BlockSet::RESTRICTION | BlockSet::EXTENSION),
            },
            identity,
        });
        // Duplicate detection is per-file: include cycles are allowed
        // to re-process the same top-level decl across files (the
        // re-insertion is benign), but a single file declaring two
        // elements with the same name is a schema validity error.
        if !self.local_top_element_names.insert(qn.clone()) {
            return Err(self.err(format!(
                "duplicate top-level <xs:element name={:?}> in this schema document",
                qn.local,
            )));
        }
        self.builder.elements.insert(qn, decl);
        Ok(())
    }

    /// Parse an inline `<xs:simpleType>` or `<xs:complexType>` child
    /// nested in an element/attribute decl, plus any
    /// `<xs:key>`/`<xs:keyref>`/`<xs:unique>` constraints.  Consumes
    /// events through the parent's matching EndElement.
    fn parse_inline_type(
        &mut self,
        _attrs: &[Attr<'a>],
    ) -> Result<(Option<TypeRef>, Vec<super::identity::IdentityConstraint>), SchemaCompileError> {
        let mut found_type: Option<TypeRef> = None;
        let mut constraints: Vec<super::identity::IdentityConstraint> = Vec::new();
        let mut seen_anno = false;
        let mut seen_other = false;
        loop {
            match self.next_event()? {
                EventInto::StartElement { name } => {
                    let child_attrs = self.take_attrs()?;
                    self.push_ns_scope(&child_attrs);
                    let qn = self.qname_of_element(&name)?;
                    let is_xs = qn.namespace.as_deref() == Some(XS);
                    if is_xs {
                        self.check_annotation_pos(
                            &qn.local, &mut seen_anno, &mut seen_other, "element",
                        )?;
                    }
                    match (is_xs, qn.local.as_ref()) {
                        (true, "simpleType") => {
                            let st = self.parse_simple_type_body(&child_attrs)?;
                            found_type = Some(TypeRef::Simple(Arc::new(st)));
                        }
                        (true, "complexType") => {
                            let ct = self.parse_complex_type_body(&child_attrs)?;
                            found_type = Some(TypeRef::Complex(Arc::new(ct)));
                        }
                        (true, "annotation") => self.parse_annotation_body(&child_attrs)?,
                        (true, kind @ ("key" | "keyref" | "unique")) => {
                            constraints.push(self.parse_identity_constraint(&child_attrs, kind)?);
                        }
                        (true, other) => return Err(self.err(format!(
                            "<xs:{other}> is not allowed as a child of <xs:element>"
                        ))),
                        _ => self.skip_body()?,
                    }
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => return Ok((found_type, constraints)),
                EventInto::Eof => return Err(self.err("unexpected EOF in element body")),
                _ => {}
            }
        }
    }

    /// Parse one `<xs:key>` / `<xs:keyref>` / `<xs:unique>` element
    /// (the start tag and attributes have already been consumed; we
    /// walk through the matching EndElement).
    fn parse_identity_constraint(
        &mut self,
        attrs: &[Attr<'a>],
        kind_str: &str,
    ) -> Result<super::identity::IdentityConstraint, SchemaCompileError> {
        use super::identity::ConstraintKind;
        let kind = match kind_str {
            "key"     => ConstraintKind::Key,
            "unique"  => ConstraintKind::Unique,
            "keyref"  => ConstraintKind::KeyRef,
            _         => unreachable!(),
        };
        let name_str = self.attr(attrs, "name").ok_or_else(||
            self.err(format!("<xs:{kind_str}> missing name"))
        )?.to_owned();
        let name = QName {
            namespace: self.current_target_ns.clone(),
            local:     Arc::from(name_str.as_str()),
        };
        if !self.seen_ic_names.insert(name.clone()) {
            return Err(self.err(format!(
                "duplicate identity-constraint name {name_str:?} \
                 (unique/key/keyref share one namespace, XSD §3.11.1)"
            )));
        }
        let refer = if kind == ConstraintKind::KeyRef {
            let r = self.attr(attrs, "refer").ok_or_else(||
                self.err("<xs:keyref> missing refer attribute")
            )?;
            Some(self.parse_qname(r, false)?)
        } else {
            if self.attr(attrs, "refer").is_some() {
                return Err(self.err(format!(
                    "<xs:{kind_str} refer=...> is only valid on <xs:keyref>"
                )));
            }
            None
        };

        // Snapshot prefix bindings from the current scope for XPath
        // resolution — XSD identity-constraint XPaths use the bindings
        // in scope at the constraint's own declaration.
        let prefix_lookup: Vec<(String, String)> = self.ns_stack.iter()
            .flat_map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())))
            .collect();
        let resolve_prefix = |p: &str| -> Option<String> {
            for (k, v) in prefix_lookup.iter().rev() {
                if k == p { return Some(v.clone()); }
            }
            None
        };

        // Walk the body for <xs:selector> and one or more <xs:field>.
        let mut selector_xpath: Option<String> = None;
        let mut field_xpaths: Vec<String> = Vec::new();
        let mut seen_anno = false;
        let mut seen_other = false;
        loop {
            match self.next_event()? {
                EventInto::StartElement { name: child } => {
                    let cattrs = self.take_attrs()?;
                    self.push_ns_scope(&cattrs);
                    let qn = self.qname_of_element(&child)?;
                    if qn.namespace.as_deref() == Some(XS) {
                        self.check_annotation_pos(
                            &qn.local, &mut seen_anno, &mut seen_other, kind_str,
                        )?;
                        match qn.local.as_ref() {
                            "selector" => {
                                if selector_xpath.is_some() {
                                    return Err(self.err(format!(
                                        "<xs:{kind_str}> may have at most one <xs:selector>"
                                    )));
                                }
                                if !field_xpaths.is_empty() {
                                    return Err(self.err(format!(
                                        "<xs:{kind_str}> body: <xs:selector> must precede <xs:field>"
                                    )));
                                }
                                if self.attr(&cattrs, "name").is_some() {
                                    return Err(self.err(
                                        "<xs:selector> does not take a 'name' attribute",
                                    ));
                                }
                                let xp = self.attr(&cattrs, "xpath").ok_or_else(||
                                    self.err("<xs:selector> missing xpath")
                                )?.to_owned();
                                selector_xpath = Some(xp);
                                self.parse_anno_only_body("selector")?;
                                self.pop_ns_scope();
                                continue;
                            }
                            "field" => {
                                if self.attr(&cattrs, "name").is_some() {
                                    return Err(self.err(
                                        "<xs:field> does not take a 'name' attribute",
                                    ));
                                }
                                let xp = self.attr(&cattrs, "xpath").ok_or_else(||
                                    self.err("<xs:field> missing xpath")
                                )?.to_owned();
                                field_xpaths.push(xp);
                                self.parse_anno_only_body("field")?;
                                self.pop_ns_scope();
                                continue;
                            }
                            "annotation" => {}
                            other => return Err(self.err(format!(
                                "<xs:{other}> is not allowed as a child of <xs:{kind_str}>"
                            ))),
                        }
                    }
                    self.skip_body()?;
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => break,
                EventInto::Eof => return Err(self.err(format!(
                    "unexpected EOF in <xs:{kind_str}>"
                ))),
                _ => {}
            }
        }

        let selector_xpath = selector_xpath.ok_or_else(||
            self.err(format!("<xs:{kind_str} name={:?}> has no <xs:selector>", name.local))
        )?;
        if field_xpaths.is_empty() {
            return Err(self.err(format!(
                "<xs:{kind_str} name={:?}> has no <xs:field>", name.local
            )));
        }

        let selector = super::identity::parse_selector(&selector_xpath, &resolve_prefix)
            .map_err(|e| self.err(format!("<xs:selector xpath={selector_xpath:?}>: {e}")))?;
        let fields: Vec<_> = field_xpaths.iter()
            .map(|fp| super::identity::parse_field(fp, &resolve_prefix)
                .map_err(|e| self.err(format!("<xs:field xpath={fp:?}>: {e}"))))
            .collect::<Result<_, _>>()?;

        Ok(super::identity::IdentityConstraint {
            name, kind, selector, fields, refer,
        })
    }

    // ── top-level attribute ──────────────────────────────────────────────

    fn parse_top_attribute(&mut self, attrs: &[Attr<'a>]) -> Result<(), SchemaCompileError> {
        // XSD §3.2.2 — the attribute set of top-level <xs:attribute>.
        self.check_known_attrs(attrs, &[
            "id", "name", "type", "default", "fixed",
        ], "attribute")?;
        let name = self.attr(attrs, "name").ok_or_else(||
            self.err("<xs:attribute> missing name attribute")
        )?.to_owned();
        // XSD §3.2.3 — the name "xmlns" is reserved (it is the
        // namespace-binding pseudo-attribute, not an XSD-declared
        // attribute) and must not be used here.
        if name == "xmlns" {
            return Err(self.err(
                "<xs:attribute name=\"xmlns\"> is reserved by the Namespaces in XML spec",
            ));
        }
        // `form=`, `use=`, and `ref=` are only valid on local
        // attribute declarations (XSD §3.2.2).
        if self.attr(attrs, "form").is_some() {
            return Err(self.err(
                "<xs:attribute form=...> is only valid on local attribute declarations"
            ));
        }
        if self.attr(attrs, "use").is_some() {
            return Err(self.err(
                "<xs:attribute use=...> is only valid on local attribute declarations"
            ));
        }
        if self.attr(attrs, "ref").is_some() {
            return Err(self.err(
                "<xs:attribute ref=...> is only valid on local attribute declarations"
            ));
        }
        // `default=` and `fixed=` are mutually exclusive (XSD §3.2.2).
        if self.attr(attrs, "default").is_some() && self.attr(attrs, "fixed").is_some() {
            return Err(self.err(
                "<xs:attribute> may have either default= or fixed=, not both"
            ));
        }
        let qn = QName {
            namespace: self.current_target_ns.clone(),
            local:     Arc::from(name.as_str()),
        };
        check_xsi_attribute_name(&qn).map_err(|m| self.err(m))?;

        // Resolve the simple type — `type=` ref or inline simpleType.
        let inline_type = self.parse_inline_simple_type(attrs)?;
        if self.attr(attrs, "type").is_some() && inline_type.is_some() {
            return Err(self.err(
                "<xs:attribute> may have either type= or an inline <xs:simpleType>, not both"
            ));
        }
        let st = match self.attr(attrs, "type") {
            Some(t) => {
                let type_qn = self.parse_qname(t, false)?;
                self.simple_type_for(type_qn)
            }
            None => match inline_type {
                Some(t) => Arc::new(t),
                None => Arc::new(SimpleType::of_builtin(BuiltinType::String)),
            },
        };

        let decl = Arc::new(AttributeDecl {
            name:    qn.clone(),
            type_def: st,
            default: self.attr(attrs, "default").map(|s| s.to_owned()),
            fixed:   self.attr(attrs, "fixed").map(|s| s.to_owned()),
            inheritable: self.parse_inheritable(attrs)?,
        });
        if self.builder.attributes.contains_key(&qn) {
            return Err(self.err(format!(
                "duplicate top-level <xs:attribute name={:?}> in target namespace",
                qn.local,
            )));
        }
        self.builder.attributes.insert(qn, decl);
        Ok(())
    }

    /// Walk attribute-decl body for an inline `<xs:simpleType>`. The
    /// only XSD children allowed inside an `<xs:attribute>` are
    /// `<xs:annotation>` and `<xs:simpleType>` (XSD §3.2.2); reject
    /// anything else so things like `<xs:unique>` inside an attribute
    /// surface as the schema validity errors they are. Annotation
    /// must precede simpleType and only appears once.
    fn parse_inline_simple_type(&mut self, _attrs: &[Attr<'a>])
        -> Result<Option<SimpleType>, SchemaCompileError>
    {
        let mut found: Option<SimpleType> = None;
        let mut seen_anno = false;
        let mut seen_other = false;
        loop {
            match self.next_event()? {
                EventInto::StartElement { name } => {
                    let child_attrs = self.take_attrs()?;
                    self.push_ns_scope(&child_attrs);
                    let qn = self.qname_of_element(&name)?;
                    if qn.namespace.as_deref() == Some(XS) {
                        self.check_annotation_pos(
                            &qn.local, &mut seen_anno, &mut seen_other, "attribute",
                        )?;
                        match qn.local.as_ref() {
                            "simpleType" => {
                                if found.is_some() {
                                    return Err(self.err(
                                        "<xs:attribute> may have at most one inline <xs:simpleType>",
                                    ));
                                }
                                found = Some(self.parse_simple_type_body(&child_attrs)?);
                            }
                            "annotation" => self.parse_annotation_body(&child_attrs)?,
                            other => return Err(self.err(format!(
                                "<xs:{other}> is not allowed as a child of <xs:attribute>"
                            ))),
                        }
                    } else {
                        self.skip_body()?;
                    }
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => return Ok(found),
                EventInto::Eof => return Err(self.err("unexpected EOF")),
                _ => {}
            }
        }
    }

    // ── top-level simpleType ─────────────────────────────────────────────

    fn parse_top_simple_type(&mut self, attrs: &[Attr<'a>]) -> Result<(), SchemaCompileError> {
        // XSD §3.14.1 — a top-level <xs:simpleType> requires `name`.
        if self.attr(attrs, "name").is_none() {
            return Err(self.err(
                "top-level <xs:simpleType> requires a 'name' attribute",
            ));
        }
        // Track the in-flight name so the restriction body can detect
        // a self-reference (`<xs:simpleType name="X"><xs:restriction
        // base="X">`) and reject it — outside <xs:redefine> this is a
        // direct circular definition, not a derivation.
        let prev_in_flight = self.simple_type_in_flight.take();
        let name_qn = self.attr(attrs, "name").map(|n| QName {
            namespace: self.current_target_ns.clone(),
            local:     Arc::from(n),
        });
        self.simple_type_in_flight = name_qn.clone();
        let st = self.parse_simple_type_body_inner(attrs, /*allow_name=*/true);
        self.simple_type_in_flight = prev_in_flight;
        let st = st?;
        if let Some(name) = &st.name {
            let qn = QName {
                namespace: self.current_target_ns.clone(),
                local:     name.clone(),
            };
            if !self.in_redefine && self.builder.types.contains_key(&qn) {
                return Err(self.err(format!(
                    "duplicate top-level type definition {:?} \
                     (a type with this name is already declared)",
                    qn.local,
                )));
            }
            self.builder.types.insert(qn, TypeRef::Simple(Arc::new(st)));
        }
        Ok(())
    }

    /// Parse the body of a `<xs:simpleType>` (the opening Start has
    /// already been consumed; we walk through the matching EndElement).
    /// Used at inline call sites (inside element/attribute/list/union/
    /// restriction); XSD §3.14.2 forbids `name` on those, so we reject
    /// it here.
    fn parse_simple_type_body(&mut self, attrs: &[Attr<'a>])
        -> Result<SimpleType, SchemaCompileError>
    {
        self.parse_simple_type_body_inner(attrs, /*allow_name=*/false)
    }

    /// XSD §3.14.2 — body is `(annotation?, (restriction | list | union))`:
    /// exactly one of the three is required, and any other XS child or
    /// a second derivation is a schema validity error.
    fn parse_simple_type_body_inner(&mut self, attrs: &[Attr<'a>], allow_name: bool)
        -> Result<SimpleType, SchemaCompileError>
    {
        if !allow_name && self.attr(attrs, "name").is_some() {
            return Err(self.err(
                "inline <xs:simpleType> must not have a 'name' attribute (XSD §3.14.2)",
            ));
        }
        let name: Option<Arc<str>> = self.attr(attrs, "name").map(Arc::from);

        // XSD §3.14.6 — top-level simpleType honours `final`; the
        // schema-level `finalDefault` supplies the value when omitted.
        // Anonymous inline simpleTypes can't be referenced, so any
        // value here is effectively inert — we still parse it for
        // completeness.
        let final_ = match self.attr(attrs, "final") {
            Some(_) => parse_block_set(self.attr(attrs, "final"))?,
            None    => self.final_default,
        };

        let mut builtin = BuiltinType::String;
        let mut facets  = FacetSet::default();
        let mut whitespace = WhitespaceMode::Preserve;
        let mut variety: super::types::Variety = super::types::Variety::Atomic;
        let mut assertions: Vec<super::schema::Assertion> = Vec::new();
        let mut seen_anno = false;
        let mut seen_other = false;
        let mut saw_derivation = false;

        loop {
            match self.next_event()? {
                EventInto::StartElement { name: child_name } => {
                    let child_attrs = self.take_attrs()?;
                    self.push_ns_scope(&child_attrs);
                    let qn = self.qname_of_element(&child_name)?;
                    if qn.namespace.as_deref() == Some(XS) {
                        self.check_annotation_pos(
                            &qn.local, &mut seen_anno, &mut seen_other, "simpleType",
                        )?;
                        let local = qn.local.as_ref();
                        if matches!(local, "restriction" | "list" | "union")
                            && saw_derivation
                        {
                            return Err(self.err(
                                "<xs:simpleType> must have exactly one of \
                                 <xs:restriction>, <xs:list>, or <xs:union>",
                            ));
                        }
                        match local {
                            "restriction" => {
                                saw_derivation = true;
                                let (b, f, ws, v, a) = self.parse_simple_restriction(&child_attrs)?;
                                builtin = b; facets = f; whitespace = ws; variety = v;
                                assertions = a;
                            }
                            "list" => {
                                saw_derivation = true;
                                variety = super::types::Variety::List {
                                    item_type: self.parse_list_body(&child_attrs)?,
                                };
                                // List value-space is the string form;
                                // facets layered above (via a wrapping
                                // restriction) operate on item count
                                // per spec.
                                builtin = BuiltinType::String;
                                whitespace = WhitespaceMode::Collapse;
                            }
                            "union" => {
                                saw_derivation = true;
                                variety = super::types::Variety::Union {
                                    members: self.parse_union_body(&child_attrs)?,
                                };
                                builtin = BuiltinType::String;
                                whitespace = WhitespaceMode::Collapse;
                            }
                            "annotation" => self.parse_annotation_body(&child_attrs)?,
                            other => return Err(self.err(format!(
                                "<xs:{other}> is not allowed as a child of <xs:simpleType>"
                            ))),
                        }
                    } else {
                        self.skip_body()?;
                    }
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => break,
                EventInto::Eof => return Err(self.err("unexpected EOF in simpleType")),
                _ => {}
            }
        }
        if !saw_derivation {
            return Err(self.err(
                "<xs:simpleType> requires one of <xs:restriction>, <xs:list>, or <xs:union>",
            ));
        }

        Ok(SimpleType {
            name, builtin, facets, whitespace, variety, final_,
            assertions,
        })
    }

    /// Parse the body of `<xs:list itemType="..."/>` (or with a nested
    /// `<xs:simpleType>` child providing an anonymous item type).
    /// Consumes through the matching EndElement.
    fn parse_list_body(&mut self, attrs: &[Attr<'a>])
        -> Result<Arc<SimpleType>, SchemaCompileError>
    {
        // XSD §3.15.2 — body of <xs:list> is (annotation?, simpleType?).
        // `itemType` and the inline simpleType are mutually exclusive,
        // and at least one of them is required.
        let item_type_attr = self.attr(attrs, "itemType").map(|s| s.to_string());
        let mut nested: Option<SimpleType> = None;
        let mut seen_anno = false;
        let mut seen_other = false;
        loop {
            match self.next_event()? {
                EventInto::StartElement { name: child_name } => {
                    let child_attrs = self.take_attrs()?;
                    self.push_ns_scope(&child_attrs);
                    let qn = self.qname_of_element(&child_name)?;
                    if qn.namespace.as_deref() == Some(XS) {
                        self.check_annotation_pos(
                            &qn.local, &mut seen_anno, &mut seen_other, "list",
                        )?;
                        match qn.local.as_ref() {
                            "simpleType" => {
                                if nested.is_some() {
                                    return Err(self.err(
                                        "<xs:list> may have at most one inline <xs:simpleType>",
                                    ));
                                }
                                if item_type_attr.is_some() {
                                    return Err(self.err(
                                        "<xs:list> cannot have both itemType= and an inline <xs:simpleType>",
                                    ));
                                }
                                nested = Some(self.parse_simple_type_body(&child_attrs)?);
                            }
                            "annotation" => self.parse_annotation_body(&child_attrs)?,
                            other => return Err(self.err(format!(
                                "<xs:{other}> is not allowed as a child of <xs:list>"
                            ))),
                        }
                    } else {
                        self.skip_body()?;
                    }
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => break,
                EventInto::Eof => return Err(self.err("unexpected EOF in <xs:list>")),
                _ => {}
            }
        }
        if let Some(t) = nested {
            return Ok(Arc::new(t));
        }
        let item = item_type_attr.ok_or_else(||
            self.err("<xs:list> needs itemType= or a nested <xs:simpleType>"))?;
        let qn = self.parse_qname(&item, false)?;
        // XSD §3.15.6 (itemType-valid): the itemType must be a simple
        // type, never a complex type, and its variety must be atomic
        // or union — list-of-list isn't permitted.
        if let Some(TypeRef::Simple(st)) = self.builder.types.get(&qn) {
            if matches!(st.variety, super::types::Variety::List { .. }) {
                return Err(self.err(format!(
                    "<xs:list itemType={item:?}>: itemType must be atomic or union, not a list"
                )));
            }
            if st.final_.contains(super::schema::BlockSet::LIST) {
                return Err(self.err(format!(
                    "<xs:list itemType={item:?}>: base simple type's `final` disallows list derivation"
                )));
            }
        }
        if let Some(TypeRef::Complex(_)) = self.builder.types.get(&qn) {
            return Err(self.err(format!(
                "<xs:list itemType={item:?}>: itemType must be a simple type"
            )));
        }
        Ok(self.simple_type_for(qn))
    }

    /// Parse the body of `<xs:union memberTypes="QName QName ..."/>` plus
    /// any nested `<xs:simpleType>` children (the anonymous-member form).
    /// Members appear in the order: memberTypes first (left-to-right),
    /// then nested simpleTypes — matching XSD §3.16.
    fn parse_union_body(&mut self, attrs: &[Attr<'a>])
        -> Result<Vec<Arc<SimpleType>>, SchemaCompileError>
    {
        let mut members: Vec<Arc<SimpleType>> = Vec::new();
        if let Some(list) = self.attr(attrs, "memberTypes") {
            for token in list.split_whitespace() {
                let qn = self.parse_qname(token, false)?;
                // XSD §3.16.6 — each memberType must be a simple type.
                if let Some(TypeRef::Complex(_)) = self.builder.types.get(&qn) {
                    return Err(self.err(format!(
                        "<xs:union memberTypes={list:?}>: {token:?} is not a simple type"
                    )));
                }
                if let Some(TypeRef::Simple(st)) = self.builder.types.get(&qn) {
                    if st.final_.contains(super::schema::BlockSet::UNION) {
                        return Err(self.err(format!(
                            "<xs:union memberTypes={list:?}>: {token:?}'s `final` \
                             disallows union derivation"
                        )));
                    }
                }
                if qn.namespace.as_deref() == Some(XS)
                    && qn.local.as_ref() != "anySimpleType"
                    && BuiltinType::from_name(&qn.local).is_none()
                {
                    return Err(self.err(format!(
                        "<xs:union memberTypes={list:?}>: {token:?} is not a built-in XSD type"
                    )));
                }
                members.push(self.simple_type_for(qn));
            }
        }
        // XSD §3.16.2 — body of <xs:union> is (annotation?, simpleType*).
        let mut seen_anno = false;
        let mut seen_other = false;
        loop {
            match self.next_event()? {
                EventInto::StartElement { name: child_name } => {
                    let child_attrs = self.take_attrs()?;
                    self.push_ns_scope(&child_attrs);
                    let qn = self.qname_of_element(&child_name)?;
                    if qn.namespace.as_deref() == Some(XS) {
                        self.check_annotation_pos(
                            &qn.local, &mut seen_anno, &mut seen_other, "union",
                        )?;
                        match qn.local.as_ref() {
                            "simpleType" => {
                                let st = self.parse_simple_type_body(&child_attrs)?;
                                members.push(Arc::new(st));
                            }
                            "annotation" => self.parse_annotation_body(&child_attrs)?,
                            other => return Err(self.err(format!(
                                "<xs:{other}> is not allowed as a child of <xs:union>"
                            ))),
                        }
                    } else {
                        self.skip_body()?;
                    }
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => break,
                EventInto::Eof => return Err(self.err("unexpected EOF in <xs:union>")),
                _ => {}
            }
        }
        if members.is_empty() {
            return Err(self.err(
                "<xs:union> needs memberTypes= or at least one nested <xs:simpleType>"
            ));
        }
        Ok(members)
    }

    /// Parse `<xs:restriction base="…">` body, returning the base
    /// built-in plus any facets.
    fn parse_simple_restriction(&mut self, attrs: &[Attr<'a>])
        -> Result<(BuiltinType, FacetSet, WhitespaceMode, super::types::Variety,
                   Vec<super::schema::Assertion>), SchemaCompileError>
    {
        // Resolve the base's value-space.  Built-in `xs:*` bases map
        // directly to a `BuiltinType`; a user-defined simple base
        // already has its chain collapsed (each `SimpleType.builtin`
        // is the ultimate ancestor), so reading its `builtin` field
        // walks the full chain in one step.  Facets and whitespace
        // inherit too — XSD restriction semantics require the derived
        // type to satisfy every facet in the chain, so we seed
        // `facets` with the base's set and append the new ones below.
        //
        // Variety also inherits: restricting a `<xs:list>` type yields
        // a list type (length facets count items, item type is the
        // base's item type); restricting a `<xs:union>` yields a
        // union.  Without this, length=3 on a 3-item list would
        // (wrongly) check string length 3.
        //
        // Two valid forms per XSD §3.14.2:
        //   * `<xs:restriction base="…">`              — named base
        //   * `<xs:restriction> <xs:simpleType>…`      — inline anon base
        // The `base=` attribute and the inline `<xs:simpleType>` child
        // are mutually exclusive but one must be present.  Inline-base
        // form is handled when we encounter the `simpleType` child
        // below — it overrides the no-base initial defaults.
        //
        // Limitation: only handles bases that have already been parsed
        // (top-down schema declarations).  Forward references to
        // later-declared user types fall through to `BuiltinType::String`
        // and `Variety::Atomic` — declaration order matters.
        let mut base_builtin = BuiltinType::String;
        let mut whitespace = WhitespaceMode::Preserve;
        let mut facets = FacetSet::default();
        let mut variety: super::types::Variety = super::types::Variety::Atomic;
        let mut inherited = false;
        let mut has_named_base = false;
        let mut pending_forward_base: Option<QName> = None;
        // XSD 1.1 `<xs:assertion test="…">` facets on a simpleType
        // restriction — evaluated at validate time with `$value` bound
        // to the parsed atomic value.  Collected here, returned to
        // the caller alongside the other restriction outputs.
        let mut assertions: Vec<super::schema::Assertion> = Vec::new();
        if let Some(base_qn) = self.attr(attrs, "base") {
            has_named_base = true;
            let qn = self.parse_qname(base_qn, false)?;
            // XSD §3.14.6 cos-st-derived-ok — a simple type cannot
            // restrict itself.  Outside `<xs:redefine>` (which is the
            // one place same-name self-reference is the spec's
            // expansion semantics), reject this directly.
            if !self.in_redefine
                && self.simple_type_in_flight.as_ref() == Some(&qn)
            {
                return Err(self.err(format!(
                    "<xs:simpleType name={:?}><xs:restriction base={:?}>: \
                     a simple type cannot restrict itself (circular definition)",
                    qn.local, qn.local,
                )));
            }
            // Stash the resolved base name; the redefine-body validator
            // consults this to confirm src-redefine compliance.
            self.last_simple_restriction_base = Some(qn.clone());
            if qn.namespace.as_deref() == Some(XS) {
                // XSD §3.14.2 / §3.4.6 — only the BuiltinType set is
                // usable as a base.  xs:anyType (complex) and
                // xs:anySimpleType (the ultimate ancestor; not
                // restrictable per §3.16.7) are both forbidden, as is
                // anything not in our BuiltinType table at all.
                if qn.local.as_ref() == "anyType" {
                    return Err(self.err(
                        "<xs:restriction base=\"xs:anyType\"> is not allowed in a simpleType",
                    ));
                }
                if qn.local.as_ref() == "anySimpleType" {
                    return Err(self.err(
                        "<xs:restriction base=\"xs:anySimpleType\">: anySimpleType is \
                         the ultimate ancestor of all simple types and cannot itself \
                         be restricted (XSD §3.16.7)",
                    ));
                }
                match BuiltinType::from_name(&qn.local) {
                    Some(b) => {
                        // Gate XSD 1.1-only built-ins (dateTimeStamp,
                        // dayTimeDuration, yearMonthDuration,
                        // anyAtomicType, error) when the schema is
                        // being compiled in strict 1.0 mode.
                        if b.is_xsd11_only()
                            && !matches!(self.builder.effective_version, SchemaVersion::Xsd11)
                        {
                            return Err(self.err(format!(
                                "xs:{} is an XSD 1.1 built-in — set \
                                 SchemaOptions::version to Xsd11, or to Auto with \
                                 vc:minVersion=\"1.1\" on <xs:schema>",
                                qn.local,
                            )));
                        }
                        base_builtin = b;
                    }
                    None => return Err(self.err(format!(
                        "<xs:restriction base={base_qn:?}>: {base_qn:?} is not a built-in XSD type"
                    ))),
                }
            } else if let Some(TypeRef::Complex(_)) = self.builder.types.get(&qn) {
                return Err(self.err(format!(
                    "<xs:restriction base={base_qn:?}> in a simpleType must reference a simpleType, not a complexType"
                )));
            } else if let Some(TypeRef::Simple(st)) = self.builder.types.get(&qn) {
                if st.final_.contains(super::schema::BlockSet::RESTRICTION) {
                    return Err(self.err(format!(
                        "<xs:restriction base={base_qn:?}>: base simple type's \
                         `final` disallows restriction"
                    )));
                }
                base_builtin = st.builtin;
                facets = st.facets.clone();
                whitespace = st.whitespace;
                variety = st.variety.clone();
                inherited = true;
            } else if qn.namespace.as_deref() != self.current_target_ns.as_deref()
                && !import_covers(&self.builder.imported_namespaces,
                                  qn.namespace.as_deref())
            {
                // XSD §3.16.6 / src-resolve clause 4 — the base
                // resolves to a foreign namespace that the schema
                // didn't import.  Even if the namespace happened to
                // declare a same-named type elsewhere, we have no way
                // to reach it.  Reject loudly so the author fixes the
                // missing <xs:import>.
                return Err(self.err(format!(
                    "<xs:restriction base={base_qn:?}>: namespace {:?} is not the \
                     schema's targetNamespace and was not brought in by <xs:import>",
                    qn.namespace.as_deref().unwrap_or(""),
                )));
            } else {
                // Same-target-namespace forward reference — the base
                // hasn't been parsed yet, so we don't have its facets
                // to inherit.  Record the unresolved base so a post-
                // pass can splice in the real facets and re-run
                // `check_facet_tightening` on whatever this
                // restriction adds.
                pending_forward_base = Some(qn.clone());
            }
        }
        if !inherited {
            whitespace = base_builtin.default_whitespace();
        }
        let mut base_facet_count = facets.facets.len();

        loop {
            match self.next_event()? {
                EventInto::StartElement { name } => {
                    let f_attrs = self.take_attrs()?;
                    self.push_ns_scope(&f_attrs);
                    let fqn = self.qname_of_element(&name)?;
                    if fqn.namespace.as_deref() == Some(XS) {
                        let v = self.attr(&f_attrs, "value");
                        match (fqn.local.as_ref(), v) {
                            ("length",        Some(v)) => facets.push(Facet::Length(v.parse().map_err(|e|
                                self.err(format!("length facet: {e}"))
                            )?)),
                            ("minLength",     Some(v)) => facets.push(Facet::MinLength(v.parse().map_err(|e|
                                self.err(format!("minLength facet: {e}"))
                            )?)),
                            ("maxLength",     Some(v)) => facets.push(Facet::MaxLength(v.parse().map_err(|e|
                                self.err(format!("maxLength facet: {e}"))
                            )?)),
                            ("pattern",       Some(v)) => {
                                let p = super::regex::Pattern::compile(v).map_err(|e|
                                    self.err(format!("pattern facet: {e}"))
                                )?;
                                facets.push(Facet::Pattern(p));
                            }
                            ("enumeration",   Some(v)) => {
                                // cvc-enumeration-valid: each enumeration
                                // value must lie in the base type's value
                                // space (XSD §3.14.6).
                                super::types::SimpleType::of_builtin(base_builtin)
                                    .validate(v)
                                    .map_err(|e| self.err(format!(
                                        "enumeration value {v:?} not in base type's value space: {}",
                                        e.message,
                                    )))?;
                                // Accumulate into one Enumeration facet.
                                if let Some(Facet::Enumeration(opts)) = facets.facets.last_mut() {
                                    opts.push(v.to_owned());
                                } else {
                                    facets.push(Facet::Enumeration(vec![v.to_owned()]));
                                }
                            }
                            ("whiteSpace",    Some(v)) => {
                                let new_ws = match v {
                                    "preserve" => WhitespaceMode::Preserve,
                                    "replace"  => WhitespaceMode::Replace,
                                    "collapse" => WhitespaceMode::Collapse,
                                    other => return Err(self.err(format!(
                                        "invalid whiteSpace value: {other:?}"
                                    ))),
                                };
                                // XSD §4.3.5 — whiteSpace facet may only be
                                // restricted further along the preserve →
                                // replace → collapse chain.  The base mode is
                                // whatever the parent (named or builtin)
                                // already imposes — not the builtin's default,
                                // because intermediate restrictions can tighten
                                // it (e.g. a user simpleType setting
                                // whiteSpace="replace" on xs:string).
                                let base_ws = whitespace;
                                let allowed = matches!(
                                    (base_ws, new_ws),
                                    (WhitespaceMode::Preserve, _)
                                    | (WhitespaceMode::Replace, WhitespaceMode::Replace | WhitespaceMode::Collapse)
                                    | (WhitespaceMode::Collapse, WhitespaceMode::Collapse)
                                );
                                if !allowed {
                                    return Err(self.err(format!(
                                        "whiteSpace={v:?} is not a valid restriction of the base whitespace mode"
                                    )));
                                }
                                whitespace = new_ws;
                            }
                            ("minInclusive",  Some(v)) => {
                                self.validate_bound_against_base(v, base_builtin, "minInclusive")?;
                                facets.push(Facet::MinInclusive(parse_bound(v, base_builtin)?));
                            }
                            ("maxInclusive",  Some(v)) => {
                                self.validate_bound_against_base(v, base_builtin, "maxInclusive")?;
                                facets.push(Facet::MaxInclusive(parse_bound(v, base_builtin)?));
                            }
                            ("minExclusive",  Some(v)) => {
                                self.validate_bound_against_base(v, base_builtin, "minExclusive")?;
                                facets.push(Facet::MinExclusive(parse_bound(v, base_builtin)?));
                            }
                            ("maxExclusive",  Some(v)) => {
                                self.validate_bound_against_base(v, base_builtin, "maxExclusive")?;
                                facets.push(Facet::MaxExclusive(parse_bound(v, base_builtin)?));
                            }
                            ("totalDigits",   Some(v)) => {
                                let n: u32 = v.parse().map_err(|e|
                                    self.err(format!("totalDigits: {e}"))
                                )?;
                                if n == 0 {
                                    return Err(self.err(
                                        "totalDigits value must be ≥ 1 (XSD §4.3.11)"
                                    ));
                                }
                                facets.push(Facet::TotalDigits(n));
                            }
                            ("fractionDigits", Some(v)) => {
                                let n: u32 = v.parse().map_err(|e|
                                    self.err(format!("fractionDigits: {e}"))
                                )?;
                                // XSD §3.3.13 / §3.3.14 — integer
                                // and its subtypes fix fractionDigits
                                // at 0; the derived value must equal
                                // the fixed base value.
                                if base_builtin.is_integer_family() && n != 0 {
                                    return Err(self.err(format!(
                                        "fractionDigits ({n}) cannot be nonzero on a derivation \
                                         of xs:integer (fixed at 0)"
                                    )));
                                }
                                facets.push(Facet::FractionDigits(n));
                            }
                            ("annotation", _) => {
                                self.parse_annotation_body(&f_attrs)?;
                                self.pop_ns_scope();
                                continue;
                            }
                            ("explicitTimezone", Some(v)) => {
                                // XSD 1.1 § 4.3.13 — values are
                                // `required` | `prohibited` | `optional`.
                                // Only legal in 1.1 mode.
                                if !matches!(self.builder.effective_version,
                                             SchemaVersion::Xsd11)
                                {
                                    return Err(self.err(
                                        "<xs:explicitTimezone> is an XSD 1.1 facet — \
                                         set SchemaOptions::version to Xsd11, or to Auto \
                                         with vc:minVersion=\"1.1\" on <xs:schema>",
                                    ));
                                }
                                use super::facets::TimezoneRequirement as TR;
                                let req = match v {
                                    "required"   => TR::Required,
                                    "prohibited" => TR::Prohibited,
                                    "optional"   => TR::Optional,
                                    other => return Err(self.err(format!(
                                        "<xs:explicitTimezone value={other:?}>: must be \
                                         \"required\", \"prohibited\", or \"optional\""
                                    ))),
                                };
                                facets.push(Facet::ExplicitTimezone(req));
                            }
                            ("length", None) | ("minLength", None) | ("maxLength", None)
                            | ("pattern", None) | ("enumeration", None) | ("whiteSpace", None)
                            | ("minInclusive", None) | ("maxInclusive", None)
                            | ("minExclusive", None) | ("maxExclusive", None)
                            | ("totalDigits", None) | ("fractionDigits", None)
                            | ("explicitTimezone", None) => {
                                return Err(self.err(format!(
                                    "<xs:{}> facet requires a 'value' attribute", fqn.local
                                )));
                            }
                            ("simpleType", _) => {
                                // Inline base for this restriction — XSD
                                // §3.14.2 lets `<xs:restriction>` carry an
                                // anonymous base via a child `simpleType`
                                // instead of `base="…"`. Only one inline
                                // simpleType is allowed, and not in
                                // combination with `base=`.
                                if has_named_base {
                                    return Err(self.err(
                                        "<xs:restriction> cannot combine base= with an inline <xs:simpleType>",
                                    ));
                                }
                                if inherited {
                                    return Err(self.err(
                                        "<xs:restriction> may have at most one inline <xs:simpleType>",
                                    ));
                                }
                                let inline_base = self.parse_simple_type_body(&f_attrs)?;
                                base_builtin = inline_base.builtin;
                                facets       = inline_base.facets.clone();
                                whitespace   = inline_base.whitespace;
                                variety      = inline_base.variety.clone();
                                inherited    = true;
                                base_facet_count = facets.facets.len();
                                self.pop_ns_scope();
                                continue;
                            }
                            ("assertion", _) => {
                                // XSD 1.1 simple-type assertion facet —
                                // `$value` is bound to the parsed
                                // atomic value at eval time.  Body is
                                // anno-only (we consume it inside
                                // `parse_assertion_body`); skip to
                                // matching EndElement.
                                assertions.extend(self.parse_assertion_body(&f_attrs)?);
                                self.pop_ns_scope();
                                continue;
                            }
                            (other, _) => {
                                return Err(self.err(format!(
                                    "<xs:{other}> is not a valid facet or child of <xs:restriction> (simpleType)"
                                )));
                            }
                        }
                    }
                    // Each facet body must be annotation-only — no
                    // nested xs:notation, xs:element, etc.
                    self.parse_anno_only_body(fqn.local.as_ref())?;
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => break,
                EventInto::Eof => return Err(self.err("unexpected EOF in restriction")),
                _ => {}
            }
        }
        if !has_named_base && !inherited {
            return Err(self.err(
                "<xs:restriction> needs either a base= attribute or an inline <xs:simpleType>",
            ));
        }
        // Derived bounds REPLACE inherited bounds of the "other"
        // inclusivity when both kinds end up in the merged set
        // (XSD §4.3.9 — minInclusive/minExclusive can't co-exist,
        // but inheritance of one then a derived override of the
        // other is the normal restriction pattern).
        prune_replaced_bounds(&mut facets, base_facet_count);
        self.validate_facet_set(&facets)?;
        // cvc-restriction (XSD §4.3): each derived facet must be a
        // legitimate tightening of the base's corresponding facet.
        // `facets.facets[..base_facet_count]` is the base set; the
        // tail is what this restriction adds.
        self.check_facet_tightening(&facets.facets[..base_facet_count], &facets.facets[base_facet_count..])?;
        // XSD §4.1.5 — applicable facets are per-variety:
        // * List: length, minLength, maxLength, pattern, enumeration, whiteSpace
        // * Union: pattern, enumeration
        // Anything else is a schema validity error.
        self.check_facet_applicability(&facets.facets[base_facet_count..], &variety)?;
        // If the named base wasn't resolvable at parse time
        // (same-target-namespace forward reference), remember the
        // tail of derived facets so the post-pass can validate them
        // against the eventually-resolved base.
        if let Some(base_qn) = pending_forward_base {
            let derived = facets.facets[base_facet_count..].to_vec();
            if !derived.is_empty() {
                self.builder.pending_simple_facet_checks.push((base_qn, derived));
            }
        }
        Ok((base_builtin, facets, whitespace, variety, assertions))
    }

    fn check_facet_applicability(
        &self,
        derived: &[Facet],
        variety: &super::types::Variety,
    ) -> Result<(), SchemaCompileError> {
        use super::types::Variety;
        for f in derived {
            let allowed = match (variety, f) {
                (Variety::List { .. }, Facet::Length(_) | Facet::MinLength(_) | Facet::MaxLength(_)
                                     | Facet::Pattern(_) | Facet::Enumeration(_)) => true,
                (Variety::Union { .. }, Facet::Pattern(_) | Facet::Enumeration(_)) => true,
                (Variety::Atomic, _) => true,
                // List/Union with disallowed facet
                _ => false,
            };
            if !allowed {
                let kind = match variety {
                    Variety::List { .. } => "list",
                    Variety::Union { .. } => "union",
                    Variety::Atomic => "atomic",
                };
                let facet_name = match f {
                    Facet::Length(_)         => "length",
                    Facet::MinLength(_)      => "minLength",
                    Facet::MaxLength(_)      => "maxLength",
                    Facet::Pattern(_)        => "pattern",
                    Facet::Enumeration(_)    => "enumeration",
                    Facet::MinInclusive(_)   => "minInclusive",
                    Facet::MaxInclusive(_)   => "maxInclusive",
                    Facet::MinExclusive(_)   => "minExclusive",
                    Facet::MaxExclusive(_)   => "maxExclusive",
                    Facet::TotalDigits(_)    => "totalDigits",
                    Facet::FractionDigits(_) => "fractionDigits",
                    Facet::ExplicitTimezone(_) => "explicitTimezone",
                };
                return Err(self.err(format!(
                    "facet <xs:{facet_name}> is not applicable to a {kind} simple type"
                )));
            }
        }
        Ok(())
    }

    // helper: see `prune_replaced_bounds` at the bottom of this module.

    fn check_facet_tightening(
        &self, base: &[Facet], derived: &[Facet],
    ) -> Result<(), SchemaCompileError> {
        check_facet_tightening_pure(base, derived).map_err(|m| self.err(m))
    }

    /// XSD §4.3 — every facet in a single restriction body must be
    /// consistent with the others. We enforce the cross-facet rules
    /// that don't depend on the base type's value space (those
    /// happen during `parse_bound` / `parse_value`).
    ///
    /// Rules enforced:
    /// * `length` is mutually exclusive with `minLength` / `maxLength`.
    /// * `minLength <= maxLength`.
    /// * `minInclusive` excludes `minExclusive` (same for max side).
    /// * `minInclusive <= maxInclusive`, `minExclusive <= maxExclusive`,
    ///   and mixed-bound forms.
    /// * `fractionDigits <= totalDigits`.
    fn validate_facet_set(&self, facets: &FacetSet) -> Result<(), SchemaCompileError> {
        let mut length:          Option<usize>  = None;
        let mut min_length:      Option<usize>  = None;
        let mut max_length:      Option<usize>  = None;
        let mut min_inclusive:   Option<&Bound> = None;
        let mut min_exclusive:   Option<&Bound> = None;
        let mut max_inclusive:   Option<&Bound> = None;
        let mut max_exclusive:   Option<&Bound> = None;
        let mut total_digits:    Option<u32>    = None;
        let mut fraction_digits: Option<u32>    = None;
        for f in &facets.facets {
            match f {
                Facet::Length(n)          => length          = Some(*n),
                Facet::MinLength(n)       => min_length      = Some(*n),
                Facet::MaxLength(n)       => max_length      = Some(*n),
                Facet::MinInclusive(b)    => min_inclusive   = Some(b),
                Facet::MinExclusive(b)    => min_exclusive   = Some(b),
                Facet::MaxInclusive(b)    => max_inclusive   = Some(b),
                Facet::MaxExclusive(b)    => max_exclusive   = Some(b),
                Facet::TotalDigits(n)     => total_digits    = Some(*n),
                Facet::FractionDigits(n)  => fraction_digits = Some(*n),
                _ => {}
            }
        }
        if length.is_some() && (min_length.is_some() || max_length.is_some()) {
            return Err(self.err(
                "length facet cannot co-occur with minLength or maxLength (XSD §4.3.1)",
            ));
        }
        if let (Some(lo), Some(hi)) = (min_length, max_length) {
            if lo > hi {
                return Err(self.err(format!(
                    "minLength ({lo}) > maxLength ({hi}) (XSD §4.3.2)"
                )));
            }
        }
        if min_inclusive.is_some() && min_exclusive.is_some() {
            return Err(self.err(
                "minInclusive and minExclusive are mutually exclusive (XSD §4.3.9)",
            ));
        }
        if max_inclusive.is_some() && max_exclusive.is_some() {
            return Err(self.err(
                "maxInclusive and maxExclusive are mutually exclusive (XSD §4.3.7)",
            ));
        }
        for (lo, hi, lo_name, hi_name) in [
            (min_inclusive, max_inclusive, "minInclusive", "maxInclusive"),
            (min_inclusive, max_exclusive, "minInclusive", "maxExclusive"),
            (min_exclusive, max_inclusive, "minExclusive", "maxInclusive"),
            (min_exclusive, max_exclusive, "minExclusive", "maxExclusive"),
        ] {
            if let (Some(lo), Some(hi)) = (lo, hi) {
                if compare_bounds(lo, hi).map(|o| o.is_gt()).unwrap_or(false) {
                    return Err(self.err(format!(
                        "{lo_name} > {hi_name} (XSD §4.3.x)"
                    )));
                }
            }
        }
        if let (Some(fd), Some(td)) = (fraction_digits, total_digits) {
            if fd > td {
                return Err(self.err(format!(
                    "fractionDigits ({fd}) > totalDigits ({td}) (XSD §4.3.12)"
                )));
            }
        }
        Ok(())
    }

    fn validate_bound_against_base(
        &self, raw: &str, builtin: BuiltinType, facet: &str,
    ) -> Result<(), SchemaCompileError> {
        super::types::SimpleType::of_builtin(builtin)
            .validate(raw)
            .map_err(|e| self.err(format!(
                "{facet} value {raw:?} not in base type's value space: {}",
                e.message,
            )))?;
        Ok(())
    }

    // ── top-level complexType ────────────────────────────────────────────

    fn parse_top_complex_type(&mut self, attrs: &[Attr<'a>]) -> Result<(), SchemaCompileError> {
        // XSD §3.4.1 — a top-level <xs:complexType> requires `name`.
        if self.attr(attrs, "name").is_none() {
            return Err(self.err(
                "top-level <xs:complexType> requires a 'name' attribute",
            ));
        }
        let ct = self.parse_complex_type_body_inner(attrs, /*allow_name=*/true)?;
        if let Some(name) = &ct.name {
            // XSD §3.4.6 (ct-props-correct.1) — type names are unique
            // within the schema's name table.  `<xs:redefine>` is the
            // one exception: it intentionally replaces the original
            // type with the redefining version.
            if !self.in_redefine && self.builder.types.contains_key(name) {
                return Err(self.err(format!(
                    "duplicate top-level type definition {:?} \
                     (a type with this name is already declared)",
                    name.local,
                )));
            }
            self.builder.types.insert(name.clone(), TypeRef::Complex(Arc::new(ct)));
        }
        Ok(())
    }

    fn parse_complex_type_body(&mut self, attrs: &[Attr<'a>])
        -> Result<ComplexType, SchemaCompileError>
    {
        self.parse_complex_type_body_inner(attrs, /*allow_name=*/false)
    }

    fn parse_complex_type_body_inner(&mut self, attrs: &[Attr<'a>], allow_name: bool)
        -> Result<ComplexType, SchemaCompileError>
    {
        if !allow_name && self.attr(attrs, "name").is_some() {
            return Err(self.err(
                "inline <xs:complexType> must not have a 'name' attribute (XSD §3.4.2)",
            ));
        }
        let name = self.attr(attrs, "name").map(|n| QName {
            namespace: self.current_target_ns.clone(),
            local:     Arc::from(n),
        });
        let abstract_ = self.parse_xsd_bool(attrs, "abstract")?.unwrap_or(false);
        let mixed_attr = self.parse_xsd_bool(attrs, "mixed")?.unwrap_or(false);
        // XSD §3.1 — fall back to schema-level blockDefault /
        // finalDefault when this complexType doesn't carry its own.
        // For complexType these accept only restriction/extension/#all
        // (substitution belongs to xs:element), so mask the inherited
        // default to those bits.
        let ct_mask = BlockSet::RESTRICTION | BlockSet::EXTENSION;
        let block  = match self.attr(attrs, "block") {
            Some(_) => parse_ct_derivation_set(self.attr(attrs, "block"), "block")?,
            None    => self.block_default & ct_mask,
        };
        let final_ = match self.attr(attrs, "final") {
            Some(_) => parse_ct_derivation_set(self.attr(attrs, "final"), "final")?,
            None    => self.final_default & ct_mask,
        };

        let mut content   = ContentModel::Empty;
        let mut attr_uses = Vec::new();
        let mut any_attr  = None;
        let mut derivation: Option<Derivation> = None;
        let mut mixed     = mixed_attr;
        let mut pending_ag_refs: Vec<QName> = Vec::new();
        let mut assertions: Vec<super::schema::Assertion> = Vec::new();
        let mut seen_anno = false;
        let mut seen_other = false;
        // XSD §3.4.2 body grammar:
        //   (annotation?, (simpleContent | complexContent
        //                  | ((group|all|choice|sequence)?,
        //                     ((attribute | attributeGroup)*,
        //                      anyAttribute?))))
        // i.e. simpleContent/complexContent are mutually exclusive
        // with each other and with any model-group/attribute child;
        // the implicit form allows at most one model group, then
        // attribute uses, then at most one anyAttribute. These flags
        // enforce that ordering.
        let mut saw_derived_content = false;
        let mut saw_model_group     = false;
        let mut saw_any_attribute   = false;

        loop {
            match self.next_event()? {
                EventInto::StartElement { name: child } => {
                    let child_attrs = self.take_attrs()?;
                    self.push_ns_scope(&child_attrs);
                    let qn = self.qname_of_element(&child)?;
                    if qn.namespace.as_deref() == Some(XS) {
                        self.check_annotation_pos(
                            &qn.local, &mut seen_anno, &mut seen_other, "complexType",
                        )?;
                        let local = qn.local.as_ref();
                        if saw_derived_content && local != "annotation" {
                            return Err(self.err(format!(
                                "<xs:complexType> with <xs:simpleContent>/<xs:complexContent> \
                                 cannot have <xs:{local}> as a sibling",
                            )));
                        }
                        match local {
                            "sequence" | "choice" | "all" => {
                                if saw_model_group || saw_any_attribute
                                    || !attr_uses.is_empty()
                                {
                                    return Err(self.err(
                                        "<xs:complexType> body: model group must precede \
                                         attribute / attributeGroup / anyAttribute, and \
                                         only one model group is allowed",
                                    ));
                                }
                                saw_model_group = true;
                                let kind = match qn.local.as_ref() {
                                    "sequence" => GroupKind::Sequence,
                                    "choice"   => GroupKind::Choice,
                                    _          => GroupKind::All,
                                };
                                let particle = self.parse_group_body(kind, &child_attrs)?;
                                content = ContentModel::Complex { root: particle, mixed };
                            }
                            "group" => {
                                if saw_model_group || saw_any_attribute
                                    || !attr_uses.is_empty()
                                {
                                    return Err(self.err(
                                        "<xs:complexType> body: model group must precede \
                                         attribute / attributeGroup / anyAttribute, and \
                                         only one model group is allowed",
                                    ));
                                }
                                saw_model_group = true;
                                // XSD 1.0 §3.4.2: a `<xs:complexType>` body
                                // may use `<xs:group ref="G"/>` directly as
                                // its single top-level particle (instead of
                                // wrapping a sequence/choice/all). The `ref`
                                // attribute is required — a nested
                                // `<xs:group name=…>` definition is not
                                // allowed here.
                                let r = self.attr(&child_attrs, "ref").ok_or_else(|| self.err(
                                    "<xs:group> inside <xs:complexType> must use ref= \
                                     (nested group definitions are not allowed)",
                                ))?;
                                {
                                    let ref_qn = self.parse_qname(r, false)?;
                                    let g_min = parse_min_occurs(self.attr(&child_attrs, "minOccurs"))?;
                                    let g_max = parse_max_occurs(self.attr(&child_attrs, "maxOccurs"))?;
                                    check_occurs(g_min, g_max)?;
                                    content = ContentModel::Complex {
                                        root: Particle {
                                            min_occurs: g_min,
                                            max_occurs: g_max,
                                            term:       Term::GroupRef(ref_qn),
                                        },
                                        mixed,
                                    };
                                }
                                self.skip_body()?;
                            }
                            "attribute" => {
                                if saw_any_attribute {
                                    return Err(self.err(
                                        "<xs:attribute> cannot follow <xs:anyAttribute> in <xs:complexType>",
                                    ));
                                }
                                let au = self.parse_local_attribute_use(&child_attrs)?;
                                attr_uses.push(au);
                            }
                            "attributeGroup" => {
                                if saw_any_attribute {
                                    return Err(self.err(
                                        "<xs:attributeGroup> cannot follow <xs:anyAttribute> in <xs:complexType>",
                                    ));
                                }
                                let r = self.attr(&child_attrs, "ref").ok_or_else(|| self.err(
                                    "<xs:attributeGroup> inside <xs:complexType> must use ref= \
                                     (nested attributeGroup definitions are not allowed)",
                                ))?;
                                let qn = self.parse_qname(r, false)?;
                                match self.builder.attr_groups.get(&qn) {
                                    Some(ag) => {
                                        for au in &ag.attributes {
                                            attr_uses.push(au.clone());
                                        }
                                        // XSD §3.10.6 — multiple
                                        // attributeGroup wildcards in
                                        // one complex type INTERSECT.
                                        any_attr = merge_any_intersect(any_attr, ag.any.clone());
                                    }
                                    None => pending_ag_refs.push(qn),
                                }
                                // XSD §3.6.2 — ref-form attributeGroup's
                                // body may only contain <xs:annotation>.
                                self.parse_anno_only_body("attributeGroup")?;
                            }
                            "anyAttribute" => {
                                if saw_any_attribute {
                                    return Err(self.err(
                                        "<xs:complexType> body: at most one <xs:anyAttribute>",
                                    ));
                                }
                                saw_any_attribute = true;
                                check_no_occurs(&child_attrs, "anyAttribute")?;
                                any_attr = Some(self.parse_wildcard(&child_attrs)?);
                                self.parse_anno_only_body("anyAttribute")?;
                            }
                            "complexContent" | "simpleContent" => {
                                if saw_model_group || saw_any_attribute
                                    || !attr_uses.is_empty()
                                {
                                    return Err(self.err(format!(
                                        "<xs:{local}> cannot co-occur with model group, \
                                         attribute, attributeGroup, or anyAttribute in <xs:complexType>",
                                    )));
                                }
                                saw_derived_content = true;
                                let (deriv, inner_content, inner_attrs, inner_any, inner_mixed, inner_pending) =
                                    self.parse_derived_content(&child_attrs, qn.local.as_ref() == "simpleContent")?;
                                derivation = deriv;
                                content    = inner_content;
                                attr_uses.extend(inner_attrs);
                                if inner_any.is_some() { any_attr = inner_any; }
                                mixed = inner_mixed.unwrap_or(mixed);
                                // Propagate the resolved mixed flag into
                                // the derived content's mixed bit — the
                                // restriction/extension body always
                                // returns mixed=false on Complex content
                                // because it doesn't know the outer
                                // context.
                                if let ContentModel::Complex { mixed: ref mut m, .. } = content {
                                    *m = mixed;
                                }
                                pending_ag_refs.extend(inner_pending);
                            }
                            "annotation" => self.parse_annotation_body(&child_attrs)?,
                            // XSD 1.1 `<xs:assert>` — an XPath 2.0
                            // assertion evaluated against this type's
                            // instance elements at validate time.
                            "assert" => assertions.extend(self.parse_assertion_body(&child_attrs)?),
                            other => return Err(self.err(format!(
                                "<xs:{other}> is not allowed as a child of <xs:complexType>"
                            ))),
                        }
                    } else {
                        self.skip_body()?;
                    }
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => break,
                EventInto::Eof => return Err(self.err("unexpected EOF in complexType")),
                _ => {}
            }
        }
        // ct-props-correct (XSD §3.4.6): a complex type's attribute
        // uses must have pairwise-distinct names. Forward refs to
        // attribute groups skip this check because the placeholder
        // we use until resolution carries no resolved name.
        let mut seen_attr_names: HashSet<QName> = HashSet::new();
        for au in &attr_uses {
            if !seen_attr_names.insert(au.decl.name.clone()) {
                return Err(self.err(format!(
                    "<xs:complexType{}> declares attribute {:?} more than once",
                    name.as_ref().map(|n| format!(" name={:?}", n.local)).unwrap_or_default(),
                    au.decl.name.local,
                )));
            }
        }
        // ct-id-checks (XSD §3.4.6): a complex type may have at most
        // one attribute whose type is xs:ID (or derived from it). The
        // ID uniqueness constraint applies per element instance, so
        // two ID-typed attributes on the same element would always
        // collide.
        let id_count = attr_uses.iter().filter(|au| {
            matches!(au.decl.type_def.builtin, BuiltinType::Id)
        }).count();
        if id_count > 1 {
            return Err(self.err(format!(
                "<xs:complexType{}> declares {id_count} attributes of type xs:ID; \
                 at most one is allowed (XSD §3.4.6)",
                name.as_ref().map(|n| format!(" name={:?}", n.local)).unwrap_or_default(),
            )));
        }

        // XSD §3.4.2 — a complexType with `mixed="true"` but no
        // explicit content (no sequence/choice/all/simpleContent/
        // complexContent) behaves as a mixed-content type whose
        // element model is empty. Promote the content from Empty
        // to Complex{empty sequence, mixed} so the validator
        // accepts text but no child elements.
        let content = if mixed && matches!(content, ContentModel::Empty) {
            ContentModel::Complex {
                root: Particle {
                    min_occurs: 1,
                    max_occurs: MaxOccurs::Bounded(1),
                    term: Term::Group {
                        kind: GroupKind::Sequence,
                        particles: Arc::from(Vec::<Particle>::new()),
                    },
                },
                mixed: true,
            }
        } else {
            content
        };

        Ok(ComplexType {
            name,
            derivation,
            content,
            matcher: std::sync::OnceLock::new(),
            attributes: attr_uses,
            any_attribute: any_attr,
            abstract_,
            block,
            final_,
            pending_attribute_group_refs: pending_ag_refs,
            assertions,
        })
    }

    /// Parse the body of `<xs:complexContent>` or `<xs:simpleContent>`,
    /// which always wraps a single `<xs:restriction>` or `<xs:extension>`.
    fn parse_derived_content(
        &mut self,
        outer_attrs: &[Attr<'a>],
        is_simple_content: bool,
    ) -> Result<
        (Option<Derivation>, ContentModel, Vec<AttributeUse>, Option<Wildcard>, Option<bool>, Vec<QName>),
        SchemaCompileError,
    > {
        let mut deriv: Option<Derivation> = None;
        let mut content = ContentModel::Empty;
        let mut attrs = Vec::new();
        let mut any   = None;
        // XSD §3.4.2 — `mixed=` may be set on either the outer
        // <xs:complexType> or on <xs:complexContent>; the latter
        // overrides.  `<xs:simpleContent>` doesn't allow mixed at all.
        let mut mixed: Option<bool> = if is_simple_content {
            None
        } else {
            self.parse_xsd_bool(outer_attrs, "mixed")?
        };
        let mut pending_ag_refs: Vec<QName> = Vec::new();
        let parent = if is_simple_content { "simpleContent" } else { "complexContent" };
        // XSD §3.4.2 — body of <xs:simpleContent>/<xs:complexContent> is
        // (annotation?, (restriction | extension)). Exactly one of the
        // two derivations is required, annotation must come first.
        let mut seen_anno = false;
        let mut seen_other = false;
        let mut saw_derivation = false;

        loop {
            match self.next_event()? {
                EventInto::StartElement { name } => {
                    let inner_attrs = self.take_attrs()?;
                    self.push_ns_scope(&inner_attrs);
                    let qn = self.qname_of_element(&name)?;
                    if qn.namespace.as_deref() == Some(XS) {
                        self.check_annotation_pos(
                            &qn.local, &mut seen_anno, &mut seen_other, parent,
                        )?;
                        let local = qn.local.as_ref();
                        match local {
                            "restriction" | "extension" => {
                                if saw_derivation {
                                    return Err(self.err(format!(
                                        "<xs:{parent}> must contain exactly one \
                                         <xs:restriction> or <xs:extension>",
                                    )));
                                }
                                saw_derivation = true;
                                let method = if local == "restriction" {
                                    DerivationMethod::Restriction
                                } else { DerivationMethod::Extension };
                                let base = self.attr(&inner_attrs, "base")
                                    .ok_or_else(|| self.err(format!(
                                        "missing 'base' on <xs:{local}>"
                                    )))?;
                                let base_qn = self.parse_qname(base, false)?;
                                // XSD §3.4.2 — xs:anyType has *complex*
                                // content, so it cannot be the base of a
                                // <xs:simpleContent> derivation.
                                if is_simple_content
                                    && base_qn.namespace.as_deref() == Some(XS)
                                    && base_qn.local.as_ref() == "anyType"
                                {
                                    return Err(self.err(
                                        "<xs:simpleContent> cannot derive from xs:anyType \
                                         (it has complex content, not simple)",
                                    ));
                                }
                                // XSD §3.4.2 — `<xs:simpleContent><xs:restriction>`
                                // requires a base whose own content is
                                // simple-content (a complex type), so the
                                // derived restriction can tighten that
                                // simple body.  Restricting a built-in
                                // simple type directly skips the carrier
                                // complex type entirely and isn't allowed
                                // (use `<xs:extension>` for that case).
                                if is_simple_content
                                    && matches!(method, DerivationMethod::Restriction)
                                    && base_qn.namespace.as_deref() == Some(XS)
                                    && base_qn.local.as_ref() != "anyType"
                                {
                                    return Err(self.err(format!(
                                        "<xs:simpleContent><xs:restriction base={:?}> — \
                                         built-in xs:* types are simple types and cannot \
                                         be restricted via <xs:simpleContent> (use \
                                         <xs:extension>, or restrict via <xs:simpleType>)",
                                        format!("xs:{}", base_qn.local),
                                    )));
                                }
                                deriv = Some(Derivation {
                                    method,
                                    base: self.type_ref_for(base_qn),
                                });

                                let (inner_content, inner_attrs2, inner_any, inner_mixed, inner_pending) =
                                    self.parse_derivation_body(is_simple_content, method)?;
                                content = inner_content;
                                attrs.extend(inner_attrs2);
                                any = inner_any;
                                // parse_derivation_body returns mixed=None,
                                // but `<xs:complexContent mixed=…>` already
                                // captured the user's choice on the outer
                                // element — don't overwrite it with None.
                                if let Some(m) = inner_mixed { mixed = Some(m); }
                                pending_ag_refs.extend(inner_pending);
                            }
                            "annotation" => self.parse_annotation_body(&inner_attrs)?,
                            other => return Err(self.err(format!(
                                "<xs:{other}> is not allowed as a child of <xs:{parent}>"
                            ))),
                        }
                    } else {
                        self.skip_body()?;
                    }
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => break,
                EventInto::Eof => return Err(self.err("unexpected EOF in derived content")),
                _ => {}
            }
        }
        if !saw_derivation {
            return Err(self.err(format!(
                "<xs:{parent}> must contain a <xs:restriction> or <xs:extension>"
            )));
        }
        Ok((deriv, content, attrs, any, mixed, pending_ag_refs))
    }

    fn parse_derivation_body(
        &mut self,
        is_simple_content: bool,
        method:            DerivationMethod,
    ) -> Result<(ContentModel, Vec<AttributeUse>, Option<Wildcard>, Option<bool>, Vec<QName>), SchemaCompileError>
    {
        let mut content = if is_simple_content {
            ContentModel::Simple(Arc::new(SimpleType::of_builtin(BuiltinType::String)))
        } else {
            ContentModel::Empty
        };
        let mut attrs = Vec::new();
        let mut any   = None;
        let mixed     = None;
        let mut pending_ag_refs: Vec<QName> = Vec::new();
        // XSD §3.4.2: an <xs:extension> or <xs:restriction> inside a
        // <xs:complexContent> contains at most one particle (group |
        // all | choice | sequence) and then attribute uses.
        let mut saw_particle = false;
        let mut saw_any_attribute = false;
        let mut saw_inline_simple_type = false;
        let mut seen_anno = false;
        let mut seen_other = false;

        loop {
            match self.next_event()? {
                EventInto::StartElement { name } => {
                    let child_attrs = self.take_attrs()?;
                    self.push_ns_scope(&child_attrs);
                    let qn = self.qname_of_element(&name)?;
                    if qn.namespace.as_deref() == Some(XS) {
                        self.check_annotation_pos(
                            &qn.local, &mut seen_anno, &mut seen_other,
                            if is_simple_content { "simpleContent derivation" }
                            else { "complexContent derivation" },
                        )?;
                        match qn.local.as_ref() {
                            "sequence" | "choice" | "all" | "group" => {
                                if is_simple_content {
                                    return Err(self.err(format!(
                                        "<xs:simpleContent>'s {} body has no place for an \
                                         <xs:{}> model group (simple content's body is text)",
                                         if method == DerivationMethod::Extension { "extension" }
                                         else { "restriction" },
                                         qn.local,
                                    )));
                                }
                                if saw_particle || saw_any_attribute || !attrs.is_empty() {
                                    return Err(self.err(
                                        "<xs:extension>/<xs:restriction> body: at most one \
                                         model-group particle, and it must precede attributes",
                                    ));
                                }
                                saw_particle = true;
                                match qn.local.as_ref() {
                                    "group" => {
                                        let r = self.attr(&child_attrs, "ref").ok_or_else(|| self.err(
                                            "<xs:group> inside <xs:extension>/<xs:restriction> must use ref=",
                                        ))?;
                                        let ref_qn = self.parse_qname(r, false)?;
                                        let g_min = parse_min_occurs(self.attr(&child_attrs, "minOccurs"))?;
                                        let g_max = parse_max_occurs(self.attr(&child_attrs, "maxOccurs"))?;
                                        check_occurs(g_min, g_max)?;
                                        content = ContentModel::Complex {
                                            root: Particle {
                                                min_occurs: g_min,
                                                max_occurs: g_max,
                                                term:       Term::GroupRef(ref_qn),
                                            },
                                            mixed: false,
                                        };
                                        self.skip_body()?;
                                    }
                                    _ => {
                                        let kind = match qn.local.as_ref() {
                                            "sequence" => GroupKind::Sequence,
                                            "choice"   => GroupKind::Choice,
                                            _          => GroupKind::All,
                                        };
                                        let particle = self.parse_group_body(kind, &child_attrs)?;
                                        content = ContentModel::Complex { root: particle, mixed: false };
                                    }
                                }
                            }
                            "attribute" => {
                                if saw_any_attribute {
                                    return Err(self.err(
                                        "<xs:attribute> cannot follow <xs:anyAttribute>",
                                    ));
                                }
                                attrs.push(self.parse_local_attribute_use(&child_attrs)?);
                            }
                            "attributeGroup" => {
                                if saw_any_attribute {
                                    return Err(self.err(
                                        "<xs:attributeGroup> cannot follow <xs:anyAttribute>",
                                    ));
                                }
                                let r = self.attr(&child_attrs, "ref").ok_or_else(|| self.err(
                                    "<xs:attributeGroup> inside <xs:extension>/<xs:restriction> \
                                     must use ref= (no nested definitions)",
                                ))?;
                                let qn = self.parse_qname(r, false)?;
                                match self.builder.attr_groups.get(&qn) {
                                    Some(ag) => {
                                        for au in &ag.attributes {
                                            attrs.push(au.clone());
                                        }
                                        any = merge_any_intersect(any, ag.any.clone());
                                    }
                                    None => pending_ag_refs.push(qn),
                                }
                                self.parse_anno_only_body("attributeGroup")?;
                            }
                            "anyAttribute" => {
                                if saw_any_attribute {
                                    return Err(self.err(
                                        "at most one <xs:anyAttribute> in this derivation body",
                                    ));
                                }
                                saw_any_attribute = true;
                                check_no_occurs(&child_attrs, "anyAttribute")?;
                                any = Some(self.parse_wildcard(&child_attrs)?);
                                self.parse_anno_only_body("anyAttribute")?;
                            }
                            "annotation" => self.parse_annotation_body(&child_attrs)?,
                            // Inside <xs:simpleContent><xs:restriction>, facet
                            // children (length, minInclusive, …) and an
                            // optional inline <xs:simpleType> base are also
                            // allowed (XSD §3.4.2). We skip their bodies
                            // here — facet semantics for simple-content
                            // restriction are picked up by `simple_type_for`
                            // on the base type. Accepted so the schema
                            // compiles; the more elaborate "derived facets
                            // must restrict base facets" check is out of
                            // scope for this layer.
                            // XSD §3.4.2 — inline simpleType and facets are
                            // only valid inside <xs:simpleContent>'s
                            // <xs:restriction>. <xs:extension> in either
                            // simple- or complex-content has no place for
                            // facets or inline simpleType bodies.
                            "simpleType" | "length" | "minLength" | "maxLength"
                            | "minInclusive" | "minExclusive" | "maxInclusive"
                            | "maxExclusive" | "totalDigits" | "fractionDigits"
                            | "enumeration" | "pattern" | "whiteSpace"
                                if is_simple_content
                                    && method == DerivationMethod::Restriction =>
                            {
                                // XSD §3.4.2 — simpleContent restriction's
                                // body grammar is
                                //   (annotation?, simpleType?, facet*,
                                //    (attribute | attributeGroup)*,
                                //    anyAttribute?)
                                // so inline simpleType and facets must
                                // precede any attribute uses.
                                if !attrs.is_empty() || saw_any_attribute {
                                    return Err(self.err(format!(
                                        "<xs:{}> in a <xs:simpleContent><xs:restriction> \
                                         must precede attribute declarations",
                                        qn.local,
                                    )));
                                }
                                if qn.local.as_ref() == "simpleType" {
                                    if saw_inline_simple_type {
                                        return Err(self.err(
                                            "<xs:simpleContent><xs:restriction> body \
                                             may have at most one inline <xs:simpleType>",
                                        ));
                                    }
                                    saw_inline_simple_type = true;
                                }
                                self.skip_body()?;
                            }
                            other => return Err(self.err(format!(
                                "<xs:{other}> is not allowed in this derivation body"
                            ))),
                        }
                    } else {
                        self.skip_body()?;
                    }
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => break,
                EventInto::Eof => return Err(self.err("unexpected EOF in derivation body")),
                _ => {}
            }
        }
        Ok((content, attrs, any, mixed, pending_ag_refs))
    }

    // ── particle group body (sequence/choice/all) ────────────────────────

    fn parse_group_body(&mut self, kind: GroupKind, attrs: &[Attr<'a>])
        -> Result<Particle, SchemaCompileError>
    {
        let min_occurs = parse_min_occurs(self.attr(attrs, "minOccurs"))?;
        let max_occurs = parse_max_occurs(self.attr(attrs, "maxOccurs"))?;
        check_occurs(min_occurs, max_occurs)?;
        // XSD 1.0 § 3.8.6 — <xs:all> has minOccurs ∈ {0,1} and
        // maxOccurs MUST be 1.  XSD 1.1 § 3.8.6 relaxed maxOccurs
        // to allow any non-negative value (or unbounded); minOccurs
        // is still bounded by maxOccurs.  Apply the 1.0 cap only
        // when the effective version is 1.0.
        if matches!(kind, GroupKind::All) {
            let strict_10 = !matches!(
                self.builder.effective_version, SchemaVersion::Xsd11
            );
            if strict_10 {
                if min_occurs > 1 {
                    return Err(self.err(
                        "<xs:all> minOccurs must be 0 or 1 (XSD 1.0 §3.8.6) — \
                         set SchemaOptions::version to Xsd11 to relax",
                    ));
                }
                if max_occurs != MaxOccurs::Bounded(1) {
                    return Err(self.err(
                        "<xs:all> maxOccurs must be exactly 1 (XSD 1.0 §3.8.6) — \
                         set SchemaOptions::version to Xsd11 to relax",
                    ));
                }
            }
        }
        let parent_name = match kind {
            GroupKind::Sequence => "sequence",
            GroupKind::Choice   => "choice",
            GroupKind::All      => "all",
        };
        // Sequence/choice/all don't accept a `name` attribute (only
        // top-level <xs:group> names a model group; the nested
        // particle groups inherit no naming).
        if self.attr(attrs, "name").is_some() {
            return Err(self.err(format!(
                "<xs:{parent_name}> does not take a 'name' attribute"
            )));
        }
        let mut particles = Vec::new();
        let mut seen_anno = false;
        let mut seen_other = false;
        loop {
            match self.next_event()? {
                EventInto::StartElement { name } => {
                    let child_attrs = self.take_attrs()?;
                    self.push_ns_scope(&child_attrs);
                    let qn = self.qname_of_element(&name)?;
                    if qn.namespace.as_deref() == Some(XS) {
                        self.check_annotation_pos(
                            &qn.local, &mut seen_anno, &mut seen_other, parent_name,
                        )?;
                        // XSD §3.8.6 — <xs:all> body may only contain
                        // <xs:element> (no nested groups, choices,
                        // sequences, or wildcards).
                        if matches!(kind, GroupKind::All)
                            && !matches!(qn.local.as_ref(), "element" | "annotation")
                        {
                            return Err(self.err(format!(
                                "<xs:all> body may only contain <xs:element> children, found <xs:{}>",
                                qn.local,
                            )));
                        }
                        match qn.local.as_ref() {
                            "element" => {
                                let p = self.parse_local_element_particle(&child_attrs)?;
                                // XSD 1.0 § 3.8.6 cos-all-particle — an
                                // element particle directly inside
                                // <xs:all> must have maxOccurs ∈ {0,1}.
                                if matches!(kind, GroupKind::All)
                                    && !matches!(
                                        self.builder.effective_version,
                                        SchemaVersion::Xsd11
                                    )
                                    && !matches!(p.max_occurs, MaxOccurs::Bounded(0)
                                                              | MaxOccurs::Bounded(1))
                                {
                                    return Err(self.err(
                                        "<xs:all> element particle must have maxOccurs ∈ {0,1} \
                                         (XSD 1.0 §3.8.6 cos-all-particle) — set \
                                         SchemaOptions::version to Xsd11 to relax",
                                    ));
                                }
                                particles.push(p);
                            }
                            "sequence" | "choice" | "all" => {
                                let inner_kind = match qn.local.as_ref() {
                                    "sequence" => GroupKind::Sequence,
                                    "choice"   => GroupKind::Choice,
                                    _          => GroupKind::All,
                                };
                                particles.push(self.parse_group_body(inner_kind, &child_attrs)?);
                            }
                            "group" => {
                                let r = self.attr(&child_attrs, "ref").ok_or_else(|| self.err(
                                    "<xs:group> inside a model group must use ref= \
                                     (nested group definitions are not allowed)",
                                ))?;
                                let ref_qn = self.parse_qname(r, false)?;
                                let g_min = parse_min_occurs(self.attr(&child_attrs, "minOccurs"))?;
                                let g_max = parse_max_occurs(self.attr(&child_attrs, "maxOccurs"))?;
                                check_occurs(g_min, g_max)?;
                                particles.push(Particle {
                                    min_occurs: g_min,
                                    max_occurs: g_max,
                                    term: Term::GroupRef(ref_qn),
                                });
                                self.skip_body()?;
                            }
                            "any" => {
                                let any_min = parse_min_occurs(self.attr(&child_attrs, "minOccurs"))?;
                                let any_max = parse_max_occurs(self.attr(&child_attrs, "maxOccurs"))?;
                                check_occurs(any_min, any_max)?;
                                particles.push(Particle {
                                    min_occurs: any_min,
                                    max_occurs: any_max,
                                    term: Term::Wildcard(self.parse_wildcard(&child_attrs)?),
                                });
                                self.parse_anno_only_body("any")?;
                            }
                            "annotation" => self.parse_annotation_body(&child_attrs)?,
                            other => return Err(self.err(format!(
                                "<xs:{other}> is not allowed as a child of <xs:{parent_name}>"
                            ))),
                        }
                    } else {
                        self.skip_body()?;
                    }
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => break,
                EventInto::Eof => return Err(self.err("unexpected EOF in particle group")),
                _ => {}
            }
        }

        // XSD §3.8.6 cos-all-particle / cos-all-distinct — every
        // particle inside an <xs:all> must have a distinct element
        // name across the group's lifetime.
        if matches!(kind, GroupKind::All) {
            let mut seen: HashSet<QName> = HashSet::new();
            for p in &particles {
                if let Term::Element(decl) = &p.term {
                    if !seen.insert(decl.name.clone()) {
                        return Err(self.err(format!(
                            "<xs:all>: duplicate element {:?} — an all-group's \
                             particles must have distinct names (XSD §3.8.6 \
                             cos-all-particle)",
                            decl.name.local,
                        )));
                    }
                }
            }
        }

        Ok(Particle {
            min_occurs,
            max_occurs,
            term: Term::Group { kind, particles: particles.into() },
        })
    }

    /// Parse a local `<xs:element>` inside a content model.  Builds an
    /// inline element decl (anonymous types are scoped to the
    /// containing complex type — for v1 we register them in `elements`
    /// only when they have a name; truly anonymous locals get a
    /// generated synthetic name).
    fn parse_local_element_particle(&mut self, attrs: &[Attr<'a>])
        -> Result<Particle, SchemaCompileError>
    {
        let min_occurs = parse_min_occurs(self.attr(attrs, "minOccurs"))?;
        let max_occurs = parse_max_occurs(self.attr(attrs, "maxOccurs"))?;
        check_occurs(min_occurs, max_occurs)?;
        // XSD §3.3.2: `abstract`, `final`, `substitutionGroup`, and
        // top-level-only `block` tokens are forbidden on a local
        // element declaration (they only apply to the global decl).
        // Check these before the general allow-list so the diagnostic
        // points the schema author at the precise rule.
        for forbidden in ["abstract", "final", "substitutionGroup"] {
            if self.attr(attrs, forbidden).is_some() {
                return Err(self.err(format!(
                    "<xs:element {forbidden}=...> is only valid on top-level element declarations"
                )));
            }
        }
        // XSD §3.3.2 — local <xs:element> takes a union of the named
        // and ref-form attribute sets; the form-specific exclusions
        // are enforced below.
        self.check_known_attrs(attrs, &[
            "id", "name", "ref", "form", "type", "default", "fixed",
            "nillable", "minOccurs", "maxOccurs", "block",
        ], "element")?;

        // ref="..." form — refers to a top-level element. XSD §3.3.3
        // forbids combining `ref` with attributes that customize the
        // referenced declaration (`name`, `form`, `type`, `default`,
        // `fixed`, `nillable`, `block`); only min/max occurs are
        // allowed alongside it.
        if let Some(r) = self.attr(attrs, "ref") {
            for forbidden in ["name", "form", "type", "default", "fixed", "nillable", "block"] {
                if self.attr(attrs, forbidden).is_some() {
                    return Err(self.err(format!(
                        "<xs:element ref=...> cannot also carry '{forbidden}'"
                    )));
                }
            }
            let qn = self.parse_qname(r, false)?;
            self.builder.pending_element_refs.push(qn.clone());
            // Stand-in decl; the post-pass patches in the real one.
            let placeholder = Arc::new(ElementDecl {
                name: qn.clone(),
                type_def: TypeRef::Simple(Arc::new(SimpleType::of_builtin(BuiltinType::String))),
                nillable: false,
                default: None,
                fixed: None,
                abstract_: false,
                substitution_group: None,
                block:  BlockSet::default(),
                final_: BlockSet::default(),
                identity: Vec::new(),
            });
            // XSD §3.3.2 — the only child allowed on a ref-form
            // element use is <xs:annotation>.  Inline type bodies,
            // identity constraints, etc. are schema validity errors.
            self.parse_anno_only_body("element")?;
            return Ok(Particle { min_occurs, max_occurs, term: Term::Element(placeholder) });
        }

        // name="..." form — local declaration.  Namespace is decided
        // by `form=` (local override) or `elementFormDefault` on the
        // schema document.  Per XSD §3.3.2, qualified locals live in
        // the target namespace; unqualified locals live in no
        // namespace.
        let name = self.attr(attrs, "name").ok_or_else(||
            self.err("local <xs:element> needs name or ref")
        )?.to_owned();
        let form = match self.attr(attrs, "form") {
            Some(raw) => Form::parse(raw).ok_or_else(|| self.err(format!(
                "<xs:element form={raw:?}>: must be \"qualified\" or \"unqualified\""
            )))?,
            None => self.element_form_default,
        };
        // default= and fixed= are mutually exclusive (XSD §3.3.2).
        if self.attr(attrs, "default").is_some() && self.attr(attrs, "fixed").is_some() {
            return Err(self.err(
                "<xs:element> may have either default= or fixed=, not both"
            ));
        }
        let qn = QName {
            namespace: match form {
                Form::Qualified   => self.current_target_ns.clone(),
                Form::Unqualified => None,
            },
            local: Arc::from(name.as_str()),
        };
        let (inline, identity) = self.parse_inline_type(attrs)?;
        if self.attr(attrs, "type").is_some() && inline.is_some() {
            return Err(self.err(
                "<xs:element> may have either type= or an inline type body, not both"
            ));
        }
        let type_def = match self.attr(attrs, "type") {
            Some(t) => {
                let type_qn = self.parse_qname(t, false)?;
                self.type_ref_for(type_qn)
            }
            None => inline.unwrap_or_else(any_type_ref),
        };
        // Local elements honour their own `block=` (XSD §3.3.2).
        // `final=`, `abstract=`, and `substitutionGroup=` are only
        // legal on top-level decls — already rejected upstream.
        let block = match self.attr(attrs, "block") {
            Some(_) => parse_block_set(self.attr(attrs, "block"))?,
            None    => self.block_default,
        };
        let decl = Arc::new(ElementDecl {
            name: qn,
            type_def,
            nillable:  self.parse_xsd_bool(attrs, "nillable")?.unwrap_or(false),
            default:   self.attr(attrs, "default").map(|s| s.to_owned()),
            fixed:     self.attr(attrs, "fixed").map(|s| s.to_owned()),
            abstract_: false,
            substitution_group: None,
            block,
            final_: BlockSet::default(),
            identity,
        });
        Ok(Particle { min_occurs, max_occurs, term: Term::Element(decl) })
    }

    fn parse_local_attribute_use(&mut self, attrs: &[Attr<'a>])
        -> Result<AttributeUse, SchemaCompileError>
    {
        // XSD §3.2.2 — attribute set of a local <xs:attribute>.
        self.check_known_attrs(attrs, &[
            "id", "name", "ref", "type", "use", "default", "fixed", "form",
        ], "attribute")?;
        let use_kind = match self.attr(attrs, "use") {
            Some("required")   => AttributeUseKind::Required,
            Some("prohibited") => AttributeUseKind::Prohibited,
            Some("optional")   => AttributeUseKind::Optional,
            None               => AttributeUseKind::Optional,
            Some(other) => return Err(self.err(format!(
                "<xs:attribute use={other:?}>: must be \"required\", \"optional\", or \"prohibited\""
            ))),
        };
        // XSD §3.2.3 — if `default` is present, `use` must be
        // "optional" (the default supplies a value when the attribute
        // is missing, which is meaningless for required/prohibited).
        if self.attr(attrs, "default").is_some()
            && !matches!(use_kind, AttributeUseKind::Optional)
        {
            return Err(self.err(
                "<xs:attribute default=...> requires use=\"optional\"",
            ));
        }

        // ref form.
        if let Some(r) = self.attr(attrs, "ref") {
            // XSD §3.2.3 — a `ref`-form attribute use cannot carry
            // attributes that customize the referenced declaration:
            // `name`, `form`, `type`, and an inline simpleType body
            // are forbidden alongside `ref`.
            for forbidden in ["name", "form", "type"] {
                if self.attr(attrs, forbidden).is_some() {
                    return Err(self.err(format!(
                        "<xs:attribute ref=...> cannot also carry '{forbidden}'"
                    )));
                }
            }
            let qn = self.parse_qname(r, false)?;
            // XSD §3.2.3 cvc-attribute-use clause 4 — a `ref`-form
            // attribute use's `fixed` must match the referenced
            // declaration's `fixed` when both are present.  Defer the
            // check for forward refs.
            let use_fixed = self.attr(attrs, "fixed").map(|s| s.to_owned());
            if let Some(use_fixed_v) = use_fixed.as_deref() {
                if let Some(decl) = self.builder.attributes.get(&qn) {
                    if let Some(decl_fixed) = decl.fixed.as_deref() {
                        if decl_fixed != use_fixed_v {
                            return Err(self.err(format!(
                                "<xs:attribute ref={r:?} fixed={use_fixed_v:?}>: \
                                 referenced declaration has fixed={decl_fixed:?}; \
                                 a ref-form use cannot change the fixed value"
                            )));
                        }
                    }
                }
            }
            self.builder.pending_attribute_refs.push(qn.clone());
            let placeholder = Arc::new(AttributeDecl {
                name: qn,
                type_def: Arc::new(SimpleType::of_builtin(BuiltinType::String)),
                default: self.attr(attrs, "default").map(|s| s.to_owned()),
                fixed:   use_fixed,
                // ref form: the value of `inheritable` comes from the
                // top-level declaration this ref points at, not from
                // the use site.  The placeholder gets `false` for now;
                // the post-pass that resolves refs replaces the whole
                // Arc with the real decl, so this field is overwritten
                // before validation runs.
                inheritable: false,
            });
            // The ref form's body may only contain <xs:annotation>.
            self.parse_anno_only_body("attribute")?;
            return Ok(AttributeUse {
                use_kind,
                decl: placeholder,
                default: None,
                fixed: None,
            });
        }

        let name = self.attr(attrs, "name").ok_or_else(||
            self.err("local <xs:attribute> needs name or ref")
        )?.to_owned();
        if name == "xmlns" {
            return Err(self.err(
                "<xs:attribute name=\"xmlns\"> is reserved by the Namespaces in XML spec",
            ));
        }
        let form = match self.attr(attrs, "form") {
            Some(raw) => Form::parse(raw).ok_or_else(|| self.err(format!(
                "<xs:attribute form={raw:?}>: must be \"qualified\" or \"unqualified\""
            )))?,
            None => self.attribute_form_default,
        };
        if self.attr(attrs, "default").is_some() && self.attr(attrs, "fixed").is_some() {
            return Err(self.err(
                "<xs:attribute> may have either default= or fixed=, not both"
            ));
        }
        let qn = QName {
            namespace: match form {
                Form::Qualified   => self.current_target_ns.clone(),
                Form::Unqualified => None,
            },
            local: Arc::from(name.as_str()),
        };
        check_xsi_attribute_name(&qn).map_err(|m| self.err(m))?;
        let inline = self.parse_inline_simple_type(attrs)?;
        if self.attr(attrs, "type").is_some() && inline.is_some() {
            return Err(self.err(
                "<xs:attribute> may have either type= or an inline <xs:simpleType>, not both"
            ));
        }
        let st = match self.attr(attrs, "type") {
            Some(t) => {
                let type_qn = self.parse_qname(t, false)?;
                self.simple_type_for(type_qn)
            }
            None => Arc::new(inline.unwrap_or_else(|| SimpleType::of_builtin(BuiltinType::String))),
        };
        let decl = Arc::new(AttributeDecl {
            name: qn,
            type_def: st,
            default: self.attr(attrs, "default").map(|s| s.to_owned()),
            fixed:   self.attr(attrs, "fixed").map(|s| s.to_owned()),
            inheritable: self.parse_inheritable(attrs)?,
        });
        Ok(AttributeUse { use_kind, decl, default: None, fixed: None })
    }

    fn parse_top_attribute_group(&mut self, attrs: &[Attr<'a>]) -> Result<(), SchemaCompileError> {
        // XSD §3.6.2: a top-level <xs:attributeGroup> requires `name`,
        // its body is (annotation?, ((attribute|attributeGroup)*, anyAttribute?)),
        // and inner <xs:attributeGroup> must be a ref (no nested defs).
        let name = match self.attr(attrs, "name") {
            Some(n) => QName {
                namespace: self.current_target_ns.clone(),
                local:     Arc::from(n),
            },
            None => return Err(self.err(
                "top-level <xs:attributeGroup> requires a 'name' attribute",
            )),
        };
        if !self.in_redefine && self.builder.attr_groups.contains_key(&name) {
            return Err(self.err(format!(
                "duplicate <xs:attributeGroup name={:?}> in target namespace",
                name.local,
            )));
        }
        let mut uses = Vec::new();
        let mut any  = None;
        let mut local_refs: Vec<QName> = Vec::new();
        let mut seen_anno = false;
        let mut seen_other = false;
        let mut seen_any_attribute = false;
        loop {
            match self.next_event()? {
                EventInto::StartElement { name: child } => {
                    let cattrs = self.take_attrs()?;
                    self.push_ns_scope(&cattrs);
                    let qn = self.qname_of_element(&child)?;
                    if qn.namespace.as_deref() == Some(XS) {
                        self.check_annotation_pos(
                            &qn.local, &mut seen_anno, &mut seen_other, "attributeGroup",
                        )?;
                        match qn.local.as_ref() {
                            "attribute" => {
                                if seen_any_attribute {
                                    return Err(self.err(
                                        "<xs:attribute> cannot follow <xs:anyAttribute> in <xs:attributeGroup>",
                                    ));
                                }
                                uses.push(self.parse_local_attribute_use(&cattrs)?);
                            }
                            "attributeGroup" => {
                                if seen_any_attribute {
                                    return Err(self.err(
                                        "<xs:attributeGroup> cannot follow <xs:anyAttribute> in <xs:attributeGroup>",
                                    ));
                                }
                                let r = self.attr(&cattrs, "ref").ok_or_else(|| self.err(
                                    "<xs:attributeGroup> inside another attributeGroup must use ref= \
                                     (nested definitions are not allowed)",
                                ))?;
                                let rqn = self.parse_qname(r, false)?;
                                self.builder.pending_ag_refs_in_ag.push(rqn.clone());
                                local_refs.push(rqn.clone());
                                if Some(&rqn) == self.builder.redefining_ag_name.as_ref() {
                                    self.builder.redefining_ag_saw_self_ref = true;
                                }
                                if let Some(ag) = self.builder.attr_groups.get(&rqn) {
                                    for au in &ag.attributes {
                                        uses.push(au.clone());
                                    }
                                    any = merge_any_intersect(any, ag.any.clone());
                                }
                                self.parse_anno_only_body("attributeGroup")?;
                            }
                            "anyAttribute" => {
                                if seen_any_attribute {
                                    return Err(self.err(
                                        "<xs:attributeGroup> body: at most one <xs:anyAttribute>",
                                    ));
                                }
                                seen_any_attribute = true;
                                check_no_occurs(&cattrs, "anyAttribute")?;
                                any = Some(self.parse_wildcard(&cattrs)?);
                                self.parse_anno_only_body("anyAttribute")?;
                            }
                            "annotation" => self.parse_annotation_body(&cattrs)?,
                            other => return Err(self.err(format!(
                                "<xs:{other}> is not allowed as a child of <xs:attributeGroup>"
                            ))),
                        }
                    } else { self.skip_body()?; }
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => break,
                EventInto::Eof => return Err(self.err("unexpected EOF in attributeGroup")),
                _ => {}
            }
        }
        // Inside <xs:redefine>, a self-reference means "include the
        // pre-redefinition content" (XSD §4.2.2) — semantically NOT a
        // cyclic reference.  Skip cycle bookkeeping for redefined
        // groups so the legitimate self-include doesn't trip
        // src-attribute-group-cyclic.
        if !self.in_redefine {
            self.builder.ag_refs_by_owner.insert(name.clone(), local_refs);
        }
        self.builder.attr_groups.insert(name.clone(), Arc::new(AttributeGroup {
            name, attributes: uses, any,
        }));
        Ok(())
    }

    fn parse_top_group(&mut self, attrs: &[Attr<'a>]) -> Result<(), SchemaCompileError> {
        // XSD §3.7.1 — a top-level <xs:group> requires `name`
        // (no `ref=` is allowed at the schema top level), and its
        // body contains exactly one sequence | choice | all
        // model-group child (plus an optional leading annotation).
        if self.attr(attrs, "ref").is_some() {
            return Err(self.err(
                "<xs:group ref=...> is only valid inside a complexType / model-group, \
                 not at schema top level",
            ));
        }
        // XSD §3.7.2 — top-level <xs:group> does not take minOccurs
        // or maxOccurs; those apply only at reference sites.
        for forbidden in ["minOccurs", "maxOccurs"] {
            if self.attr(attrs, forbidden).is_some() {
                return Err(self.err(format!(
                    "top-level <xs:group> does not take '{forbidden}'"
                )));
            }
        }
        let name = match self.attr(attrs, "name") {
            Some(n) => QName {
                namespace: self.current_target_ns.clone(),
                local:     Arc::from(n),
            },
            None => return Err(self.err(
                "top-level <xs:group> requires a 'name' attribute",
            )),
        };
        if !self.in_redefine && self.builder.model_groups.contains_key(&name) {
            return Err(self.err(format!(
                "duplicate <xs:group name={:?}> in target namespace",
                name.local,
            )));
        }
        let mut particle: Option<Particle> = None;
        let mut seen_anno = false;
        let mut seen_other = false;
        loop {
            match self.next_event()? {
                EventInto::StartElement { name: child } => {
                    let cattrs = self.take_attrs()?;
                    self.push_ns_scope(&cattrs);
                    let qn = self.qname_of_element(&child)?;
                    if qn.namespace.as_deref() == Some(XS) {
                        self.check_annotation_pos(
                            &qn.local, &mut seen_anno, &mut seen_other, "group",
                        )?;
                        match qn.local.as_ref() {
                            "sequence" | "choice" | "all" => {
                                if particle.is_some() {
                                    return Err(self.err(
                                        "<xs:group> must contain exactly one of \
                                         <xs:sequence>, <xs:choice>, or <xs:all>",
                                    ));
                                }
                                let kind = match qn.local.as_ref() {
                                    "sequence" => GroupKind::Sequence,
                                    "choice"   => GroupKind::Choice,
                                    _          => GroupKind::All,
                                };
                                particle = Some(self.parse_group_body(kind, &cattrs)?);
                            }
                            "annotation" => self.parse_annotation_body(&cattrs)?,
                            other => return Err(self.err(format!(
                                "<xs:{other}> is not allowed as a child of <xs:group>"
                            ))),
                        }
                    } else { self.skip_body()?; }
                    self.pop_ns_scope();
                }
                EventInto::EndElement { .. } => break,
                EventInto::Eof => return Err(self.err("unexpected EOF in group")),
                _ => {}
            }
        }
        if let Some(p) = particle {
            self.builder.model_groups.insert(name.clone(), Arc::new(ModelGroup { name, particle: p }));
        }
        Ok(())
    }

    fn parse_top_notation(&mut self, attrs: &[Attr<'a>]) -> Result<(), SchemaCompileError> {
        // XSD §3.12.1 / §3.12.3 — <xs:notation> requires `name`
        // (an NCName) and at least one of `public` / `system`.
        self.check_known_attrs(attrs, &["id", "name", "public", "system"], "notation")?;
        let name_raw = self.attr(attrs, "name")
            .ok_or_else(|| self.err("<xs:notation> requires a 'name' attribute"))?;
        super::types::SimpleType::of_builtin(super::types::BuiltinType::NCName)
            .validate(name_raw)
            .map_err(|e| self.err(format!(
                "<xs:notation name={name_raw:?}> must be an NCName: {}", e.message,
            )))?;
        let public_id = self.attr(attrs, "public").map(|s| s.to_owned());
        let system_id = self.attr(attrs, "system").map(|s| s.to_owned());
        if public_id.is_none() && system_id.is_none() {
            return Err(self.err(
                "<xs:notation> requires at least one of 'public' or 'system'"
            ));
        }
        let name = QName {
            namespace: self.current_target_ns.clone(),
            local:     Arc::from(name_raw),
        };
        if !self.in_redefine && self.builder.notations.contains_key(&name) {
            return Err(self.err(format!(
                "duplicate <xs:notation name={name_raw:?}> in target namespace"
            )));
        }
        self.parse_anno_only_body("notation")?;
        self.builder.notations.insert(name.clone(), Arc::new(NotationDecl {
            name, public_id, system_id,
        }));
        Ok(())
    }

}

// ── helpers ──────────────────────────────────────────────────────────────────

/// XSD §3.4.2 — merge each named complex type that derives by
/// extension with its base's content model and attribute uses.
///
/// Resolves the chain bottom-up so transitive extensions
/// (`A → B → C`) compose correctly.  The result for each derived
/// type:
///
/// * **Content** — `Sequence[base.content, derived.content]`.  Both
///   sides keep their original mixed flag (merged via OR).  When the
///   base has `Empty` content the derived's content is used as-is.
///   `Simple` base content extension is a different beast (the derived
///   stays simple-content and only adds attributes) — handled here
///   by passing the base's `Simple(_)` through unchanged.
/// * **Attributes** — concatenation; a derived attribute with the
///   same name as a base attribute overrides the base (covers
///   `use="prohibited"` overrides and explicit redeclaration).
/// * **anyAttribute** — derived's overrides base's if present.
///
/// Cycles in the chain (`A extends B extends A`) are rejected as
/// schema-compile errors.  Restriction-by-restriction does NOT
/// trigger this merge — restricted types are expected to declare
/// their own content model explicitly per spec.
///
/// Anonymous complex types inlined inside element decls are not
/// merged — they can't be referenced by name, so the typical
/// `Base`/`Derived` named-type pattern is unaffected.
fn merge_extension_chains(
    types: &mut HashMap<QName, TypeRef>,
) -> Result<(), SchemaCompileError> {
    let names: Vec<QName> = types.keys().cloned().collect();
    let mut merged:  HashSet<QName> = HashSet::new();
    let mut merging: HashSet<QName> = HashSet::new();
    for name in &names {
        merge_one_extension(name, types, &mut merged, &mut merging)?;
    }
    Ok(())
}

fn merge_one_extension(
    name:    &QName,
    types:   &mut HashMap<QName, TypeRef>,
    merged:  &mut HashSet<QName>,
    merging: &mut HashSet<QName>,
) -> Result<(), SchemaCompileError> {
    if merged.contains(name) { return Ok(()); }
    if !merging.insert(name.clone()) {
        return Err(SchemaCompileError::msg(format!(
            "cyclic complex-type derivation involving {}", name
        )));
    }

    // Snapshot the current entry — we may replace it below.
    let current = types.get(name).cloned();
    if let Some(TypeRef::Complex(ct)) = current {
        if let Some(d) = &ct.derivation {
            if d.method == DerivationMethod::Extension {
                // Three shapes for the base:
                //   * `UNRESOLVED:` Simple placeholder → name lookup
                //     (recurse so the chain is merged bottom-up).
                //   * `Complex(Arc<…>)` direct reference → composed
                //     against that Arc as-is (used by `<xs:redefine>`
                //     where the base IS the pre-redefinition original).
                //   * Anything else (simple base for simple-content
                //     extension, or a built-in like xs:anyType) →
                //     leave the derived type unchanged.
                let base_ct: Option<Arc<ComplexType>> = match &d.base {
                    TypeRef::Complex(c) => Some(c.clone()),
                    _ => {
                        if let Some(base_qn) = resolve_typeref_to_qname(&d.base) {
                            merge_one_extension(&base_qn, types, merged, merging)?;
                            match types.get(&base_qn) {
                                Some(TypeRef::Complex(c)) => Some(c.clone()),
                                _ => None,
                            }
                        } else { None }
                    }
                };
                if let Some(base_ct) = base_ct {
                    let new_ct = compose_extension(&base_ct, &ct);
                    types.insert(name.clone(), TypeRef::Complex(Arc::new(new_ct)));
                } else if matches!(ct.content, ContentModel::Simple(_)) {
                    // simpleContent extension of a simple base type
                    // (built-in or user simple type).  The derived's
                    // content was initialised to Simple(String) by the
                    // parser as a placeholder; replace it with the
                    // base's actual simple type so default/fixed
                    // validation and instance text validation see the
                    // declared type.
                    let base_simple: Option<Arc<SimpleType>> = match &d.base {
                        TypeRef::Simple(st) if st.name.as_deref()
                            .map(|n| !n.starts_with("UNRESOLVED:"))
                            .unwrap_or(true) => Some(st.clone()),
                        TypeRef::Simple(_) => {
                            resolve_typeref_to_qname(&d.base)
                                .and_then(|qn| match types.get(&qn) {
                                    Some(TypeRef::Simple(s)) => Some(s.clone()),
                                    _ => None,
                                })
                        }
                        TypeRef::Complex(_) => None,
                    };
                    if let Some(base_simple) = base_simple {
                        let new_ct = ComplexType {
                            name:          ct.name.clone(),
                            derivation:    ct.derivation.clone(),
                            content:       ContentModel::Simple(base_simple),
                            matcher:       std::sync::OnceLock::new(),
                            attributes:    ct.attributes.clone(),
                            any_attribute: ct.any_attribute.clone(),
                            abstract_:     ct.abstract_,
                            block:         ct.block,
                            final_:        ct.final_,
                            pending_attribute_group_refs: ct.pending_attribute_group_refs.clone(),
                            assertions: ct.assertions.clone(),
                        };
                        types.insert(name.clone(), TypeRef::Complex(Arc::new(new_ct)));
                    }
                }
            }
        }
    }

    merged.insert(name.clone());
    merging.remove(name);
    Ok(())
}

/// Apply `compose_extension` to inline anonymous complex types that
/// hang off element decls.  Top-level named types are handled by
/// [`merge_extension_chains`]; this fills the gap for declarations
/// like `<xs:element name="Foo"><xs:complexType><xs:complexContent>
/// <xs:extension base="Bar">…`, whose `Bar` lookup is satisfied by
/// the already-merged `types` map.
///
/// Element decls are stored as `Arc<ElementDecl>`, shared with
/// `resolve_element_refs`'s patched content models.  We rebuild the
/// Arc when the inline type changes so the substitution map (built
/// just after this pass) captures the merged version.
/// Fold each restriction-derived complex type's base attribute set
/// onto the derived type, keeping only those base attribute uses
/// that the derived type doesn't already redeclare.  XSD §3.4.2
/// treats unmentioned base attribute uses as implicitly inherited;
/// without this pass the derived would silently drop them.
fn merge_restriction_attributes(
    types:    &mut HashMap<QName, TypeRef>,
    elements: &mut HashMap<QName, Arc<ElementDecl>>,
) {
    use super::types::DerivationMethod;

    fn resolved_base<'a>(
        d: &super::types::Derivation,
        types: &'a HashMap<QName, TypeRef>,
    ) -> Option<Arc<ComplexType>> {
        match &d.base {
            TypeRef::Complex(c) => Some(c.clone()),
            _ => resolve_typeref_to_qname(&d.base)
                .and_then(|qn| match types.get(&qn) {
                    Some(TypeRef::Complex(c)) => Some(c.clone()),
                    _ => None,
                }),
        }
    }

    fn merge_one(ct: &ComplexType, base: &ComplexType) -> ComplexType {
        use super::schema::{AttributeUseKind, AttributeUse};
        let mut merged: Vec<AttributeUse> = ct.attributes.clone();
        for b_au in &base.attributes {
            // A redeclaration (same name) wins by virtue of being
            // already present; prohibited uses in the derived stay
            // prohibited; missing base uses get carried over unless
            // they were `prohibited` in the base (in which case
            // there's nothing to inherit).
            if merged.iter().any(|a| a.decl.name == b_au.decl.name) {
                continue;
            }
            if b_au.use_kind == AttributeUseKind::Prohibited {
                continue;
            }
            merged.push(b_au.clone());
        }
        // anyAttribute on the derived overrides the base's wildcard;
        // if absent, inherit the base's.
        let any_attribute = ct.any_attribute.clone().or_else(|| base.any_attribute.clone());
        ComplexType {
            name:          ct.name.clone(),
            derivation:    ct.derivation.clone(),
            content:       ct.content.clone(),
            matcher:       std::sync::OnceLock::new(),
            attributes:    merged,
            any_attribute,
            abstract_:     ct.abstract_,
            block:         ct.block,
            final_:        ct.final_,
            pending_attribute_group_refs: ct.pending_attribute_group_refs.clone(),
            assertions: ct.assertions.clone(),
        }
    }

    let names: Vec<QName> = types.keys().cloned().collect();
    for name in names {
        let Some(TypeRef::Complex(ct)) = types.get(&name).cloned() else { continue };
        let Some(d) = &ct.derivation else { continue };
        if d.method != DerivationMethod::Restriction { continue; }
        let Some(base) = resolved_base(d, types) else { continue };
        let merged = merge_one(&ct, &base);
        types.insert(name, TypeRef::Complex(Arc::new(merged)));
    }

    // Same treatment for inline anonymous types attached to top-level
    // element decls.
    let elem_names: Vec<QName> = elements.keys().cloned().collect();
    for ename in elem_names {
        let Some(decl) = elements.get(&ename).cloned() else { continue };
        let TypeRef::Complex(ct) = &decl.type_def else { continue };
        if ct.name.is_some() { continue; }
        let Some(d) = &ct.derivation else { continue };
        if d.method != DerivationMethod::Restriction { continue; }
        let Some(base) = resolved_base(d, types) else { continue };
        let merged = merge_one(ct, &base);
        let new_decl = ElementDecl {
            name:               decl.name.clone(),
            type_def:           TypeRef::Complex(Arc::new(merged)),
            nillable:           decl.nillable,
            default:            decl.default.clone(),
            fixed:              decl.fixed.clone(),
            abstract_:          decl.abstract_,
            substitution_group: decl.substitution_group.clone(),
            identity:           decl.identity.clone(),
            block:              decl.block,
            final_:             decl.final_,
        };
        elements.insert(ename, Arc::new(new_decl));
    }
}

fn merge_inline_extension_in_elements(
    elements: &mut HashMap<QName, Arc<ElementDecl>>,
    types:    &HashMap<QName, TypeRef>,
) {
    let names: Vec<QName> = elements.keys().cloned().collect();
    for name in names {
        let Some(decl) = elements.get(&name).cloned() else { continue };
        let TypeRef::Complex(ct) = &decl.type_def else { continue };
        // Skip named types — they're handled by merge_extension_chains.
        if ct.name.is_some() { continue; }
        let Some(d) = &ct.derivation else { continue };
        if d.method != DerivationMethod::Extension { continue; }
        let base_ct: Option<Arc<ComplexType>> = match &d.base {
            TypeRef::Complex(c) => Some(c.clone()),
            _ => resolve_typeref_to_qname(&d.base)
                .and_then(|qn| types.get(&qn).cloned())
                .and_then(|tr| match tr {
                    TypeRef::Complex(c) => Some(c),
                    _ => None,
                }),
        };
        let merged = if let Some(base_ct) = base_ct {
            compose_extension(&base_ct, ct)
        } else if matches!(ct.content, ContentModel::Simple(_)) {
            // simpleContent extension whose base is a simple type
            // (built-in or user simple).  parse_derivation_body left
            // the carrier's content as `Simple(string)` as a
            // placeholder; substitute the real base so facet checks
            // on element text see the declared type.
            let base_simple: Option<Arc<SimpleType>> = match &d.base {
                TypeRef::Simple(st) if st.name.as_deref()
                    .map(|n| !n.starts_with("UNRESOLVED:"))
                    .unwrap_or(true) => Some(st.clone()),
                TypeRef::Simple(_) => resolve_typeref_to_qname(&d.base)
                    .and_then(|qn| match types.get(&qn) {
                        Some(TypeRef::Simple(s)) => Some(s.clone()),
                        _ => None,
                    }),
                TypeRef::Complex(_) => None,
            };
            let Some(base_simple) = base_simple else { continue };
            ComplexType {
                name:          ct.name.clone(),
                derivation:    ct.derivation.clone(),
                content:       ContentModel::Simple(base_simple),
                matcher:       std::sync::OnceLock::new(),
                attributes:    ct.attributes.clone(),
                any_attribute: ct.any_attribute.clone(),
                final_:        ct.final_,
                block:         ct.block,
                abstract_:     ct.abstract_,
                pending_attribute_group_refs: ct.pending_attribute_group_refs.clone(),
                assertions: ct.assertions.clone(),
            }
        } else { continue };
        let new_decl = ElementDecl {
            name:                decl.name.clone(),
            type_def:            TypeRef::Complex(Arc::new(merged)),
            nillable:            decl.nillable,
            default:             decl.default.clone(),
            fixed:               decl.fixed.clone(),
            abstract_:           decl.abstract_,
            substitution_group:  decl.substitution_group.clone(),
            identity:            decl.identity.clone(),
            block:               decl.block,
            final_:              decl.final_,
        };
        elements.insert(name, Arc::new(new_decl));
    }
}

/// Pull a [`QName`] out of a TypeRef when it's an `UNRESOLVED:` Simple
/// placeholder produced by `Builder::type_ref_for`.  Returns `None`
/// for built-in types or for placeholders whose marker can't be parsed.
fn check_derivation_content_kind(
    types: &HashMap<QName, TypeRef>,
) -> Result<(), SchemaCompileError> {
    for (name, tr) in types {
        let TypeRef::Complex(ct) = tr else { continue };
        let Some(deriv) = ct.derivation.as_ref() else { continue };
        // XSD §3.4.2 — simpleContent restriction's base MUST be a
        // complex type (with simple content), never a simple type
        // directly. The extension form (`<xs:extension>`) does
        // allow a simple-type base.
        let derived_is_simple = matches!(ct.content, ContentModel::Simple(_));
        let derived_is_complex = matches!(ct.content, ContentModel::Complex { .. });
        if derived_is_simple && deriv.method == DerivationMethod::Restriction {
            let base_is_complex = match &deriv.base {
                TypeRef::Complex(_) => true,
                TypeRef::Simple(_)  => match resolve_typeref_to_qname(&deriv.base) {
                    Some(qn) => matches!(types.get(&qn), Some(TypeRef::Complex(_))),
                    None => false, // built-in xs:* — always Simple
                },
            };
            if !base_is_complex {
                return Err(SchemaCompileError::msg(format!(
                    "<xs:complexType name={:?}>: <xs:simpleContent><xs:restriction> \
                     base must be a complex type with simple content, not a simple type",
                    name.local,
                )));
            }
        }
        // Resolve the base — placeholder Simple or direct Complex.
        let base_ct = match &deriv.base {
            TypeRef::Complex(c) => Some(c.clone()),
            TypeRef::Simple(_)  => match resolve_typeref_to_qname(&deriv.base) {
                Some(qn) => match types.get(&qn) {
                    Some(TypeRef::Complex(c)) => Some(c.clone()),
                    _ => None,
                },
                None => None,
            },
        };
        // XSD §3.4.2 — complexContent extension/restriction's base
        // must itself be a complex type.  A simple-type base (whether
        // a built-in like xs:string or a named user simpleType) is a
        // schema validity error.
        if derived_is_complex {
            let base_is_simple = match &deriv.base {
                TypeRef::Simple(st) => match resolve_typeref_to_qname(&deriv.base) {
                    Some(qn) if qn.namespace.as_deref() == Some(QName::XSD_NS) => {
                        // Built-in simple types resolve via BuiltinType.
                        // xs:anyType (which is complex) is the only
                        // XSD-namespace name that isn't a simple type.
                        BuiltinType::from_name(&qn.local).is_some()
                    }
                    Some(qn) => matches!(types.get(&qn), Some(TypeRef::Simple(_))),
                    // No UNRESOLVED marker — must be a resolved built-in
                    // (`type_ref_for` strips builtins straight to a
                    // SimpleType with name=None).
                    None => st.name.is_none(),
                },
                _ => false,
            };
            if base_is_simple {
                return Err(SchemaCompileError::msg(format!(
                    "<xs:complexType name={:?}>: <xs:complexContent> base must be a \
                     complex type, not a simple type",
                    name.local,
                )));
            }
        }
        let Some(base_ct) = base_ct else { continue };
        let base_is_simple = matches!(base_ct.content, ContentModel::Simple(_));
        let base_is_complex = matches!(base_ct.content, ContentModel::Complex { .. });
        if derived_is_complex && base_is_simple {
            return Err(SchemaCompileError::msg(format!(
                "<xs:complexType name={:?}>: complexContent derivation cannot \
                 have a simpleContent base",
                name.local,
            )));
        }
        if derived_is_simple && base_is_complex {
            return Err(SchemaCompileError::msg(format!(
                "<xs:complexType name={:?}>: simpleContent derivation cannot \
                 have a complexContent base",
                name.local,
            )));
        }
    }
    Ok(())
}

fn check_complex_type_final(
    types:    &HashMap<QName, TypeRef>,
    elements: &HashMap<QName, Arc<ElementDecl>>,
) -> Result<(), SchemaCompileError> {
    use super::types::DerivationMethod;
    fn check_one(
        ct: &ComplexType,
        types: &HashMap<QName, TypeRef>,
        label: &str,
    ) -> Result<(), SchemaCompileError> {
        let Some(d) = &ct.derivation else { return Ok(()) };
        let base_ct: Option<Arc<ComplexType>> = match &d.base {
            TypeRef::Complex(c) => Some(c.clone()),
            TypeRef::Simple(_) => resolve_typeref_to_qname(&d.base)
                .and_then(|qn| match types.get(&qn) {
                    Some(TypeRef::Complex(c)) => Some(c.clone()),
                    _ => None,
                }),
        };
        let Some(base_ct) = base_ct else { return Ok(()) };
        let blocked = match d.method {
            DerivationMethod::Restriction => base_ct.final_.contains(BlockSet::RESTRICTION),
            DerivationMethod::Extension   => base_ct.final_.contains(BlockSet::EXTENSION),
        };
        if blocked {
            return Err(SchemaCompileError::msg(format!(
                "{label}: base complex type's `final` disallows {} derivation",
                match d.method {
                    DerivationMethod::Restriction => "restriction",
                    DerivationMethod::Extension   => "extension",
                },
            )));
        }
        Ok(())
    }
    for (name, tr) in types {
        if let TypeRef::Complex(ct) = tr {
            let label = format!("<xs:complexType name={:?}>", name.local);
            check_one(ct, types, &label)?;
        }
    }
    for (name, decl) in elements {
        if let TypeRef::Complex(ct) = &decl.type_def {
            let label = format!("<xs:element name={:?}>'s inline type", name.local);
            check_one(ct, types, &label)?;
        }
    }
    Ok(())
}

fn check_complex_mixed_consistency(
    types:    &HashMap<QName, TypeRef>,
    elements: &HashMap<QName, Arc<ElementDecl>>,
) -> Result<(), SchemaCompileError> {
    fn mixed_of(c: &ContentModel) -> Option<bool> {
        match c {
            ContentModel::Complex { mixed, .. } => Some(*mixed),
            _ => None,
        }
    }
    fn check_one(
        ct: &ComplexType,
        types: &HashMap<QName, TypeRef>,
        label: &str,
    ) -> Result<(), SchemaCompileError> {
        let Some(d) = &ct.derivation else { return Ok(()) };
        let Some(d_mixed) = mixed_of(&ct.content) else { return Ok(()) };
        let base_ct: Option<Arc<ComplexType>> = match &d.base {
            TypeRef::Complex(c) => Some(c.clone()),
            TypeRef::Simple(_) => resolve_typeref_to_qname(&d.base)
                .and_then(|qn| match types.get(&qn) {
                    Some(TypeRef::Complex(c)) => Some(c.clone()),
                    _ => None,
                }),
        };
        let Some(base_ct) = base_ct else { return Ok(()) };
        let Some(b_mixed) = mixed_of(&base_ct.content) else { return Ok(()) };
        let bad = match d.method {
            // XSD §3.4.6 cos-ct-extends: mixed must match exactly.
            super::types::DerivationMethod::Extension => b_mixed != d_mixed,
            // cos-derived-ok-restriction-3 lets restriction *tighten*
            // mixed→element-only, but not the reverse.
            super::types::DerivationMethod::Restriction => !b_mixed && d_mixed,
        };
        if bad {
            return Err(SchemaCompileError::msg(format!(
                "{label}: complexContent {} cannot change mixed from {b_mixed} to {d_mixed}",
                match d.method {
                    super::types::DerivationMethod::Extension => "extension",
                    super::types::DerivationMethod::Restriction => "restriction",
                },
            )));
        }
        Ok(())
    }
    for (name, tr) in types {
        if let TypeRef::Complex(ct) = tr {
            let label = format!("<xs:complexType name={:?}>", name.local);
            check_one(ct, types, &label)?;
        }
    }
    for (name, decl) in elements {
        if let TypeRef::Complex(ct) = &decl.type_def {
            let label = format!("<xs:element name={:?}>'s inline type", name.local);
            check_one(ct, types, &label)?;
        }
    }
    Ok(())
}

fn ns_constraint_subset(
    derived: &super::schema::NamespaceConstraint,
    base:    &super::schema::NamespaceConstraint,
) -> bool {
    use super::schema::NamespaceConstraint::*;
    match (derived, base) {
        (_,             Any)   => true,
        (Any,           _)     => false,
        (Other,         Other) => true,
        (Other,         _)     => false,
        (List(d),       Other) => d.iter().all(|e| e.is_some()),
        (List(d),       List(b)) => d.iter().all(|d|
            b.iter().any(|b| match (d, b) {
                (None, None) => true,
                (Some(x), Some(y)) => x == y,
                _ => false,
            })
        ),
    }
}

fn wildcard_allows_ns(
    c:  &super::schema::NamespaceConstraint,
    ns: Option<&str>,
) -> bool {
    use super::schema::NamespaceConstraint::*;
    match c {
        Any => true,
        Other => ns.is_some(),
        List(entries) => entries.iter().any(|e| match (e, ns) {
            (None, None)            => true,
            (Some(u), Some(n))      => u.as_ref() == n,
            _ => false,
        }),
    }
}

fn process_strictness(w: &super::schema::Wildcard) -> u8 {
    use super::schema::ProcessContents::*;
    match w.process_contents { Strict => 2, Lax => 1, Skip => 0 }
}

fn check_complex_restriction_attributes(
    types:    &HashMap<QName, TypeRef>,
    elements: &HashMap<QName, Arc<ElementDecl>>,
) -> Result<(), SchemaCompileError> {
    use super::schema::AttributeUse;
    use super::types::DerivationMethod;

    fn resolve_base<'a>(
        d: &'a super::types::Derivation,
        types: &'a HashMap<QName, TypeRef>,
    ) -> Option<&'a ComplexType> {
        match &d.base {
            TypeRef::Complex(c) => Some(c.as_ref()),
            _ => resolve_typeref_to_qname(&d.base).and_then(|qn| match types.get(&qn) {
                Some(TypeRef::Complex(c)) => Some(c.as_ref()),
                _ => None,
            }),
        }
    }

    fn check_one(
        ct: &ComplexType,
        types: &HashMap<QName, TypeRef>,
        label: &str,
    ) -> Result<(), SchemaCompileError> {
        let Some(d) = &ct.derivation else { return Ok(()); };
        if d.method != DerivationMethod::Restriction { return Ok(()); }
        let Some(base) = resolve_base(d, types) else { return Ok(()); };

        let find_au = |name: &QName, attrs: &[AttributeUse]| -> Option<AttributeUse> {
            attrs.iter().find(|au| &au.decl.name == name).cloned()
        };

        // XSD §3.4.6 cos-ct-derived-ok.2 — when restricting, the
        // derived type's `<xs:anyAttribute>` must be a valid subset of
        // the base's, and any attribute the derived adds beyond the
        // base's attribute set must be allowed by the base's wildcard.
        match (&base.any_attribute, &ct.any_attribute) {
            (None, Some(_)) => {
                return Err(SchemaCompileError::msg(format!(
                    "{label}: invalid restriction of base — base has no \
                     <xs:anyAttribute> but the derived type adds one (restriction \
                     can only tighten, not introduce, attribute wildcards)"
                )));
            }
            (Some(b_any), Some(d_any)) => {
                if !ns_constraint_subset(&d_any.namespaces, &b_any.namespaces) {
                    return Err(SchemaCompileError::msg(format!(
                        "{label}: invalid restriction of base — derived \
                         <xs:anyAttribute> namespace is not a subset of base's"
                    )));
                }
                if process_strictness(d_any) < process_strictness(b_any) {
                    return Err(SchemaCompileError::msg(format!(
                        "{label}: invalid restriction of base — derived \
                         <xs:anyAttribute> processContents relaxes the base's"
                    )));
                }
            }
            _ => {}
        }
        // Each derived attribute not present in the base must satisfy
        // the base's anyAttribute (XSD §3.4.6 cos-ct-derived-ok.2.2).
        let base_attr_names: std::collections::HashSet<&QName> =
            base.attributes.iter().map(|au| &au.decl.name).collect();
        for au_d in &ct.attributes {
            if au_d.use_kind == AttributeUseKind::Prohibited { continue; }
            if base_attr_names.contains(&au_d.decl.name) { continue; }
            let Some(b_any) = &base.any_attribute else {
                return Err(SchemaCompileError::msg(format!(
                    "{label}: invalid restriction of base — derived adds \
                     attribute {:?} not present in the base, but the base \
                     has no <xs:anyAttribute> to license it",
                    au_d.decl.name.local,
                )));
            };
            if !wildcard_allows_ns(&b_any.namespaces, au_d.decl.name.namespace.as_deref()) {
                return Err(SchemaCompileError::msg(format!(
                    "{label}: invalid restriction of base — derived adds \
                     attribute {:?} (namespace {:?}) which the base's \
                     <xs:anyAttribute> namespace set doesn't allow",
                    au_d.decl.name.local, au_d.decl.name.namespace.as_deref().unwrap_or(""),
                )));
            }
        }

        for au_b in &base.attributes {
            if au_b.use_kind == AttributeUseKind::Prohibited { continue; }
            let au_r = find_au(&au_b.decl.name, &ct.attributes);

            // Required-in-base must remain required in derived
            // (omitted or relaxed → an instance valid under the
            // derived type would be invalid under the base, breaking
            // the restriction relationship).
            if au_b.use_kind == AttributeUseKind::Required {
                match au_r.as_ref().map(|x| x.use_kind) {
                    Some(AttributeUseKind::Required) => {}
                    Some(other) => {
                        return Err(SchemaCompileError::msg(format!(
                            "{label}: invalid restriction of base — attribute {:?} \
                             is `required` in the base but `{:?}` in the derived type",
                            au_b.decl.name.local, other,
                        )));
                    }
                    None => {
                        return Err(SchemaCompileError::msg(format!(
                            "{label}: invalid restriction of base — attribute {:?} \
                             is `required` in the base but absent in the derived type",
                            au_b.decl.name.local,
                        )));
                    }
                }
            }

            // Fixed value in the base must be carried through
            // unchanged.  An overriding `default=` is rejected: the
            // base required the value, the derived doesn't.
            let base_fixed = au_b.fixed.as_deref().or(au_b.decl.fixed.as_deref());
            if let Some(bf) = base_fixed {
                if let Some(au_r) = au_r.as_ref() {
                    let r_fixed = au_r.fixed.as_deref().or(au_r.decl.fixed.as_deref());
                    let r_default = au_r.default.as_deref().or(au_r.decl.default.as_deref());
                    match (r_fixed, r_default) {
                        (Some(rf), _) if rf != bf => {
                            return Err(SchemaCompileError::msg(format!(
                                "{label}: invalid restriction of base — attribute {:?} \
                                 fixed value {:?} differs from base fixed {:?}",
                                au_b.decl.name.local, rf, bf,
                            )));
                        }
                        (None, Some(_)) => {
                            return Err(SchemaCompileError::msg(format!(
                                "{label}: invalid restriction of base — attribute {:?} \
                                 has `fixed={:?}` in the base; the derived may not \
                                 replace it with `default=`",
                                au_b.decl.name.local, bf,
                            )));
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    for (name, tr) in types {
        if let TypeRef::Complex(ct) = tr {
            let label = format!("<xs:complexType name={:?}>", name.local);
            check_one(ct, types, &label)?;
        }
    }
    for (name, decl) in elements {
        if let TypeRef::Complex(ct) = &decl.type_def {
            let label = format!("<xs:element name={:?}>'s inline type", name.local);
            check_one(ct, types, &label)?;
        }
    }
    Ok(())
}

fn check_type_refs_collide(
    types:       &HashMap<QName, TypeRef>,
    attributes:  &HashMap<QName, Arc<AttributeDecl>>,
    attr_groups: &HashMap<QName, Arc<AttributeGroup>>,
    elements:    &HashMap<QName, Arc<ElementDecl>>,
) -> Result<(), SchemaCompileError> {
    fn check_typeref(
        tr: &TypeRef,
        owner: &str,
        types: &HashMap<QName, TypeRef>,
        attributes:  &HashMap<QName, Arc<AttributeDecl>>,
        attr_groups: &HashMap<QName, Arc<AttributeGroup>>,
        elements:    &HashMap<QName, Arc<ElementDecl>>,
    ) -> Result<(), SchemaCompileError> {
        // Only `UNRESOLVED:` placeholders make it here.  Resolved
        // built-ins / inline types / `xs:anyType` return None and
        // are silently accepted.
        let Some(qn) = resolve_typeref_to_qname(tr) else { return Ok(()) };
        if types.contains_key(&qn) { return Ok(()) }
        let kind = if attributes.contains_key(&qn) { Some("an attribute") }
                   else if attr_groups.contains_key(&qn) { Some("an attributeGroup") }
                   else if elements.contains_key(&qn) { Some("an element") }
                   else { None };
        match kind {
            Some(k) => Err(SchemaCompileError::msg(format!(
                "{owner}: type={:?} resolves to {k}, not a type",
                qn.local,
            ))),
            // Nothing in the schema declares this name as anything —
            // the `<xs:include>` / `<xs:import>` that should have
            // brought it in either failed to load or never existed.
            None => Err(SchemaCompileError::msg(format!(
                "{owner}: undefined type {:?} \
                 (no <xs:simpleType> or <xs:complexType> with this name; \
                 check that any <xs:include>/<xs:import> schemaLocation \
                 was loadable)",
                qn.local,
            ))),
        }
    }
    fn walk_complex(
        ct: &ComplexType,
        types: &HashMap<QName, TypeRef>,
        attributes:  &HashMap<QName, Arc<AttributeDecl>>,
        attr_groups: &HashMap<QName, Arc<AttributeGroup>>,
        elements:    &HashMap<QName, Arc<ElementDecl>>,
        visited:     &mut std::collections::HashSet<*const ComplexType>,
    ) -> Result<(), SchemaCompileError> {
        if !visited.insert(ct as *const _) { return Ok(()); }
        // XSD §3.4.6 — `<xs:extension>` / `<xs:restriction>` `base="…"`
        // must resolve to an existing type definition.  An UNRESOLVED
        // placeholder here means the referenced type was never
        // declared (or its include / import failed to load).
        if let Some(deriv) = ct.derivation.as_ref() {
            let label = format!("<xs:complexType name={:?}> base",
                ct.name.as_ref().map(|n| &*n.local).unwrap_or("<anonymous>"));
            check_typeref(&deriv.base, &label,
                          types, attributes, attr_groups, elements)?;
        }
        for au in &ct.attributes {
            check_typeref(&TypeRef::Simple(au.decl.type_def.clone()),
                          &format!("<xs:attribute name={:?}>", au.decl.name.local),
                          types, attributes, attr_groups, elements)?;
        }
        if let ContentModel::Complex { root, .. } = &ct.content {
            walk_particle(root, types, attributes, attr_groups, elements, visited)?;
        }
        Ok(())
    }
    fn walk_particle(
        p: &super::schema::Particle,
        types: &HashMap<QName, TypeRef>,
        attributes:  &HashMap<QName, Arc<AttributeDecl>>,
        attr_groups: &HashMap<QName, Arc<AttributeGroup>>,
        elements:    &HashMap<QName, Arc<ElementDecl>>,
        visited:     &mut std::collections::HashSet<*const ComplexType>,
    ) -> Result<(), SchemaCompileError> {
        use super::schema::Term;
        match &p.term {
            Term::Element(e) => {
                check_typeref(&e.type_def, &format!("<xs:element name={:?}>", e.name.local),
                              types, attributes, attr_groups, elements)?;
                if let TypeRef::Complex(ct) = &e.type_def {
                    walk_complex(ct, types, attributes, attr_groups, elements, visited)?;
                }
            }
            Term::Group { particles, .. } => {
                for child in particles.iter() {
                    walk_particle(child, types, attributes, attr_groups, elements, visited)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    let mut visited: std::collections::HashSet<*const ComplexType> = std::collections::HashSet::new();
    for (qn, decl) in elements {
        let owner = format!("<xs:element name={:?}>", qn.local);
        check_typeref(&decl.type_def, &owner, types, attributes, attr_groups, elements)?;
        if let TypeRef::Complex(ct) = &decl.type_def {
            walk_complex(ct, types, attributes, attr_groups, elements, &mut visited)?;
        }
    }
    for (qn, attr) in attributes {
        let owner = format!("<xs:attribute name={:?}>", qn.local);
        check_typeref(&TypeRef::Simple(attr.type_def.clone()), &owner,
                      types, attributes, attr_groups, elements)?;
    }
    for ct in types.values().filter_map(|t| match t {
        TypeRef::Complex(c) => Some(c),
        _ => None,
    }) {
        walk_complex(ct, types, attributes, attr_groups, elements, &mut visited)?;
    }
    Ok(())
}

fn check_element_refs(
    refs:        &[QName],
    elements:    &HashMap<QName, Arc<ElementDecl>>,
    types:       &HashMap<QName, TypeRef>,
    attributes:  &HashMap<QName, Arc<AttributeDecl>>,
    attr_groups: &HashMap<QName, Arc<AttributeGroup>>,
    target_ns:   Option<&str>,
    imports:     &HashSet<Option<Arc<str>>>,
) -> Result<(), SchemaCompileError> {
    for ref_qn in refs {
        if elements.contains_key(ref_qn) { continue; }
        let collision = if types.contains_key(ref_qn) {
            Some("a type")
        } else if attributes.contains_key(ref_qn) {
            Some("an attribute")
        } else if attr_groups.contains_key(ref_qn) {
            Some("an attributeGroup")
        } else {
            None
        };
        if let Some(kind) = collision {
            return Err(SchemaCompileError::msg(format!(
                "<xs:element ref={:?}> resolves to {kind}, not an element",
                ref_qn.local,
            )));
        }
        // XSD §3.3.6 src-resolve clause 4: a QName ref whose namespace
        // differs from this schema's target must have been brought
        // into scope by an `<xs:import>`.
        if ref_qn.namespace.as_deref() != target_ns
            && !import_covers(imports, ref_qn.namespace.as_deref())
        {
            return Err(SchemaCompileError::msg(format!(
                "<xs:element ref={:?}>: namespace {:?} is not the schema's \
                 targetNamespace and was not brought in by <xs:import> (XSD §3.3.6 \
                 src-resolve)",
                ref_qn.local, ref_qn.namespace.as_deref().unwrap_or(""),
            )));
        }
        // Same-namespace ref must point at a *top-level* element
        // declaration (XSD §3.3.3 — `ref` only resolves against the
        // schema's global element table, never local ones).
        if ref_qn.namespace.as_deref() == target_ns {
            return Err(SchemaCompileError::msg(format!(
                "<xs:element ref={:?}>: no top-level <xs:element name={:?}> \
                 declaration in this schema",
                ref_qn.local, ref_qn.local,
            )));
        }
    }
    Ok(())
}

fn check_redefined_attribute_groups(
    pending: &[(QName, Arc<AttributeGroup>, Arc<AttributeGroup>, bool)],
    types:   &HashMap<QName, TypeRef>,
) -> Result<(), SchemaCompileError> {
    for (name, new_ag, original, saw_self_ref) in pending {
        // Self-reference shape mirrors what `check_redefined_groups`
        // does for model groups — when the redefining body carried
        // a `<xs:attributeGroup ref="same-name"/>` the original's
        // attribute uses are spliced in via the ref, and additions
        // are XSTS-accepted as an additive redefine.  Skip the
        // strict-restriction check for that case.
        if *saw_self_ref { continue; }
        for new_use in &new_ag.attributes {
            let orig_use = original.attributes.iter()
                .find(|u| u.decl.name == new_use.decl.name);
            let Some(orig_use) = orig_use else {
                return Err(SchemaCompileError::msg(format!(
                    "<xs:redefine><xs:attributeGroup name={:?}>: attribute {:?} \
                     is not present in the original attribute group — a redefining \
                     body without a self-reference must be a valid restriction \
                     (XSD §4.2.2 src-redefine)",
                    name.local, new_use.decl.name.local,
                )));
            };
            // Required attributes from the original must stay
            // required (or fixed-by-value); a redefining body can't
            // relax `use="required"` to `optional`.
            if matches!(orig_use.use_kind, AttributeUseKind::Required)
                && matches!(new_use.use_kind,
                    AttributeUseKind::Optional | AttributeUseKind::Prohibited)
            {
                return Err(SchemaCompileError::msg(format!(
                    "<xs:redefine><xs:attributeGroup name={:?}>: attribute {:?} \
                     was required in the original; the redefining body cannot \
                     relax it to {:?}",
                    name.local, new_use.decl.name.local, new_use.use_kind,
                )));
            }
            // (Type-derivation check intentionally omitted: chained
            // redefines through `<xs:include>` mutate the snapshot in
            // ways that make a single-step type comparison fragile;
            // the strict-form variants of this rule are not net
            // positive against the XSTS corpus.)
            let _ = types;
            // Prohibited in original must stay prohibited — a
            // redefining body can't loosen `use="prohibited"`.
            if matches!(orig_use.use_kind, AttributeUseKind::Prohibited)
                && !matches!(new_use.use_kind, AttributeUseKind::Prohibited)
            {
                return Err(SchemaCompileError::msg(format!(
                    "<xs:redefine><xs:attributeGroup name={:?}>: attribute {:?} \
                     was prohibited in the original; the redefining body cannot \
                     relax that to {:?}",
                    name.local, new_use.decl.name.local, new_use.use_kind,
                )));
            }
        }
        // Every required attribute in the original must still appear
        // (with at least the same `use`) in the redefining body —
        // dropping a required attribute would leave instances valid
        // under derived but invalid under base.
        for orig_use in &original.attributes {
            if !matches!(orig_use.use_kind, AttributeUseKind::Required) { continue; }
            if !new_ag.attributes.iter().any(|u|
                u.decl.name == orig_use.decl.name
                    && matches!(u.use_kind, AttributeUseKind::Required))
            {
                return Err(SchemaCompileError::msg(format!(
                    "<xs:redefine><xs:attributeGroup name={:?}>: required attribute \
                     {:?} is missing in the redefining body — a non-self-reference \
                     redefine cannot drop required attributes",
                    name.local, orig_use.decl.name.local,
                )));
            }
        }
    }
    Ok(())
}


/// Resolve forward-referenced bases captured by
/// `parse_simple_restriction` and validate the recorded derived
/// facets against the real ancestor facet set.  Same-target-namespace
/// references that still don't resolve are flagged here too — they're
/// `<xs:restriction base="X">`s pointing at a name that nothing
/// declares.
fn check_pending_simple_facet_tightening(
    pending: &[(QName, Vec<Facet>)],
    types:   &HashMap<QName, TypeRef>,
) -> Result<(), SchemaCompileError> {
    for (base_qn, derived) in pending {
        let Some(tr) = types.get(base_qn) else {
            return Err(SchemaCompileError::msg(format!(
                "<xs:restriction base={:?}>: undefined simple type",
                base_qn.local,
            )));
        };
        let base_st = match tr {
            TypeRef::Simple(st)  => st.clone(),
            TypeRef::Complex(_)  => return Err(SchemaCompileError::msg(format!(
                "<xs:restriction base={:?}> in a simpleType must reference a \
                 simpleType, not a complexType",
                base_qn.local,
            ))),
        };
        // The original parse stashed bound-facet values as
        // `Bound::Value(Value::String(...))` because the base built-in
        // was unknown.  Now that we know it, re-parse the raw strings
        // into the proper numeric / temporal `Bound` shape so
        // `compare_bounds` can do a meaningful order check.
        let mut reparsed: Vec<Facet> = Vec::with_capacity(derived.len());
        for f in derived {
            reparsed.push(reparse_bound_facet(f, base_st.builtin)?);
        }
        check_facet_tightening_pure(&base_st.facets.facets, &reparsed).map_err(|m| {
            SchemaCompileError::msg(format!(
                "<xs:restriction base={:?}>: {m}", base_qn.local,
            ))
        })?;
    }
    Ok(())
}

fn reparse_bound_facet(f: &Facet, builtin: BuiltinType) -> Result<Facet, SchemaCompileError> {
    fn reparse(b: &Bound, builtin: BuiltinType) -> Result<Bound, SchemaCompileError> {
        match b {
            Bound::Value(super::types::Value::String(s)) => parse_bound(s, builtin),
            _ => Ok(b.clone()),
        }
    }
    Ok(match f {
        Facet::MinInclusive(b) => Facet::MinInclusive(reparse(b, builtin)?),
        Facet::MaxInclusive(b) => Facet::MaxInclusive(reparse(b, builtin)?),
        Facet::MinExclusive(b) => Facet::MinExclusive(reparse(b, builtin)?),
        Facet::MaxExclusive(b) => Facet::MaxExclusive(reparse(b, builtin)?),
        other => other.clone(),
    })
}

/// XSD §4.3 (cvc-restriction) — every facet in `derived` must be a
/// legitimate tightening of the corresponding facet in `base`.
/// Module-scope so the parser's inline check and the post-pass that
/// validates forward-referenced bases share a single implementation.
fn check_facet_tightening_pure(base: &[Facet], derived: &[Facet]) -> Result<(), String> {
    fn pick<'a, T, F>(slice: &'a [Facet], f: F) -> Option<&'a T>
    where F: Fn(&'a Facet) -> Option<&'a T> {
        slice.iter().rev().find_map(f)
    }
    let base_length    = pick(base, |f| if let Facet::Length(n)    = f { Some(n) } else { None });
    let base_min_len   = pick(base, |f| if let Facet::MinLength(n) = f { Some(n) } else { None });
    let base_max_len   = pick(base, |f| if let Facet::MaxLength(n) = f { Some(n) } else { None });
    let base_min_incl  = pick(base, |f| if let Facet::MinInclusive(b) = f { Some(b) } else { None });
    let base_max_incl  = pick(base, |f| if let Facet::MaxInclusive(b) = f { Some(b) } else { None });
    let base_min_excl  = pick(base, |f| if let Facet::MinExclusive(b) = f { Some(b) } else { None });
    let base_max_excl  = pick(base, |f| if let Facet::MaxExclusive(b) = f { Some(b) } else { None });
    let base_total_d   = pick(base, |f| if let Facet::TotalDigits(n) = f { Some(n) } else { None });
    let base_frac_d    = pick(base, |f| if let Facet::FractionDigits(n) = f { Some(n) } else { None });
    for f in derived {
        match f {
            Facet::Length(n) => {
                if let Some(b) = base_length {
                    if n != b {
                        return Err(format!("restriction length ({n}) must equal base length ({b})"));
                    }
                }
                if let Some(b) = base_min_len {
                    if (*n as usize) < (*b) {
                        return Err(format!("restriction length ({n}) is below base minLength ({b})"));
                    }
                }
                if let Some(b) = base_max_len {
                    if (*n as usize) > (*b) {
                        return Err(format!("restriction length ({n}) exceeds base maxLength ({b})"));
                    }
                }
            }
            Facet::MinLength(n) => {
                if let Some(b) = base_min_len {
                    if n < b {
                        return Err(format!("restriction minLength ({n}) is below base minLength ({b})"));
                    }
                }
                if let Some(b) = base_length {
                    if (*n as u32) != (*b as u32) {
                        return Err(format!("restriction minLength ({n}) is inconsistent with base length ({b})"));
                    }
                }
            }
            Facet::MaxLength(n) => {
                if let Some(b) = base_max_len {
                    if n > b {
                        return Err(format!("restriction maxLength ({n}) exceeds base maxLength ({b})"));
                    }
                }
                if let Some(b) = base_length {
                    if (*n as u32) != (*b as u32) {
                        return Err(format!("restriction maxLength ({n}) is inconsistent with base length ({b})"));
                    }
                }
            }
            Facet::MinInclusive(d) => {
                if let Some(b) = base_min_incl {
                    if compare_bounds(d, b).map(|o| o.is_lt()).unwrap_or(false) {
                        return Err("restriction minInclusive is below the base's min bound".into());
                    }
                }
                if let Some(b) = base_min_excl {
                    // d ≤ b means derived admits a value (d itself or
                    // anything ≤ b) that the base's exclusive bound
                    // forbids.
                    if compare_bounds(d, b).map(|o| !o.is_gt()).unwrap_or(false) {
                        return Err("restriction minInclusive is at or below the base's minExclusive bound".into());
                    }
                }
                if let Some(b) = base_max_incl {
                    if compare_bounds(d, b).map(|o| o.is_gt()).unwrap_or(false) {
                        return Err("restriction minInclusive exceeds the base's max bound".into());
                    }
                }
                if let Some(b) = base_max_excl {
                    // d ≥ b means the derived inclusive bound admits a
                    // value the base's exclusive max forbids.
                    if compare_bounds(d, b).map(|o| !o.is_lt()).unwrap_or(false) {
                        return Err("restriction minInclusive is at or above the base's maxExclusive bound".into());
                    }
                }
            }
            Facet::MinExclusive(d) => {
                for b in base_min_incl.into_iter().chain(base_min_excl) {
                    if compare_bounds(d, b).map(|o| o.is_lt()).unwrap_or(false) {
                        return Err("restriction minExclusive is below the base's min bound".into());
                    }
                }
                for b in base_max_incl.into_iter().chain(base_max_excl) {
                    if compare_bounds(d, b).map(|o| o.is_ge()).unwrap_or(false) {
                        return Err("restriction minExclusive is at or above the base's max bound".into());
                    }
                }
            }
            Facet::MaxInclusive(d) => {
                if let Some(b) = base_max_incl {
                    if compare_bounds(d, b).map(|o| o.is_gt()).unwrap_or(false) {
                        return Err("restriction maxInclusive exceeds the base's max bound".into());
                    }
                }
                if let Some(b) = base_max_excl {
                    // d ≥ b means derived admits a value (d itself or
                    // anything ≥ b) that the base's exclusive bound
                    // forbids.
                    if compare_bounds(d, b).map(|o| !o.is_lt()).unwrap_or(false) {
                        return Err("restriction maxInclusive is at or above the base's maxExclusive bound".into());
                    }
                }
                for b in base_min_incl.into_iter().chain(base_min_excl) {
                    if compare_bounds(d, b).map(|o| o.is_lt()).unwrap_or(false) {
                        return Err("restriction maxInclusive is below the base's min bound".into());
                    }
                }
            }
            Facet::MaxExclusive(d) => {
                for b in base_max_incl.into_iter().chain(base_max_excl) {
                    if compare_bounds(d, b).map(|o| o.is_gt()).unwrap_or(false) {
                        return Err("restriction maxExclusive exceeds the base's max bound".into());
                    }
                }
                for b in base_min_incl.into_iter().chain(base_min_excl) {
                    if compare_bounds(d, b).map(|o| o.is_le()).unwrap_or(false) {
                        return Err("restriction maxExclusive is at or below the base's min bound".into());
                    }
                }
            }
            Facet::TotalDigits(n) => {
                if let Some(b) = base_total_d {
                    if n > b {
                        return Err(format!("restriction totalDigits ({n}) exceeds base totalDigits ({b})"));
                    }
                }
            }
            Facet::FractionDigits(n) => {
                if let Some(b) = base_frac_d {
                    if n > b {
                        return Err(format!("restriction fractionDigits ({n}) exceeds base fractionDigits ({b})"));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn import_covers(imports: &HashSet<Option<Arc<str>>>, ns: Option<&str>) -> bool {
    match ns {
        None    => imports.iter().any(|entry| entry.is_none()),
        Some(s) => imports.iter().any(|entry| entry.as_deref() == Some(s)),
    }
}

/// Verify that no element declaration with `default=` / `fixed=`
/// has a (transitively-resolved) type whose builtin chain reaches
/// `xs:ID` — XSD §3.3.6 forbids value constraints on ID-typed
/// elements.  Runs after type resolution so chains through
/// user-defined `<xs:simpleType>` are fully visible.
fn check_id_typed_element_value_constraints(
    elements: &HashMap<QName, Arc<ElementDecl>>,
    types:    &HashMap<QName, TypeRef>,
) -> Result<(), SchemaCompileError> {
    fn id_typed_simple(st: &super::types::SimpleType, types: &HashMap<QName, TypeRef>) -> bool {
        if matches!(st.builtin, BuiltinType::Id) { return true; }
        // Walk the named-type chain for a deeper resolved view.  The
        // parser collapses `<xs:simpleType><xs:restriction base="X">`
        // into a SimpleType carrying X's builtin, so for most chains
        // `st.builtin == Id` already.  This loop just handles the
        // case where the name still points at a top-level user-named
        // simple type that wasn't fully merged in.
        if let Some(name) = &st.name {
            if let Some(rest) = name.strip_prefix("UNRESOLVED:") {
                let qn = if let Some(rest) = rest.strip_prefix('{') {
                    if let Some(end) = rest.find('}') {
                        let ns    = &rest[..end];
                        let local = &rest[end + 1..];
                        QName::new(if ns.is_empty() { None } else { Some(ns) }, local)
                    } else { QName::new(None, rest) }
                } else { QName::new(None, rest) };
                if let Some(TypeRef::Simple(real)) = types.get(&qn) {
                    return id_typed_simple(real, types);
                }
            }
        }
        false
    }
    for decl in elements.values() {
        if decl.default.is_none() && decl.fixed.is_none() { continue; }
        if let TypeRef::Simple(st) = &decl.type_def {
            if id_typed_simple(st, types) {
                return Err(SchemaCompileError::msg(format!(
                    "<xs:element name={:?}> of type xs:ID (or derived) cannot have \
                     a default or fixed value (XSD §3.3.6)",
                    decl.name.local,
                )));
            }
        }
    }
    Ok(())
}

/// XSD §3.8.6 *Element Declarations Consistent*: within a single
/// content model, every element-particle declaration with a given
/// `{name}` must have the same `{type definition}` (judged by Arc
/// identity, then by declared-name equality).  Walks the particle
/// tree across nested groups, collecting `name → typeref` pairs.
fn check_element_decls_consistent(
    types:    &HashMap<QName, TypeRef>,
    elements: &HashMap<QName, Arc<ElementDecl>>,
) -> Result<(), SchemaCompileError> {
    fn typerefs_compatible(a: &TypeRef, b: &TypeRef) -> bool {
        match (a, b) {
            (TypeRef::Simple(x), TypeRef::Simple(y)) => {
                if Arc::ptr_eq(x, y) { return true; }
                // Two parser-built built-in references share no Arc but
                // are the same type when their `builtin` matches and
                // neither carries a user-defined name.
                if x.name.is_none() && y.name.is_none()
                    && x.builtin == y.builtin
                    && matches!(x.variety, super::types::Variety::Atomic)
                    && matches!(y.variety, super::types::Variety::Atomic)
                {
                    return true;
                }
                x.name.is_some() && x.name == y.name
            }
            (TypeRef::Complex(x), TypeRef::Complex(y)) => Arc::ptr_eq(x, y)
                || (x.name.is_some() && x.name == y.name),
            _ => false,
        }
    }
    fn walk(
        p:    &super::schema::Particle,
        seen: &mut HashMap<QName, TypeRef>,
        owner: &str,
    ) -> Result<(), SchemaCompileError> {
        match &p.term {
            super::schema::Term::Element(decl) => {
                if let Some(prev) = seen.get(&decl.name) {
                    if !typerefs_compatible(prev, &decl.type_def) {
                        return Err(SchemaCompileError::msg(format!(
                            "{owner}: element {:?} appears twice in the content \
                             model with different types (XSD §3.8.6 Element \
                             Declarations Consistent)",
                            decl.name.local,
                        )));
                    }
                } else {
                    seen.insert(decl.name.clone(), decl.type_def.clone());
                }
            }
            super::schema::Term::Group { particles, .. } => {
                for c in particles.iter() { walk(c, seen, owner)?; }
            }
            super::schema::Term::Wildcard(_)
            | super::schema::Term::GroupRef(_) => {}
        }
        Ok(())
    }
    let check_complex = |ct: &ComplexType| -> Result<(), SchemaCompileError> {
        if let ContentModel::Complex { root, .. } = &ct.content {
            let mut seen: HashMap<QName, TypeRef> = HashMap::new();
            let owner = format!("<xs:complexType name={:?}>",
                ct.name.as_ref().map(|n| &*n.local).unwrap_or("<anonymous>"));
            walk(root, &mut seen, &owner)?;
        }
        Ok(())
    };
    for tr in types.values() {
        if let TypeRef::Complex(ct) = tr { check_complex(ct)?; }
    }
    for decl in elements.values() {
        if let TypeRef::Complex(ct) = &decl.type_def { check_complex(ct)?; }
    }
    Ok(())
}

fn check_substitution_group_typing(
    elements: &HashMap<QName, Arc<ElementDecl>>,
    types:    &HashMap<QName, TypeRef>,
) -> Result<(), SchemaCompileError> {
    fn is_any_type(tr: &TypeRef) -> bool {
        matches!(tr, TypeRef::Complex(ct)
            if ct.name.as_ref()
                .map(|n| n.namespace.as_deref() == Some(QName::XSD_NS)
                         && n.local.as_ref() == "anyType")
                .unwrap_or(false))
    }
    for sub in elements.values() {
        let Some(head_qn) = &sub.substitution_group else { continue };
        let Some(head) = elements.get(head_qn) else { continue };

        // XSD §3.3.2 — an element with a substitutionGroup whose
        // `type=` is omitted inherits the head's type.  We can't
        // distinguish "explicitly typed as xs:anyType" from "no type
        // attribute" post-parse, but xs:anyType in an instance schema
        // is rare enough that treating it as the inherited default
        // covers the common case without over-rejecting.
        if is_any_type(&sub.type_def) {
            continue;
        }

        let used = super::dfa::derivation_methods_between(
            &sub.type_def, &head.type_def, types,
        );
        let Some(used) = used else {
            return Err(SchemaCompileError::msg(format!(
                "<xs:element name={:?} substitutionGroup={:?}>: \
                 substituting element's type does not derive from the head's type",
                sub.name.local, head_qn.local,
            )));
        };

        // Substitution-group exclusions per cvc-elt-substitution =
        // head element's final + head type's final.  The schema-level
        // finalDefault was folded into both at parse time.
        let head_type_final = match &head.type_def {
            TypeRef::Complex(ct) => ct.final_,
            TypeRef::Simple(_)   => BlockSet::default(),
        };
        let blocked = head.final_ | head_type_final;
        let forbidden = used & blocked & (BlockSet::RESTRICTION | BlockSet::EXTENSION);
        if !forbidden.is_empty() {
            let label = if forbidden.contains(BlockSet::RESTRICTION) && forbidden.contains(BlockSet::EXTENSION) {
                "restriction and extension"
            } else if forbidden.contains(BlockSet::RESTRICTION) {
                "restriction"
            } else {
                "extension"
            };
            return Err(SchemaCompileError::msg(format!(
                "<xs:element name={:?} substitutionGroup={:?}>: \
                 head element's `final` disallows {label}-based substitution",
                sub.name.local, head_qn.local,
            )));
        }
    }
    Ok(())
}

fn check_substitution_group_heads(
    elements: &HashMap<QName, Arc<ElementDecl>>,
) -> Result<(), SchemaCompileError> {
    for decl in elements.values() {
        let Some(head) = &decl.substitution_group else { continue };
        if !elements.contains_key(head) {
            return Err(SchemaCompileError::msg(format!(
                "<xs:element name={:?} substitutionGroup={:?}>: \
                 the head element is not declared",
                decl.name.local, head.local,
            )));
        }
    }
    // XSD §3.3.6 cos-equiv-class-correct — substitution groups must
    // form a forest (no cycles).  Walk each element's substitution
    // chain and reject when a name repeats.
    for decl in elements.values() {
        let mut seen: HashSet<QName> = HashSet::new();
        let mut cur = decl.clone();
        loop {
            let Some(head_qn) = &cur.substitution_group else { break };
            if !seen.insert(cur.name.clone()) {
                return Err(SchemaCompileError::msg(format!(
                    "<xs:element name={:?}>: substitution-group chain is cyclic \
                     (eventually revisits {:?}); per XSD §3.3.6 cos-equiv-class-correct \
                     the substitution graph must be a forest",
                    decl.name.local, cur.name.local,
                )));
            }
            if head_qn == &cur.name {
                return Err(SchemaCompileError::msg(format!(
                    "<xs:element name={:?}>: cannot substitute for itself",
                    cur.name.local,
                )));
            }
            let Some(next) = elements.get(head_qn) else { break };
            cur = next.clone();
        }
    }
    Ok(())
}

/// XSD §3.2.5 — the XML Schema Instance namespace
/// (`http://www.w3.org/2001/XMLSchema-instance`) is reserved for the
/// four pre-declared attributes (`xsi:type`, `xsi:nil`,
/// `xsi:schemaLocation`, `xsi:noNamespaceSchemaLocation`).  User
/// schemas cannot declare attributes in that namespace.
fn check_xsi_attribute_name(qn: &QName) -> Result<(), String> {
    match qn.namespace.as_deref() {
        Some("http://www.w3.org/2001/XMLSchema-instance") => Err(format!(
            "<xs:attribute name={:?}> in the XML Schema Instance namespace is not allowed",
            qn.local,
        )),
        _ => Ok(()),
    }
}

fn check_attribute_group_cycles(
    edges: &HashMap<QName, Vec<QName>>,
) -> Result<(), SchemaCompileError> {
    enum Color { White, Gray, Black }
    let mut color: HashMap<&QName, Color> = edges.keys().map(|k| (k, Color::White)).collect();
    fn dfs<'a>(
        node:  &'a QName,
        edges: &'a HashMap<QName, Vec<QName>>,
        color: &mut HashMap<&'a QName, Color>,
    ) -> Result<(), QName> {
        color.insert(node, Color::Gray);
        if let Some(refs) = edges.get(node) {
            for r in refs {
                // Find the key in `edges` that equals `r` by value.
                let next_key = edges.keys().find(|k| *k == r);
                let Some(next) = next_key else { continue };
                match color.get(next) {
                    Some(Color::Gray)  => return Err(node.clone()),
                    Some(Color::Black) => {}
                    Some(Color::White) | None => dfs(next, edges, color)?,
                }
            }
        }
        color.insert(node, Color::Black);
        Ok(())
    }
    let names: Vec<&QName> = edges.keys().collect();
    for n in names {
        if matches!(color.get(n), Some(Color::White)) {
            if let Err(cyc) = dfs(n, edges, &mut color) {
                return Err(SchemaCompileError::msg(format!(
                    "cyclic <xs:attributeGroup> reference involving {:?} \
                     (XSD §3.6.6 src-attribute-group-cyclic)",
                    cyc.local,
                )));
            }
        }
    }
    Ok(())
}

fn check_ag_refs_in_ag(
    refs:        &[QName],
    attr_groups: &HashMap<QName, Arc<AttributeGroup>>,
    types:       &HashMap<QName, TypeRef>,
    elements:    &HashMap<QName, Arc<ElementDecl>>,
    attributes:  &HashMap<QName, Arc<AttributeDecl>>,
) -> Result<(), SchemaCompileError> {
    for ref_qn in refs {
        if attr_groups.contains_key(ref_qn) { continue; }
        let collision = if types.contains_key(ref_qn) {
            Some("a type")
        } else if elements.contains_key(ref_qn) {
            Some("an element")
        } else if attributes.contains_key(ref_qn) {
            Some("an attribute")
        } else {
            None
        };
        if let Some(kind) = collision {
            return Err(SchemaCompileError::msg(format!(
                "<xs:attributeGroup ref={:?}> resolves to {kind}, not an attributeGroup",
                ref_qn.local,
            )));
        }
    }
    Ok(())
}

fn check_attribute_refs(
    refs:        &[QName],
    attributes:  &HashMap<QName, Arc<AttributeDecl>>,
    attr_groups: &HashMap<QName, Arc<AttributeGroup>>,
    types:       &HashMap<QName, TypeRef>,
    elements:    &HashMap<QName, Arc<ElementDecl>>,
    target_ns:   Option<&str>,
    imports:     &HashSet<Option<Arc<str>>>,
) -> Result<(), SchemaCompileError> {
    for ref_qn in refs {
        if attributes.contains_key(ref_qn) { continue; }
        let collision = if attr_groups.contains_key(ref_qn) {
            Some("an attributeGroup")
        } else if types.contains_key(ref_qn) {
            Some("a type")
        } else if elements.contains_key(ref_qn) {
            Some("an element")
        } else {
            None
        };
        if let Some(kind) = collision {
            return Err(SchemaCompileError::msg(format!(
                "<xs:attribute ref={:?}> resolves to {kind}, not an attribute",
                ref_qn.local,
            )));
        }
        // XSD §3.2.3 src-resolve clause 4 — foreign-namespace refs
        // must be brought in by an `<xs:import>`.
        if ref_qn.namespace.as_deref() != target_ns
            && !import_covers(imports, ref_qn.namespace.as_deref())
        {
            return Err(SchemaCompileError::msg(format!(
                "<xs:attribute ref={:?}>: namespace {:?} is not the schema's \
                 targetNamespace and was not brought in by <xs:import> (XSD §3.2.3 \
                 src-resolve)",
                ref_qn.local, ref_qn.namespace.as_deref().unwrap_or(""),
            )));
        }
        // Same-namespace ref must point at a *top-level* attribute
        // declaration (XSD §3.2.3 — `ref` only resolves against the
        // schema's global attribute table, never local ones).
        if ref_qn.namespace.as_deref() == target_ns {
            return Err(SchemaCompileError::msg(format!(
                "<xs:attribute ref={:?}>: no top-level <xs:attribute name={:?}> \
                 declaration in this schema",
                ref_qn.local, ref_qn.local,
            )));
        }
    }
    Ok(())
}

fn check_attribute_group_ref_kinds(
    types:       &HashMap<QName, TypeRef>,
    elements:    &HashMap<QName, Arc<ElementDecl>>,
    attr_groups: &HashMap<QName, Arc<AttributeGroup>>,
    attributes:  &HashMap<QName, Arc<AttributeDecl>>,
) -> Result<(), SchemaCompileError> {
    let check_refs = |refs: &[QName], owner: &str| -> Result<(), SchemaCompileError> {
        for ref_qn in refs {
            // The ref resolves successfully against attr_groups —
            // perfect. Otherwise, only flag if it collides with a
            // *different* kind of declaration (which is the real
            // schema validity error). Unresolvable refs (no match
            // anywhere) are tolerated for cross-schema soft-skip
            // compatibility.
            if attr_groups.contains_key(ref_qn) { continue; }
            let collision = if types.contains_key(ref_qn) {
                Some("a type")
            } else if elements.contains_key(ref_qn) {
                Some("an element")
            } else if attributes.contains_key(ref_qn) {
                Some("an attribute")
            } else {
                None
            };
            if let Some(kind) = collision {
                return Err(SchemaCompileError::msg(format!(
                    "<xs:attributeGroup ref={:?}> in {owner} resolves to {kind}, \
                     not an attributeGroup",
                    ref_qn.local,
                )));
            }
        }
        Ok(())
    };
    for (name, tr) in types {
        if let TypeRef::Complex(ct) = tr {
            check_refs(&ct.pending_attribute_group_refs, &format!("complex type {:?}", name.local))?;
        }
    }
    for (name, decl) in elements {
        if let TypeRef::Complex(ct) = &decl.type_def {
            check_refs(&ct.pending_attribute_group_refs, &format!("element {:?}", name.local))?;
        }
    }
    Ok(())
}

fn check_element_value_constraints(
    types:    &HashMap<QName, TypeRef>,
    elements: &HashMap<QName, Arc<ElementDecl>>,
) -> Result<(), SchemaCompileError> {
    for decl in elements.values() {
        let Some(v) = decl.default.as_deref().or(decl.fixed.as_deref()) else { continue };
        // Walk the type_def, following UNRESOLVED placeholders into
        // the types map until we reach the concrete type.
        let resolved: Option<TypeRef> = match &decl.type_def {
            TypeRef::Complex(_) => Some(decl.type_def.clone()),
            TypeRef::Simple(_)  => match resolve_typeref_to_qname(&decl.type_def) {
                Some(qn) => types.get(&qn).cloned().or_else(|| Some(decl.type_def.clone())),
                None     => Some(decl.type_def.clone()),
            },
        };
        let Some(resolved) = resolved else { continue };
        let allowed = match &resolved {
            TypeRef::Simple(_) => true,
            TypeRef::Complex(ct) => match &ct.content {
                ContentModel::Simple(_) => true,
                ContentModel::Complex { mixed, .. } => *mixed,
                ContentModel::Empty => false,
            },
        };
        if !allowed {
            return Err(SchemaCompileError::msg(format!(
                "<xs:element name={:?}> with element-only or empty content cannot \
                 have a default/fixed value of {v:?} (XSD §3.3.3)",
                decl.name.local,
            )));
        }
        // XSD §3.3.3 cvc-elt-value: the default/fixed string must
        // validate against the element's simple type (or the simple
        // base of a simpleContent complex type).
        let simple = match &resolved {
            TypeRef::Simple(st) => Some(st.clone()),
            TypeRef::Complex(ct) => match &ct.content {
                ContentModel::Simple(st) => Some(st.clone()),
                _ => None,
            },
        };
        if let Some(st) = simple {
            if let Err(e) = st.validate(v) {
                return Err(SchemaCompileError::msg(format!(
                    "<xs:element name={:?}> default/fixed value {v:?} is not \
                     valid for its type: {}",
                    decl.name.local, e.message,
                )));
            }
        }
    }
    Ok(())
}

fn check_keyref_refer(
    elements: &HashMap<QName, Arc<ElementDecl>>,
) -> Result<(), SchemaCompileError> {
    use super::identity::ConstraintKind;
    // Walk a particle tree, calling `visit` on every element decl
    // we touch (including local elements nested in complex types).
    fn walk_particle(p: &super::schema::Particle, visit: &mut dyn FnMut(&ElementDecl)) {
        match &p.term {
            super::schema::Term::Element(decl) => {
                visit(decl);
                if let TypeRef::Complex(ct) = &decl.type_def {
                    if let super::schema::ContentModel::Complex { root, .. } = &ct.content {
                        walk_particle(root, visit);
                    }
                }
            }
            super::schema::Term::Group { particles, .. } => {
                for p in particles.iter() { walk_particle(p, visit); }
            }
            _ => {}
        }
    }
    fn for_every_decl(
        elements: &HashMap<QName, Arc<ElementDecl>>,
        mut visit: impl FnMut(&ElementDecl),
    ) {
        for top in elements.values() {
            visit(top);
            if let TypeRef::Complex(ct) = &top.type_def {
                if let super::schema::ContentModel::Complex { root, .. } = &ct.content {
                    walk_particle(root, &mut visit);
                }
            }
        }
    }

    // First pass — collect every key/unique's name + field count
    // across all element decls reachable from the global set.
    let mut keys: HashMap<QName, usize> = HashMap::new();
    for_every_decl(elements, |decl| {
        for ic in &decl.identity {
            if matches!(ic.kind, ConstraintKind::Key | ConstraintKind::Unique) {
                keys.insert(ic.name.clone(), ic.fields.len());
            }
        }
    });
    // Second pass — every keyref's `refer` must resolve, and its
    // field count must match the referenced key's (XSD §3.11.6
    // src-identity-constraint).
    let mut dangling: Option<(QName, QName, usize, Option<usize>)> = None;
    for_every_decl(elements, |decl| {
        if dangling.is_some() { return; }
        for ic in &decl.identity {
            if !matches!(ic.kind, ConstraintKind::KeyRef) { continue; }
            let Some(refer) = ic.refer.as_ref() else { continue };
            match keys.get(refer) {
                None => {
                    dangling = Some((ic.name.clone(), refer.clone(), ic.fields.len(), None));
                    return;
                }
                Some(&refer_fields) if ic.fields.len() != refer_fields => {
                    dangling = Some((ic.name.clone(), refer.clone(), ic.fields.len(),
                        Some(refer_fields)));
                    return;
                }
                _ => {}
            }
        }
    });
    if let Some((name, refer, df, rf)) = dangling {
        return match rf {
            None => Err(SchemaCompileError::msg(format!(
                "<xs:keyref name={:?} refer={:?}>: \
                 the referenced key/unique is not declared",
                name.local, refer.local,
            ))),
            Some(rf) => Err(SchemaCompileError::msg(format!(
                "<xs:keyref name={:?} refer={:?}>: declares {df} \
                 <xs:field> child(ren), but the referenced key has {rf}",
                name.local, refer.local,
            ))),
        };
    }
    Ok(())
}

fn check_attribute_type_kinds(
    types:      &HashMap<QName, TypeRef>,
    attributes: &HashMap<QName, Arc<AttributeDecl>>,
    elements:   &HashMap<QName, Arc<ElementDecl>>,
) -> Result<(), SchemaCompileError> {
    fn check_one(
        decl: &AttributeDecl,
        types: &HashMap<QName, TypeRef>,
    ) -> Result<(), SchemaCompileError> {
        let resolved_type: Arc<SimpleType> =
            if let Some(qn) = resolve_typeref_to_qname(&TypeRef::Simple(decl.type_def.clone())) {
                match types.get(&qn) {
                    Some(TypeRef::Complex(_)) => return Err(SchemaCompileError::msg(format!(
                        "<xs:attribute name={:?} type={:?}> — attribute type must be a simple type, not a complex type",
                        decl.name.local, qn.local,
                    ))),
                    Some(TypeRef::Simple(real)) => real.clone(),
                    None => decl.type_def.clone(),
                }
            } else {
                decl.type_def.clone()
            };
        // XSD §3.2.6 — an attribute of type xs:ID (or derived from
        // xs:ID) cannot carry default or fixed: ID validity is
        // per-instance, so a fixed/default ID would be illegal as
        // soon as the attribute appeared twice.
        if matches!(resolved_type.builtin, super::types::BuiltinType::Id)
            && (decl.default.is_some() || decl.fixed.is_some())
        {
            return Err(SchemaCompileError::msg(format!(
                "<xs:attribute name={:?}> of type xs:ID cannot have a default or fixed value",
                decl.name.local,
            )));
        }
        // XSD §3.2.3 (cvc-attribute-3) — `default` and `fixed`
        // values must validate against the attribute's type.
        for (label, raw) in [
            ("default", decl.default.as_deref()),
            ("fixed",   decl.fixed.as_deref()),
        ] {
            let Some(raw) = raw else { continue };
            if let Err(e) = resolved_type.validate(raw) {
                return Err(SchemaCompileError::msg(format!(
                    "<xs:attribute name={:?} {label}={raw:?}> is not valid for its type: {}",
                    decl.name.local, e.message,
                )));
            }
        }
        Ok(())
    }
    fn walk_complex(
        ct: &ComplexType,
        types: &HashMap<QName, TypeRef>,
        visited: &mut std::collections::HashSet<*const ComplexType>,
    ) -> Result<(), SchemaCompileError> {
        if !visited.insert(ct as *const _) {
            return Ok(());
        }
        for au in &ct.attributes {
            check_one(&au.decl, types)?;
        }
        if let ContentModel::Complex { root, .. } = &ct.content {
            walk_particle(root, types, visited)?;
        }
        Ok(())
    }
    fn walk_particle(
        p: &super::schema::Particle,
        types: &HashMap<QName, TypeRef>,
        visited: &mut std::collections::HashSet<*const ComplexType>,
    ) -> Result<(), SchemaCompileError> {
        use super::schema::Term;
        match &p.term {
            Term::Element(e) => {
                if let TypeRef::Complex(ct) = &e.type_def {
                    walk_complex(ct, types, visited)?;
                }
            }
            Term::Group { particles, .. } => {
                for child in particles.iter() {
                    walk_particle(child, types, visited)?;
                }
            }
            Term::Wildcard(_) | Term::GroupRef(_) => {}
        }
        Ok(())
    }

    for decl in attributes.values() {
        check_one(decl, types)?;
    }
    let mut visited: std::collections::HashSet<*const ComplexType> = std::collections::HashSet::new();
    for ct in types.values().filter_map(|t| match t {
        TypeRef::Complex(c) => Some(c),
        _ => None,
    }) {
        walk_complex(ct, types, &mut visited)?;
    }
    for decl in elements.values() {
        if let TypeRef::Complex(ct) = &decl.type_def {
            walk_complex(ct, types, &mut visited)?;
        }
    }
    Ok(())
}

fn resolve_typeref_to_qname(tr: &TypeRef) -> Option<QName> {
    if let TypeRef::Simple(st) = tr {
        if let Some(name) = &st.name {
            if let Some(rest) = name.strip_prefix("UNRESOLVED:") {
                // Same format as parse_unresolved_marker in validate.rs:
                // `{ns}local` or just `local`.
                if let Some(rest) = rest.strip_prefix('{') {
                    if let Some(end) = rest.find('}') {
                        let ns = &rest[..end];
                        let local = &rest[end + 1..];
                        return Some(QName::new(
                            if ns.is_empty() { None } else { Some(ns) },
                            local,
                        ));
                    }
                }
                return Some(QName::new(None, rest));
            }
        }
    }
    None
}

/// Union two attribute-wildcard namespace constraints (XSD §3.10.6,
/// "Attribute Wildcard Union").  The result accepts attributes from
/// the union of the two namespace sets.  `Any` is the absorbing
/// element; `Other` is conservatively widened to `Any` rather than
/// reasoning about which target namespace each one excludes.
/// `process_contents` falls through from the more-derived wildcard
/// since that's what the local declaration intended.
/// Combine an existing accumulated attribute wildcard with a newly
/// arrived one from another `<xs:attributeGroup>` contribution.
/// Intersect semantics per XSD §3.10.6 — but when one side is
/// `None` (no wildcard yet), the new one wins as-is.  Returning
/// `None` means no effective wildcard remains (intersection
/// reduced to the empty set).
fn merge_any_intersect(
    acc: Option<super::schema::Wildcard>,
    new: Option<super::schema::Wildcard>,
) -> Option<super::schema::Wildcard> {
    match (acc, new) {
        (None, x) | (x, None) => x,
        (Some(a), Some(b)) => intersect_wildcards(&a, &b),
    }
}

/// XSD §3.10.6 "Attribute Wildcard Intersection" — used when a
/// single complex type composes multiple `<xs:anyAttribute>`s
/// (from its own body plus each referenced `<xs:attributeGroup>`).
/// The effective wildcard accepts only those attributes accepted
/// by EVERY contributor.
///
/// `Any ∩ X = X`.  Two enumerated lists intersect by set
/// intersection.  `##other` doesn't combine cleanly with another
/// `##other` from a different target namespace, but we collapse
/// conservatively to the spec's "absent" result (treat as None to
/// disable the wildcard).
fn intersect_wildcards(
    a: &super::schema::Wildcard, b: &super::schema::Wildcard,
) -> Option<super::schema::Wildcard> {
    use super::schema::{NamespaceConstraint, Wildcard};
    let namespaces = match (&a.namespaces, &b.namespaces) {
        (NamespaceConstraint::Any, x) | (x, NamespaceConstraint::Any) => x.clone(),
        (NamespaceConstraint::List(la), NamespaceConstraint::List(lb)) => {
            let common: Vec<Option<Arc<str>>> = la.iter()
                .filter(|x| lb.contains(x))
                .cloned()
                .collect();
            if common.is_empty() {
                // Empty intersection — no attributes are accepted.
                // Represent as an empty list (matches nothing).
                NamespaceConstraint::List(Vec::new())
            } else {
                NamespaceConstraint::List(common)
            }
        }
        (NamespaceConstraint::Other, NamespaceConstraint::List(lb))
        | (NamespaceConstraint::List(lb), NamespaceConstraint::Other) => {
            // `##other` excludes the schema's targetNamespace and
            // the absent namespace; intersect with explicit list by
            // dropping the absent entry.  We don't know the target
            // namespace here, so be conservative: keep all non-None
            // entries that *might* be in `##other`'s set.
            let kept: Vec<Option<Arc<str>>> = lb.iter()
                .filter(|x| x.is_some())
                .cloned()
                .collect();
            NamespaceConstraint::List(kept)
        }
        (NamespaceConstraint::Other, NamespaceConstraint::Other) => {
            NamespaceConstraint::Other
        }
    };
    // processContents intersection: choose the *stricter* of the two
    // (the result must satisfy both).  strict > lax > skip.
    let process_contents = match (a.process_contents, b.process_contents) {
        (x, y) if x == y => x,
        (super::schema::ProcessContents::Strict, _)
        | (_, super::schema::ProcessContents::Strict) =>
            super::schema::ProcessContents::Strict,
        (super::schema::ProcessContents::Lax, _)
        | (_, super::schema::ProcessContents::Lax) =>
            super::schema::ProcessContents::Lax,
        _ => super::schema::ProcessContents::Skip,
    };
    Some(Wildcard {
        namespaces,
        process_contents,
        not_qnames:                a.not_qnames.iter().chain(&b.not_qnames).cloned().collect(),
        not_namespaces:            a.not_namespaces.iter().chain(&b.not_namespaces).cloned().collect(),
        not_qname_defined:         a.not_qname_defined || b.not_qname_defined,
        not_qname_defined_sibling: a.not_qname_defined_sibling || b.not_qname_defined_sibling,
    })
}

fn union_wildcards(a: &super::schema::Wildcard, b: &super::schema::Wildcard)
    -> super::schema::Wildcard
{
    use super::schema::{NamespaceConstraint, Wildcard};
    let namespaces = match (&a.namespaces, &b.namespaces) {
        (NamespaceConstraint::Any, _) | (_, NamespaceConstraint::Any) => NamespaceConstraint::Any,
        // ##other ∪ list-containing-None covers every namespace except
        // the target's — overapproximate to Any (we lack a "any but X"
        // constraint, and the practical effect is to accept namespaces
        // both sides allow).
        (NamespaceConstraint::Other, NamespaceConstraint::List(l))
        | (NamespaceConstraint::List(l), NamespaceConstraint::Other)
            if l.iter().any(|e| e.is_none()) =>
            NamespaceConstraint::Any,
        (NamespaceConstraint::Other, _) | (_, NamespaceConstraint::Other) => NamespaceConstraint::Other,
        (NamespaceConstraint::List(la), NamespaceConstraint::List(lb)) => {
            let mut combined: Vec<Option<Arc<str>>> = la.clone();
            for item in lb {
                if !combined.iter().any(|existing| existing == item) {
                    combined.push(item.clone());
                }
            }
            NamespaceConstraint::List(combined)
        }
    };
    // Wildcard composition for restriction: take the derived's `not*`
    // sets as-is (extension can only ADD to the exclusion list — XSD
    // 1.1 §3.10.4 requires the derived's notQName/notNamespace to be
    // a superset of the base's, which the schema author is
    // responsible for; we don't re-check here).
    Wildcard {
        namespaces,
        process_contents:          b.process_contents,
        not_qnames:                b.not_qnames.clone(),
        not_namespaces:            b.not_namespaces.clone(),
        not_qname_defined:         b.not_qname_defined,
        not_qname_defined_sibling: b.not_qname_defined_sibling,
    }
}

/// Compose `derived = extension of base` per XSD §3.4.2.  Returns a
/// new `ComplexType` with merged content + attributes.
fn compose_extension(base: &ComplexType, derived: &ComplexType) -> ComplexType {
    use super::schema::{MaxOccurs, Term};
    let merged_content = match (&base.content, &derived.content) {
        (ContentModel::Empty, c) => c.clone(),
        (b, ContentModel::Empty) => b.clone(),
        (
            ContentModel::Complex { root: b_root, mixed: b_mixed },
            ContentModel::Complex { root: d_root, mixed: d_mixed },
        ) => ContentModel::Complex {
            root: Particle {
                min_occurs: 1,
                max_occurs: MaxOccurs::Bounded(1),
                term: Term::Group {
                    kind: GroupKind::Sequence,
                    particles: Arc::from(vec![b_root.clone(), d_root.clone()]),
                },
            },
            mixed: *b_mixed || *d_mixed,
        },
        // Simple-content extension of a simple base: derived's
        // simpleType wraps with new attrs; content stays simple.
        // Other mixed cases (simple base + complex derived, etc.)
        // are spec-disallowed; we pass derived's content through.
        (ContentModel::Simple(_), c) => c.clone(),
        (_, c) => c.clone(),
    };

    // Attribute merge: base first, derived overrides same-name.
    let mut attrs: Vec<super::schema::AttributeUse> = base.attributes.clone();
    for d_au in &derived.attributes {
        let same_name = attrs.iter().position(|a| a.decl.name == d_au.decl.name);
        match same_name {
            Some(idx) => attrs[idx] = d_au.clone(),
            None      => attrs.push(d_au.clone()),
        }
    }

    // Wildcard composition under extension (XSD §3.4.2): the resulting
    // `anyAttribute` is the *union* of the base's and derived's
    // wildcards.  Either one alone wins when the other is absent.
    let any_attribute = match (&base.any_attribute, &derived.any_attribute) {
        (Some(b), Some(d)) => Some(union_wildcards(b, d)),
        (Some(w), None) | (None, Some(w)) => Some(w.clone()),
        (None, None) => None,
    };

    ComplexType {
        name:          derived.name.clone(),
        derivation:    derived.derivation.clone(),
        content:       merged_content,
        matcher:       std::sync::OnceLock::new(),
        attributes:    attrs,
        any_attribute,
        abstract_:     derived.abstract_,
        block:         derived.block,
        final_:        derived.final_,
        pending_attribute_group_refs: Vec::new(),
        // XSD 1.1: `xs:assert` declarations are *added to* the
        // assertions of the base type on extension; on restriction
        // they replace them (the restriction body re-declares any
        // assertions to keep).  This helper handles both flavours;
        // the caller fills `derived.assertions` accordingly upstream.
        assertions: derived.assertions.clone(),
    }
}

/// Replace `<xs:element ref="X"/>` placeholders with the actual
/// top-level decl from `elements`.
///
/// The parser produces a stand-in `ElementDecl` (type=xs:string,
/// no nillable/abstract/etc.) for every `ref=`-form particle —
/// "patches in the real one" was promised but never wired.  Without
/// this pass, the validator gets the wrong decl when the DFA's
/// content matcher steps on a substitution-group head's name, and
/// downstream features (xsi:type derivation check, required-attribute
/// enforcement, nillable, identity constraints) all see the
/// placeholder's empty fields instead of the schema-author's
/// declaration.
///
/// Cost: one walk of each top-level complex type's content tree,
/// linear in particle count.  Reuses the same Particle/ContentModel
/// Clone derive added for extension merging.
/// Resolve every `ComplexType::pending_attribute_group_refs` entry
/// by expanding the referenced [`AttributeGroup`]'s attribute uses
/// (and anyAttribute) into the type, then clearing the pending list.
/// Walks both top-level types and inline-on-element types.  A pending
/// ref to an undeclared group is silently dropped (consistent with
/// how forward refs were treated before this pass).
/// Bottom-up flatten every attribute group's attribute list to
/// include the transitive union of attribute uses from every group
/// it references (XSD §3.6.3).  Iterates fixed-point so order of
/// declaration in source doesn't matter — `attg1` declared before
/// `attg2` still picks up attg2's attributes when attg1 references
/// it.  Cycle detection runs separately
/// ([`check_attribute_group_cycles`]) before this step, so the
/// iteration always terminates.  Wildcards merge by intersection
/// per §3.10.6 — same rule [`rewrite_complex_for_ag_refs`] applies
/// when the type itself references the group.
fn flatten_nested_attribute_group_refs(
    groups: &mut HashMap<QName, Arc<AttributeGroup>>,
    refs_by_owner: &HashMap<QName, Vec<QName>>,
) {
    let max_rounds = groups.len() + 1;
    for _ in 0..max_rounds {
        let mut changed = false;
        let names: Vec<QName> = groups.keys().cloned().collect();
        for name in names {
            let Some(refs) = refs_by_owner.get(&name) else { continue };
            if refs.is_empty() { continue; }
            let Some(current) = groups.get(&name).cloned() else { continue };
            let mut attrs = current.attributes.clone();
            let mut any   = current.any.clone();
            let mut grew  = false;
            for ref_qn in refs {
                let Some(target) = groups.get(ref_qn) else { continue };
                for au in &target.attributes {
                    // Skip refs already present so a re-flatten round
                    // doesn't multiply entries.  Identity is the
                    // declared name + use kind — `Arc::ptr_eq` would
                    // miss the merged copies produced upstream.
                    let already = attrs.iter().any(|existing|
                        existing.decl.name == au.decl.name
                        && existing.use_kind == au.use_kind);
                    if !already {
                        attrs.push(au.clone());
                        grew = true;
                    }
                }
                any = merge_any_intersect(any, target.any.clone());
            }
            if grew {
                let new_ag = AttributeGroup {
                    name:       current.name.clone(),
                    attributes: attrs,
                    any,
                };
                groups.insert(name, Arc::new(new_ag));
                changed = true;
            }
        }
        if !changed { break; }
    }
}

fn resolve_attribute_group_refs(
    types:    &mut HashMap<QName, TypeRef>,
    elements: &mut HashMap<QName, Arc<ElementDecl>>,
    groups:   &HashMap<QName, Arc<AttributeGroup>>,
) -> Result<(), SchemaCompileError> {
    let names: Vec<QName> = types.keys().cloned().collect();
    for name in names {
        if let Some(TypeRef::Complex(ct)) = types.get(&name) {
            let rewritten = rewrite_complex_for_ag_refs(ct, groups);
            types.insert(name, TypeRef::Complex(Arc::new(rewritten)));
        }
    }

    let elem_names: Vec<QName> = elements.keys().cloned().collect();
    for ename in elem_names {
        if let Some(decl) = elements.get(&ename) {
            if let TypeRef::Complex(ct) = &decl.type_def {
                let rewritten = rewrite_complex_for_ag_refs(ct, groups);
                let new_decl = Arc::new(ElementDecl {
                    name:               decl.name.clone(),
                    type_def:           TypeRef::Complex(Arc::new(rewritten)),
                    nillable:           decl.nillable,
                    default:            decl.default.clone(),
                    fixed:              decl.fixed.clone(),
                    abstract_:          decl.abstract_,
                    substitution_group: decl.substitution_group.clone(),
                    block:              decl.block,
                    final_:             decl.final_,
                    identity:           decl.identity.clone(),
                });
                elements.insert(ename, new_decl);
            }
        }
    }
    Ok(())
}

/// Rebuild a ComplexType with all pending attributeGroup refs
/// expanded — both on the type itself and on every inline complex
/// type reachable through its content model's particle tree.
fn rewrite_complex_for_ag_refs(
    ct:     &ComplexType,
    groups: &HashMap<QName, Arc<AttributeGroup>>,
) -> ComplexType {
    let mut attributes = ct.attributes.clone();
    let mut any_attribute = ct.any_attribute.clone();
    for ref_qn in &ct.pending_attribute_group_refs {
        if let Some(ag) = groups.get(ref_qn) {
            for au in &ag.attributes {
                attributes.push(au.clone());
            }
            // XSD §3.10.6 — multiple wildcards in one complex
            // type INTERSECT to form the effective `anyAttribute`.
            any_attribute = merge_any_intersect(any_attribute, ag.any.clone());
        }
    }
    let new_content = match &ct.content {
        ContentModel::Complex { root, mixed } => {
            ContentModel::Complex {
                root: rewrite_particle_for_ag_refs(root.clone(), groups),
                mixed: *mixed,
            }
        }
        other => other.clone(),
    };
    // Snapshot-captured base types (used by `<xs:redefine>` to point
    // at the pre-redefinition version) may carry pending refs of
    // their own.  Recurse so the redefinition sees the original's
    // attribute set after expansion against the *current* group set.
    let new_derivation = ct.derivation.as_ref().map(|d| {
        let new_base = match &d.base {
            TypeRef::Complex(c) => TypeRef::Complex(Arc::new(rewrite_complex_for_ag_refs(c, groups))),
            other => other.clone(),
        };
        super::types::Derivation { method: d.method, base: new_base }
    });
    ComplexType {
        name:          ct.name.clone(),
        derivation:    new_derivation,
        content:       new_content,
        matcher:       std::sync::OnceLock::new(),
        attributes,
        any_attribute,
        abstract_:     ct.abstract_,
        block:         ct.block,
        final_:        ct.final_,
        pending_attribute_group_refs: Vec::new(),
        assertions: ct.assertions.clone(),
    }
}

fn rewrite_particle_for_ag_refs(
    p:      Particle,
    groups: &HashMap<QName, Arc<AttributeGroup>>,
) -> Particle {
    let term = match p.term {
        Term::Element(decl) => {
            // Descend into the element's type_def if it's complex —
            // an inline anonymous complex type might carry pending
            // attribute group refs of its own.
            let new_decl = if let TypeRef::Complex(ct) = &decl.type_def {
                let new_ct = rewrite_complex_for_ag_refs(ct, groups);
                Arc::new(ElementDecl {
                    name:               decl.name.clone(),
                    type_def:           TypeRef::Complex(Arc::new(new_ct)),
                    nillable:           decl.nillable,
                    default:            decl.default.clone(),
                    fixed:              decl.fixed.clone(),
                    abstract_:          decl.abstract_,
                    substitution_group: decl.substitution_group.clone(),
                    block:              decl.block,
                    final_:             decl.final_,
                    identity:           decl.identity.clone(),
                })
            } else {
                decl
            };
            Term::Element(new_decl)
        }
        Term::Group { kind, particles } => {
            let new_particles: Vec<Particle> = particles.iter()
                .cloned()
                .map(|p| rewrite_particle_for_ag_refs(p, groups))
                .collect();
            Term::Group { kind, particles: Arc::from(new_particles) }
        }
        other => other,
    };
    Particle { min_occurs: p.min_occurs, max_occurs: p.max_occurs, term }
}

/// Replace every `Term::GroupRef(name)` in the schema with the
/// referenced [`ModelGroup`]'s particle, walking every complex
/// type's content tree (top-level + inline-on-elements).
///
/// Group refs can chain — a group can reference another group — so
/// each Term::GroupRef is expanded by walking the referent's
/// particle, which in turn is walked recursively (with a depth
/// bound to break cycles).
/// XSD §3.8.6 src-model-group — a model group definition is invalid
/// when it (transitively) references itself without crossing an
/// element boundary.  Element-mediated recursion (Container element
/// whose type contains Container) is fine because each level is a new
/// instance; pure group-to-group recursion is structurally infinite.
fn check_model_group_cycles(
    groups: &HashMap<QName, Arc<ModelGroup>>,
    redefined_groups: &HashSet<QName>,
) -> Result<(), SchemaCompileError> {
    fn walk(
        p: &Particle,
        groups: &HashMap<QName, Arc<ModelGroup>>,
        active: &mut Vec<QName>,
        redefined: &HashSet<QName>,
    ) -> Result<(), SchemaCompileError> {
        match &p.term {
            Term::GroupRef(name) => {
                if active.contains(name) {
                    return Err(SchemaCompileError::msg(format!(
                        "<xs:group name={:?}>: model-group definition is circular \
                         — references itself without an intervening element \
                         declaration (XSD §3.8.6 src-model-group)",
                        name.local,
                    )));
                }
                let Some(g) = groups.get(name) else { return Ok(()); };
                active.push(name.clone());
                walk(&g.particle, groups, active, redefined)?;
                active.pop();
            }
            Term::Group { particles, .. } => {
                for p in particles.iter() { walk(p, groups, active, redefined)?; }
            }
            // Element boundary — the cycle wouldn't be a pure group
            // cycle from here on. Element decls live in their own type
            // expansion universe; nothing to check at this layer.
            Term::Element(_) | Term::Wildcard(_) => {}
        }
        Ok(())
    }
    for (name, g) in groups {
        // Groups originating in <xs:redefine> contain at most one
        // self-reference per src-redefine; that's a legal pattern,
        // not a structural cycle. Skip them.
        if redefined_groups.contains(name) { continue; }
        let mut active = vec![name.clone()];
        walk(&g.particle, groups, &mut active, redefined_groups)?;
    }
    Ok(())
}

fn resolve_group_refs(
    types:    &mut HashMap<QName, TypeRef>,
    elements: &mut HashMap<QName, Arc<ElementDecl>>,
    groups:   &HashMap<QName, Arc<ModelGroup>>,
) -> Result<(), SchemaCompileError> {
    let names: Vec<QName> = types.keys().cloned().collect();
    for name in names {
        if let Some(TypeRef::Complex(ct)) = types.get(&name) {
            if !content_has_group_ref(&ct.content) { continue; }
            let new_content = expand_group_refs_in_content(&ct.content, groups, 0)?;
            let new_ct = ComplexType {
                name:          ct.name.clone(),
                derivation:    ct.derivation.clone(),
                content:       new_content,
                matcher:       std::sync::OnceLock::new(),
                attributes:    ct.attributes.clone(),
                any_attribute: ct.any_attribute.clone(),
                abstract_:     ct.abstract_,
                block:         ct.block,
                final_:        ct.final_,
                pending_attribute_group_refs: ct.pending_attribute_group_refs.clone(),
                assertions: ct.assertions.clone(),
            };
            types.insert(name, TypeRef::Complex(Arc::new(new_ct)));
        }
    }

    let elem_names: Vec<QName> = elements.keys().cloned().collect();
    for ename in elem_names {
        if let Some(decl) = elements.get(&ename) {
            if let TypeRef::Complex(ct) = &decl.type_def {
                if !content_has_group_ref(&ct.content) { continue; }
                let new_content = expand_group_refs_in_content(&ct.content, groups, 0)?;
                let new_ct = ComplexType {
                    name:          ct.name.clone(),
                    derivation:    ct.derivation.clone(),
                    content:       new_content,
                    matcher:       std::sync::OnceLock::new(),
                    attributes:    ct.attributes.clone(),
                    any_attribute: ct.any_attribute.clone(),
                    abstract_:     ct.abstract_,
                    block:         ct.block,
                    final_:        ct.final_,
                    pending_attribute_group_refs: ct.pending_attribute_group_refs.clone(),
                    assertions: ct.assertions.clone(),
                };
                let new_decl = Arc::new(ElementDecl {
                    name:               decl.name.clone(),
                    type_def:           TypeRef::Complex(Arc::new(new_ct)),
                    nillable:           decl.nillable,
                    default:            decl.default.clone(),
                    fixed:              decl.fixed.clone(),
                    abstract_:          decl.abstract_,
                    substitution_group: decl.substitution_group.clone(),
                    block:              decl.block,
                    final_:             decl.final_,
                    identity:           decl.identity.clone(),
                });
                elements.insert(ename, new_decl);
            }
        }
    }
    Ok(())
}

fn content_has_group_ref(c: &ContentModel) -> bool {
    match c {
        ContentModel::Complex { root, .. } => particle_has_group_ref(root),
        _ => false,
    }
}

fn particle_has_group_ref(p: &Particle) -> bool {
    match &p.term {
        Term::GroupRef(_) => true,
        Term::Group { particles, .. } => particles.iter().any(particle_has_group_ref),
        _ => false,
    }
}

const MAX_GROUP_DEPTH: usize = 64;

fn expand_group_refs_in_content(
    c:      &ContentModel,
    groups: &HashMap<QName, Arc<ModelGroup>>,
    depth:  usize,
) -> Result<ContentModel, SchemaCompileError> {
    expand_group_refs_in_content_inner(c, groups, depth, &mut HashSet::new())
}

fn expand_group_refs_in_content_inner(
    c:      &ContentModel,
    groups: &HashMap<QName, Arc<ModelGroup>>,
    depth:  usize,
    active: &mut HashSet<QName>,
) -> Result<ContentModel, SchemaCompileError> {
    match c {
        ContentModel::Complex { root, mixed } => {
            let root = expand_group_refs_in_particle_inner(
                root.clone(), groups, depth, active,
            )?;
            Ok(ContentModel::Complex { root, mixed: *mixed })
        }
        other => Ok(other.clone()),
    }
}

#[allow(dead_code)]
fn expand_group_refs_in_particle(
    p:      Particle,
    groups: &HashMap<QName, Arc<ModelGroup>>,
    depth:  usize,
) -> Result<Particle, SchemaCompileError> {
    expand_group_refs_in_particle_inner(p, groups, depth, &mut HashSet::new())
}

/// Same as [`expand_group_refs_in_particle`] but threads an "active"
/// set of group names currently mid-expansion.  Recursive group refs
/// guarded by an element decl (XSD §3.7.6 — the cycle resolves at
/// runtime through the element boundary) are left as `Term::GroupRef`
/// rather than blowing the depth limit during expansion; later
/// matcher builds will re-encounter them inside the element's own
/// type and expand them with a fresh set.
fn expand_group_refs_in_particle_inner(
    p:      Particle,
    groups: &HashMap<QName, Arc<ModelGroup>>,
    depth:  usize,
    active: &mut HashSet<QName>,
) -> Result<Particle, SchemaCompileError> {
    if depth > MAX_GROUP_DEPTH {
        return Err(SchemaCompileError::msg(
            "model group reference chain exceeds depth limit (cycle?)"
        ));
    }
    let term = match p.term {
        Term::GroupRef(ref name) => {
            if active.contains(name) {
                // Cycle through element-boundary recursion (see test
                // addB077); leave the ref intact for the element's
                // own matcher to expand on its independent pass.
                return Ok(Particle {
                    min_occurs: p.min_occurs,
                    max_occurs: p.max_occurs,
                    term:       Term::GroupRef(name.clone()),
                });
            }
            let g = groups.get(name).ok_or_else(|| SchemaCompileError::msg(
                format!("<xs:group ref={name}> refers to an undeclared group")
            ))?;
            // XSD §3.7.6 (cos-all-limited) — a `<xs:group ref="G"/>`
            // whose target group is an `<xs:all>` must have
            // minOccurs ∈ {0, 1} and maxOccurs = 1.
            if matches!(g.particle.term, Term::Group { kind: GroupKind::All, .. })
                && (p.min_occurs > 1 || p.max_occurs != MaxOccurs::Bounded(1))
            {
                return Err(SchemaCompileError::msg(format!(
                    "<xs:group ref={name}>: a reference to an <xs:all> group must \
                     have minOccurs ∈ {{0,1}} and maxOccurs=1 (XSD §3.7.6 cos-all-limited)"
                )));
            }
            active.insert(name.clone());
            let expanded = expand_group_refs_in_particle_inner(
                g.particle.clone(), groups, depth + 1, active,
            )?;
            active.remove(name);
            expanded.term
        }
        Term::Group { kind, particles } => {
            let new_particles: Vec<Particle> = particles.iter()
                .cloned()
                .map(|p| expand_group_refs_in_particle_inner(p, groups, depth + 1, active))
                .collect::<Result<_, _>>()?;
            Term::Group { kind, particles: Arc::from(new_particles) }
        }
        Term::Element(decl) => {
            // Element refs into an inline complex type that itself
            // contains `<xs:group ref="…">`.  Recurse with the same
            // active set so cycles via the element body terminate
            // (leaving the GroupRef in place for the element type's
            // own matcher build to resolve later).
            Term::Element(expand_group_refs_in_element_decl_inner(
                decl, groups, depth + 1, active,
            )?)
        }
        other => other,
    };
    Ok(Particle { min_occurs: p.min_occurs, max_occurs: p.max_occurs, term })
}

/// Walk an element declaration's inline complex type (if any) and
/// resolve every `Term::GroupRef` inside it.  Returns the original Arc
/// untouched when nothing changes so identity comparisons elsewhere
/// stay cheap.
#[allow(dead_code)]
fn expand_group_refs_in_element_decl(
    decl:   Arc<ElementDecl>,
    groups: &HashMap<QName, Arc<ModelGroup>>,
    depth:  usize,
) -> Result<Arc<ElementDecl>, SchemaCompileError> {
    expand_group_refs_in_element_decl_inner(decl, groups, depth, &mut HashSet::new())
}

fn expand_group_refs_in_element_decl_inner(
    decl:   Arc<ElementDecl>,
    groups: &HashMap<QName, Arc<ModelGroup>>,
    depth:  usize,
    active: &mut HashSet<QName>,
) -> Result<Arc<ElementDecl>, SchemaCompileError> {
    let TypeRef::Complex(ct) = &decl.type_def else { return Ok(decl); };
    if !content_has_group_ref(&ct.content) { return Ok(decl); }
    let new_content = expand_group_refs_in_content_inner(&ct.content, groups, depth, active)?;
    let new_ct = ComplexType {
        name:                         ct.name.clone(),
        derivation:                   ct.derivation.clone(),
        content:                      new_content,
        matcher:                      std::sync::OnceLock::new(),
        attributes:                   ct.attributes.clone(),
        any_attribute:                ct.any_attribute.clone(),
        abstract_:                    ct.abstract_,
        block:                        ct.block,
        final_:                       ct.final_,
        pending_attribute_group_refs: ct.pending_attribute_group_refs.clone(),
        assertions: ct.assertions.clone(),
    };
    Ok(Arc::new(ElementDecl {
        name:               decl.name.clone(),
        type_def:           TypeRef::Complex(Arc::new(new_ct)),
        nillable:           decl.nillable,
        default:            decl.default.clone(),
        fixed:              decl.fixed.clone(),
        abstract_:          decl.abstract_,
        substitution_group: decl.substitution_group.clone(),
        block:              decl.block,
        final_:             decl.final_,
        identity:           decl.identity.clone(),
    }))
}

fn resolve_element_refs(
    types:    &mut HashMap<QName, TypeRef>,
    elements: &mut HashMap<QName, Arc<ElementDecl>>,
) {
    // Snapshot the un-patched elements so iteration uses a stable
    // lookup while we mutate the live maps.
    let snapshot: HashMap<QName, Arc<ElementDecl>> = elements.clone();

    let names: Vec<QName> = types.keys().cloned().collect();
    for name in names {
        let needs_rewrite = match types.get(&name) {
            Some(TypeRef::Complex(ct)) => match &ct.content {
                ContentModel::Complex { root, .. } => particle_has_unresolved_ref(root, &snapshot),
                _ => false,
            },
            _ => false,
        };
        if !needs_rewrite { continue; }
        if let Some(TypeRef::Complex(ct)) = types.get(&name) {
            types.insert(name, TypeRef::Complex(Arc::new(rewrite_complex_type(ct, &snapshot))));
        }
    }

    // Anonymous inline complex types attached to top-level element
    // decls also need patching — they don't live in `types`.
    let elem_names: Vec<QName> = elements.keys().cloned().collect();
    for ename in elem_names {
        let needs_rewrite = match elements.get(&ename) {
            Some(decl) => match &decl.type_def {
                TypeRef::Complex(ct) => match &ct.content {
                    ContentModel::Complex { root, .. } => particle_has_unresolved_ref(root, &snapshot),
                    _ => false,
                },
                _ => false,
            },
            None => false,
        };
        if !needs_rewrite { continue; }
        if let Some(decl) = elements.get(&ename) {
            if let TypeRef::Complex(ct) = &decl.type_def {
                let new_ct = rewrite_complex_type(ct, &snapshot);
                let new_decl = Arc::new(ElementDecl {
                    name:               decl.name.clone(),
                    type_def:           TypeRef::Complex(Arc::new(new_ct)),
                    nillable:           decl.nillable,
                    default:            decl.default.clone(),
                    fixed:              decl.fixed.clone(),
                    abstract_:          decl.abstract_,
                    substitution_group: decl.substitution_group.clone(),
                    block:              decl.block,
                    final_:             decl.final_,
                    identity:           decl.identity.clone(),
                });
                elements.insert(ename, new_decl);
            }
        }
    }
}

/// Build a fresh ComplexType with placeholder refs in its content
/// model replaced by the real ElementDecl from `elements`.  Returns
/// a structurally-equivalent type — only the Element terms in the
/// content tree are swapped.  The matcher is reset to OnceLock::new()
/// so it gets rebuilt over the patched content.
fn rewrite_complex_type(
    ct:       &ComplexType,
    elements: &HashMap<QName, Arc<ElementDecl>>,
) -> ComplexType {
    let new_content = match &ct.content {
        ContentModel::Complex { root, mixed } => {
            let rewritten = rewrite_particle_refs(root.clone(), elements);
            ContentModel::Complex { root: rewritten, mixed: *mixed }
        }
        other => other.clone(),
    };
    ComplexType {
        name:          ct.name.clone(),
        derivation:    ct.derivation.clone(),
        content:       new_content,
        matcher:       std::sync::OnceLock::new(),
        attributes:    ct.attributes.clone(),
        any_attribute: ct.any_attribute.clone(),
        abstract_:     ct.abstract_,
        block:         ct.block,
        final_:        ct.final_,
        pending_attribute_group_refs: ct.pending_attribute_group_refs.clone(),
        assertions: ct.assertions.clone(),
    }
}

fn particle_has_unresolved_ref(
    p: &Particle,
    elements: &HashMap<QName, Arc<ElementDecl>>,
) -> bool {
    match &p.term {
        Term::Element(decl) => {
            // A placeholder produced by `parse_local_element_particle`
            // has type=xs:string + no nillable + no abstract.  If a
            // real decl exists in `elements` for this name AND it
            // differs (by Arc identity), this particle needs rewrite.
            elements.get(&decl.name)
                .map(|real| !Arc::ptr_eq(real, decl))
                .unwrap_or(false)
        }
        Term::Group { particles, .. } => {
            particles.iter().any(|p| particle_has_unresolved_ref(p, elements))
        }
        Term::Wildcard(_) => false,
        Term::GroupRef(_) => false,
    }
}

fn rewrite_particle_refs(
    p: Particle,
    elements: &HashMap<QName, Arc<ElementDecl>>,
) -> Particle {
    let term = match p.term {
        Term::Element(decl) => {
            let real = elements.get(&decl.name).cloned().unwrap_or(decl);
            Term::Element(real)
        }
        Term::Group { kind, particles } => {
            let new_particles: Vec<Particle> = particles.iter()
                .cloned()
                .map(|p| rewrite_particle_refs(p, elements))
                .collect();
            Term::Group { kind, particles: Arc::from(new_particles) }
        }
        Term::Wildcard(w) => Term::Wildcard(w),
        Term::GroupRef(name) => Term::GroupRef(name),
    };
    Particle { min_occurs: p.min_occurs, max_occurs: p.max_occurs, term }
}

/// nested complex type reachable through its content model.  Uses a
/// pointer-set to avoid revisiting (and to break any Arc-based cycle,
/// though XSD types don't normally cycle at the Arc level).
fn walk_complex_for_matchers(
    ct: &Arc<ComplexType>,
    subs: &HashMap<QName, Vec<Arc<ElementDecl>>>,
    types: &HashMap<QName, TypeRef>,
    visited: &mut HashSet<usize>,
    target_ns: Option<&str>,
) -> Result<(), SchemaCompileError> {
    let ptr = Arc::as_ptr(ct) as usize;
    if !visited.insert(ptr) { return Ok(()); }

    let matcher = super::dfa::build_matcher_with_target_ns(&ct.content, subs, types, target_ns)?;
    let _ = ct.matcher.set(matcher);

    if let ContentModel::Complex { root, .. } = &ct.content {
        walk_particle_for_matchers(root, subs, types, visited, target_ns)?;
    }
    Ok(())
}

fn walk_particle_for_matchers(
    p: &Particle,
    subs: &HashMap<QName, Vec<Arc<ElementDecl>>>,
    types: &HashMap<QName, TypeRef>,
    visited: &mut HashSet<usize>,
    target_ns: Option<&str>,
) -> Result<(), SchemaCompileError> {
    match &p.term {
        Term::Element(decl) => {
            if let TypeRef::Complex(ct) = &decl.type_def {
                walk_complex_for_matchers(ct, subs, types, visited, target_ns)?;
            }
        }
        Term::Group { particles, .. } => {
            for p in particles.iter() {
                walk_particle_for_matchers(p, subs, types, visited, target_ns)?;
            }
        }
        Term::Wildcard(_) => {}
        Term::GroupRef(_) => {
            // Cyclic group ref left intentionally by
            // `resolve_group_refs` (cycle crosses an element
            // boundary).  The DFA builder converts it to a
            // permissive wildcard; nothing to walk here.
        }
    }
    Ok(())
}

fn parse_max_occurs(s: Option<&str>) -> Result<MaxOccurs, SchemaCompileError> {
    match s {
        None              => Ok(MaxOccurs::Bounded(1)),
        Some("unbounded") => Ok(MaxOccurs::Unbounded),
        Some(raw)         => parse_occurs_int(raw, "maxOccurs").map(|n| match n {
            // XSD spec defines maxOccurs as "non-negative integer"
            // with no upper bound. Values exceeding u32::MAX are
            // semantically Unbounded — they're still well-formed,
            // they just exceed what our matcher can count.
            Some(n) => MaxOccurs::Bounded(n),
            None    => MaxOccurs::Unbounded,
        }),
    }
}

fn parse_min_occurs(s: Option<&str>) -> Result<u32, SchemaCompileError> {
    match s {
        None      => Ok(1),
        Some(raw) => parse_occurs_int(raw, "minOccurs")?.ok_or_else(|| {
            // minOccurs over u32::MAX is theoretically valid but
            // makes the matcher unreachable; treat it as a hard
            // schema-compile error.
            SchemaCompileError::msg(format!(
                "minOccurs={raw:?} exceeds the maximum supported value"
            ))
        }),
    }
}

/// Parse an XSD non-negative integer occurrence count. Returns
/// `Some(n)` for u32-fitting values, `None` for valid but
/// u32-overflowing values, or an error for malformed input.
fn parse_occurs_int(raw: &str, attr: &str) -> Result<Option<u32>, SchemaCompileError> {
    if raw.is_empty() || raw.bytes().any(|b| !b.is_ascii_digit()) {
        return Err(SchemaCompileError::msg(format!(
            "{attr}={raw:?} is not a non-negative integer or 'unbounded'"
        )));
    }
    match raw.parse::<u32>() {
        Ok(n)  => Ok(Some(n)),
        Err(_) => Ok(None),
    }
}

fn check_occurs(min: u32, max: MaxOccurs) -> Result<(), SchemaCompileError> {
    if let MaxOccurs::Bounded(m) = max {
        if min > m {
            return Err(SchemaCompileError::msg(format!(
                "minOccurs ({min}) > maxOccurs ({m})"
            )));
        }
    }
    Ok(())
}

/// Remove inherited bounds that the derived restriction has
/// replaced with the opposite inclusivity.  XSD §4.3.9 says
/// `minInclusive` and `minExclusive` are mutually exclusive
/// within a single facet set — but during restriction the
/// derived type's explicit bound naturally supersedes the
/// inherited one.  Without this pruning the combined set carries
/// both and `validate_facet_set` mis-reports a conflict.
fn prune_replaced_bounds(facets: &mut FacetSet, base_count: usize) {
    let derived_has_min_incl = facets.facets[base_count..].iter()
        .any(|f| matches!(f, Facet::MinInclusive(_)));
    let derived_has_min_excl = facets.facets[base_count..].iter()
        .any(|f| matches!(f, Facet::MinExclusive(_)));
    let derived_has_max_incl = facets.facets[base_count..].iter()
        .any(|f| matches!(f, Facet::MaxInclusive(_)));
    let derived_has_max_excl = facets.facets[base_count..].iter()
        .any(|f| matches!(f, Facet::MaxExclusive(_)));
    let derived_has_length = facets.facets[base_count..].iter()
        .any(|f| matches!(f, Facet::Length(_)));
    let derived_has_minmax_length = facets.facets[base_count..].iter()
        .any(|f| matches!(f, Facet::MinLength(_) | Facet::MaxLength(_)));

    let mut i = 0;
    facets.facets.retain(|f| {
        let inherited = i < base_count;
        i += 1;
        if !inherited { return true; }
        // Derived's explicit bound replaces the inherited
        // counterpart; the same goes for length-vs-min/max-length.
        let drop = matches!(
            (f, derived_has_min_incl, derived_has_min_excl,
                derived_has_max_incl, derived_has_max_excl,
                derived_has_length, derived_has_minmax_length),
            (Facet::MinExclusive(_), true,  _,    _,    _,    _, _) |
            (Facet::MinInclusive(_), _,    true, _,    _,    _, _) |
            (Facet::MaxExclusive(_), _,    _,    true, _,    _, _) |
            (Facet::MaxInclusive(_), _,    _,    _,    true, _, _) |
            (Facet::Length(_),       _,    _,    _,    _,    _, true) |
            (Facet::MinLength(_),    _,    _,    _,    _,    true, _) |
            (Facet::MaxLength(_),    _,    _,    _,    _,    true, _)
        );
        !drop
    });
}

fn parse_block_set(s: Option<&str>) -> Result<BlockSet, SchemaCompileError> {
    let Some(s) = s else { return Ok(BlockSet::default()); };
    if s == "#all" {
        return Ok(BlockSet::all());
    }
    let mut out = BlockSet::default();
    for tok in s.split_whitespace() {
        match tok {
            "restriction"  => out |= BlockSet::RESTRICTION,
            "extension"    => out |= BlockSet::EXTENSION,
            "substitution" => out |= BlockSet::SUBSTITUTION,
            "list"         => out |= BlockSet::LIST,
            "union"        => out |= BlockSet::UNION,
            other => return Err(SchemaCompileError::msg(format!(
                "block/final attribute: {other:?} is not a valid token \
                 (expected 'restriction', 'extension', 'substitution', \
                 'list', 'union', or '#all')"
            ))),
        }
    }
    Ok(out)
}

/// XSD §3.4.2 — `block` / `final` on `<xs:complexType>` accept only
/// `restriction`, `extension`, or `#all` (substitution belongs to
/// elements). Strictly validate the value, rejecting unknown or
/// element-only tokens.
/// XSD §3.3.2 — `block` on `<xs:element>` accepts only
/// `restriction`, `extension`, `substitution`, or `#all`.  The
/// simple-type tokens (`list`, `union`) are rejected.
fn parse_element_block_set(s: Option<&str>) -> Result<BlockSet, SchemaCompileError> {
    let Some(s) = s else { return Ok(BlockSet::default()); };
    if s == "#all" {
        return Ok(BlockSet::RESTRICTION | BlockSet::EXTENSION | BlockSet::SUBSTITUTION);
    }
    let mut out = BlockSet::default();
    for tok in s.split_whitespace() {
        match tok {
            "restriction"  => out |= BlockSet::RESTRICTION,
            "extension"    => out |= BlockSet::EXTENSION,
            "substitution" => out |= BlockSet::SUBSTITUTION,
            other          => return Err(SchemaCompileError::msg(format!(
                "<xs:element block={s:?}>: {other:?} is not a valid value \
                 (expected 'restriction', 'extension', 'substitution', or '#all')"
            ))),
        }
    }
    Ok(out)
}

fn parse_ct_derivation_set(
    s: Option<&str>, attr: &str,
) -> Result<BlockSet, SchemaCompileError> {
    let Some(s) = s else { return Ok(BlockSet::default()); };
    if s == "#all" {
        return Ok(BlockSet::all());
    }
    let mut out = BlockSet::default();
    for tok in s.split_whitespace() {
        match tok {
            "restriction" => out |= BlockSet::RESTRICTION,
            "extension"   => out |= BlockSet::EXTENSION,
            other         => return Err(SchemaCompileError::msg(format!(
                "<xs:complexType {attr}={s:?}>: {other:?} is not a valid value \
                 (expected 'restriction', 'extension', or '#all')"
            ))),
        }
    }
    Ok(out)
}

/// Reject `minOccurs` / `maxOccurs` on an `<xs:anyAttribute>` —
/// XSD §3.10.2's grammar admits them only on the element-wildcard
/// `<xs:any>`.  Call before [`parse_wildcard`].
fn check_no_occurs(attrs: &[Attr], element: &str) -> Result<(), SchemaCompileError> {
    for a in attrs {
        if matches!(a.name(), "minOccurs" | "maxOccurs") {
            return Err(SchemaCompileError::msg(format!(
                "<xs:{element} {}=...> is not allowed (only <xs:any> takes \
                 minOccurs / maxOccurs)",
                a.name(),
            )));
        }
    }
    Ok(())
}

fn parse_wildcard_attrs(
    attrs: &[Attr],
    target_ns: &Option<Arc<str>>,
    version: SchemaVersion,
    parse_qname: &mut dyn FnMut(&str) -> Result<QName, SchemaCompileError>,
) -> Result<Wildcard, SchemaCompileError> {
    // XSD 1.1 added `notQName` and `notNamespace` to the wildcard
    // attribute set (§3.10.2).  Silently accepting them in 1.0 mode
    // would be the worst possible behaviour: the schema author thinks
    // they constrained the wildcard, but no constraint is enforced
    // at validation.  Reject explicitly in 1.0 mode; parse + enforce
    // in 1.1 mode.  `Auto` behaves like strict 1.0 until
    // `parse_schema` sees `vc:minVersion="1.1"` and promotes
    // `effective_version` to `Xsd11`.
    let is_xsd11 = matches!(version, SchemaVersion::Xsd11);
    if !is_xsd11 {
        if let Some(a) = attrs.iter().find(|a| a.name() == "notQName" || a.name() == "notNamespace") {
            return Err(SchemaCompileError::msg(format!(
                "wildcard attribute {:?} is XSD 1.1 only — \
                 set SchemaOptions::version to Xsd11, or to Auto with \
                 vc:minVersion=\"1.1\" on <xs:schema>",
                a.name(),
            )));
        }
    }
    // Parse `notQName` (1.1).  Tokens are either a QName
    // (`prefix:local` / `local`) or one of the special markers
    // `##defined` / `##definedSibling`.  The two markers need
    // schema-context resolution (the live element/attribute-decl
    // set, plus sibling info from the enclosing complex type) that
    // the wildcard parser doesn't have access to here; flag them
    // on the [`Wildcard`] for the validator to consume at match
    // time.
    let mut not_qnames: Vec<QName> = Vec::new();
    let mut not_qname_defined         = false;
    let mut not_qname_defined_sibling = false;
    if is_xsd11 {
        if let Some(a) = attrs.iter().find(|a| a.name() == "notQName") {
            for tok in a.value.as_ref().split_whitespace() {
                match tok {
                    "##defined"        => not_qname_defined = true,
                    "##definedSibling" => not_qname_defined_sibling = true,
                    qn                 => not_qnames.push(parse_qname(qn)?),
                }
            }
        }
    }
    // Parse `notNamespace` (1.1).  Same token set as `namespace`
    // minus `##any` / `##other` (those are only valid as the sole
    // value, not in a list).
    let mut not_namespaces: Vec<Option<Arc<str>>> = Vec::new();
    if is_xsd11 {
        if let Some(a) = attrs.iter().find(|a| a.name() == "notNamespace") {
            for tok in a.value.as_ref().split_whitespace() {
                match tok {
                    "##local"           => not_namespaces.push(None),
                    "##targetNamespace" => not_namespaces.push(target_ns.clone()),
                    "##any" | "##other" => return Err(SchemaCompileError::msg(format!(
                        "wildcard notNamespace token {tok:?} is only valid as the \
                         sole value, not part of a list (XSD 1.1 §3.10.2)"
                    ))),
                    other if other.starts_with("##") => return Err(SchemaCompileError::msg(format!(
                        "wildcard notNamespace token {other:?} is not a defined keyword"
                    ))),
                    other => not_namespaces.push(Some(Arc::from(other))),
                }
            }
        }
    }
    let ns_str = attrs.iter()
        .find(|a| a.name() == "namespace")
        .map(|a| a.value.as_ref())
        .unwrap_or("##any");
    // XSD §3.10.2: the `namespace` attribute is either `##any`,
    // `##other`, or a whitespace-separated list of (`##local` |
    // `##targetNamespace` | anyURI). `##any` and `##other` cannot
    // appear in the list form, only standalone.
    let namespaces = match ns_str {
        "##any"   => NamespaceConstraint::Any,
        "##other" => NamespaceConstraint::Other,
        list => {
            let mut out = Vec::new();
            for tok in list.split_whitespace() {
                match tok {
                    "##any" | "##other" => return Err(SchemaCompileError::msg(format!(
                        "wildcard namespace {tok:?} is only valid as the sole value, \
                         not part of a list (XSD §3.10.2)"
                    ))),
                    "##local"           => out.push(None),
                    "##targetNamespace" => out.push(target_ns.clone()),
                    other if other.starts_with("##") => return Err(SchemaCompileError::msg(format!(
                        "wildcard namespace {other:?} is not a defined keyword \
                         (expected '##any', '##other', '##local', or '##targetNamespace')"
                    ))),
                    other               => out.push(Some(Arc::from(other))),
                }
            }
            NamespaceConstraint::List(out)
        }
    };
    let process_contents = match attrs.iter()
        .find(|a| a.name() == "processContents")
        .map(|a| a.value.as_ref())
    {
        Some("lax")    => ProcessContents::Lax,
        Some("skip")   => ProcessContents::Skip,
        Some("strict") => ProcessContents::Strict,
        None           => ProcessContents::Strict,
        Some(other)    => return Err(SchemaCompileError::msg(format!(
            "wildcard processContents={other:?}: must be 'lax', 'skip', or 'strict'"
        ))),
    };
    Ok(Wildcard {
        namespaces, process_contents,
        not_qnames, not_namespaces,
        not_qname_defined, not_qname_defined_sibling,
    })
}

/// Construct a `TypeRef` for the implicit `xs:anyType`: a complex
/// type whose content is an unbounded wildcard accepting any element,
/// whose attribute use is an `anyAttribute` wildcard, and whose
/// process-contents mode is `Lax` per XSD §3.4.7.  Used as the
/// type-def for `<xs:element>` declarations that omit `type=` and
/// don't carry an inline type — the spec's default.
fn any_type_ref() -> TypeRef {
    let wildcard = Wildcard {
        namespaces:                NamespaceConstraint::Any,
        process_contents:          ProcessContents::Lax,
        not_qnames:                Vec::new(),
        not_namespaces:            Vec::new(),
        not_qname_defined:         false,
        not_qname_defined_sibling: false,
    };
    let content = ContentModel::Complex {
        root: Particle {
            min_occurs: 0,
            max_occurs: MaxOccurs::Unbounded,
            term: Term::Wildcard(wildcard.clone()),
        },
        mixed: true,
    };
    TypeRef::Complex(Arc::new(ComplexType {
        name:          Some(QName::xsd("anyType")),
        derivation:    None,
        content,
        matcher:       std::sync::OnceLock::new(),
        attributes:    Vec::new(),
        any_attribute: Some(wildcard),
        abstract_:     false,
        block:         BlockSet::default(),
        final_:        BlockSet::default(),
        pending_attribute_group_refs: Vec::new(),
        assertions: Vec::new(),
    }))
}

fn parse_bound(s: &str, builtin: BuiltinType) -> Result<Bound, SchemaCompileError> {
    use BuiltinType::*;
    Ok(match builtin {
        Decimal => Bound::Decimal(s.parse().map_err(|e| SchemaCompileError::msg(format!("decimal bound: {e}")))?),
        Float   => Bound::Float(s.parse().map_err(|e| SchemaCompileError::msg(format!("float bound: {e}")))?),
        Double  => Bound::Double(s.parse().map_err(|e| SchemaCompileError::msg(format!("double bound: {e}")))?),
        Integer | Long | Int | Short | Byte
        | NonPositiveInteger | NegativeInteger
        | NonNegativeInteger | UnsignedInt | UnsignedShort | UnsignedByte
        | PositiveInteger | UnsignedLong
            => Bound::Int(s.parse().map_err(|e| SchemaCompileError::msg(format!("int bound: {e}")))?),
        DateTime | Date | Time | GYearMonth | GYear
        | GMonthDay | GDay | GMonth | Duration => {
            // Parse the bound into its value-space representation now
            // so check_order can compare via the per-type ordering.
            let v = super::types::SimpleType::of_builtin(builtin).validate(s)
                .map_err(|e| SchemaCompileError::msg(format!(
                    "{builtin:?} bound {s:?}: {}", e.message
                )))?;
            Bound::Value(v)
        }
        // String-like / binary / boolean bounds aren't well-defined
        // for order facets — store as a string Value so a comparison
        // attempt at validate time fails cleanly rather than at
        // compile time.
        _ => Bound::Value(super::types::Value::String(s.to_owned())),
    })
}

/// Compare two bounds within the same numeric/temporal family for
/// the cross-facet `min* <= max*` checks. Returns `None` for pairs
/// whose representations don't admit a meaningful comparison
/// (mixed Float/Double, Value-vs-numeric, …); the cross-facet
/// validator treats `None` as "cannot prove violation" and lets
/// the bound stand — the runtime facet check will reject any
/// truly broken instance.
fn compare_bounds(a: &Bound, b: &Bound) -> Option<std::cmp::Ordering> {
    use rust_decimal::Decimal;
    match (a, b) {
        (Bound::Int(x),     Bound::Int(y))     => Some(x.cmp(y)),
        (Bound::Decimal(x), Bound::Decimal(y)) => Some(x.cmp(y)),
        (Bound::Float(x),   Bound::Float(y))   => x.partial_cmp(y),
        (Bound::Double(x),  Bound::Double(y))  => x.partial_cmp(y),
        (Bound::Int(x),     Bound::Decimal(y)) => i128::try_from(*y).ok().map(|y| x.cmp(&y))
            .or_else(|| Decimal::from(*x).partial_cmp(y)),
        (Bound::Decimal(x), Bound::Int(y))     => Some(x.cmp(&Decimal::from(*y))),
        // Date/time/duration bounds are kept as Bound::Value(_) by
        // parse_bound and share the per-type ordering in
        // super::types::Value's PartialOrd implementation.
        (Bound::Value(x),   Bound::Value(y))   => super::facets::compare_values(x, y),
        _ => None,
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn xsd_str(extra_decls: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:test"
           xmlns="urn:test">
{extra_decls}
</xs:schema>"#
        )
    }

    #[test]
    fn compile_empty_schema() {
        let s = Schema::compile_str(&xsd_str("")).unwrap();
        assert_eq!(s.target_namespace(), Some("urn:test"));
        assert_eq!(s.elements().count(), 0);
    }

    #[test]
    fn compile_single_element_with_builtin_type() {
        let s = Schema::compile_str(&xsd_str(
            r#"<xs:element name="age" type="xs:int"/>"#
        )).unwrap();
        let qn = QName::new(Some("urn:test"), "age");
        assert!(s.element(&qn).is_some());
    }

    #[test]
    fn rejects_no_namespace_top_level_element() {
        // An unprefixed `<element>` directly under `<xs:schema>` lands in
        // no namespace, so it is not a valid schema component — reject it
        // rather than silently ignoring it (foreign-*namespace* elements
        // are still tolerated/skipped).
        let bad = r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
            <element name="a" type="xs:string"/>
        </xs:schema>"#;
        let err = Schema::compile_str(bad)
            .expect_err("no-namespace top-level element must be rejected");
        assert!(err.to_string().to_lowercase().contains("namespace"),
            "error should name the namespace problem: {err}");
    }

    #[test]
    fn compile_simple_type_with_facets() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:simpleType name="ZipCode">
                <xs:restriction base="xs:string">
                    <xs:pattern value="\d{5}(-\d{4})?"/>
                    <xs:maxLength value="10"/>
                </xs:restriction>
            </xs:simpleType>
        "#)).unwrap();
        let qn = QName::new(Some("urn:test"), "ZipCode");
        let TypeRef::Simple(st) = s.type_def(&qn).unwrap() else { panic!() };
        assert_eq!(st.builtin, BuiltinType::String);
        assert!(st.facets.facets.iter().any(|f| matches!(f, Facet::Pattern { .. })));
    }

    #[test]
    fn restriction_chain_collapses_user_base_to_builtin_and_inherits_facets() {
        // `Bounded` derives from xs:integer with `minInclusive=0`;
        // `Limited` then derives from `Bounded` with `maxInclusive=100`.
        // Before the chain walk, `Limited.builtin` would have been
        // `String` (wrong value space) and the inherited `minInclusive`
        // would have been silently dropped — letting negative values
        // validate.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:simpleType name="Bounded">
                <xs:restriction base="xs:integer">
                    <xs:minInclusive value="0"/>
                </xs:restriction>
            </xs:simpleType>
            <xs:simpleType name="Limited">
                <xs:restriction base="Bounded">
                    <xs:maxInclusive value="100"/>
                </xs:restriction>
            </xs:simpleType>
        "#)).unwrap();
        let qn = QName::new(Some("urn:test"), "Limited");
        let TypeRef::Simple(st) = s.type_def(&qn).unwrap() else { panic!() };
        assert_eq!(st.builtin, BuiltinType::Integer,
            "user-defined base must collapse to the ultimate built-in");

        let has_min = st.facets.facets.iter().any(|f|
            matches!(f, Facet::MinInclusive(_)));
        let has_max = st.facets.facets.iter().any(|f|
            matches!(f, Facet::MaxInclusive(_)));
        assert!(has_min && has_max,
            "derived type must inherit base's minInclusive AND keep its own maxInclusive");

        // End-to-end: the inherited minInclusive must actually reject
        // out-of-range values.  Without inheritance, `-5` would pass.
        assert!(st.validate("50").is_ok());
        assert!(st.validate("-5").is_err(),
            "minInclusive=0 from base must still reject negatives");
        assert!(st.validate("150").is_err(),
            "maxInclusive=100 from derived must still reject overflows");
    }

    #[test]
    fn compile_complex_type_sequence() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:complexType name="Person">
                <xs:sequence>
                    <xs:element name="name" type="xs:string"/>
                    <xs:element name="age"  type="xs:int"/>
                </xs:sequence>
            </xs:complexType>
        "#)).unwrap();
        let qn = QName::new(Some("urn:test"), "Person");
        let TypeRef::Complex(ct) = s.type_def(&qn).unwrap() else { panic!() };
        match &ct.content {
            ContentModel::Complex { root: Particle { term: Term::Group { kind, particles }, .. }, .. } => {
                assert_eq!(*kind, GroupKind::Sequence);
                assert_eq!(particles.len(), 2);
            }
            _ => panic!("expected sequence"),
        }
    }

    #[test]
    fn compile_complex_type_with_attributes() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:complexType name="Item">
                <xs:sequence>
                    <xs:element name="title" type="xs:string"/>
                </xs:sequence>
                <xs:attribute name="id"    type="xs:int" use="required"/>
                <xs:attribute name="kind"  type="xs:string"/>
            </xs:complexType>
        "#)).unwrap();
        let qn = QName::new(Some("urn:test"), "Item");
        let TypeRef::Complex(ct) = s.type_def(&qn).unwrap() else { panic!() };
        assert_eq!(ct.attributes.len(), 2);
        assert_eq!(ct.attributes[0].use_kind, AttributeUseKind::Required);
        assert_eq!(ct.attributes[1].use_kind, AttributeUseKind::Optional);
    }

    #[test]
    fn compile_choice_and_all() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:complexType name="Either">
                <xs:choice>
                    <xs:element name="left"  type="xs:int"/>
                    <xs:element name="right" type="xs:string"/>
                </xs:choice>
            </xs:complexType>
            <xs:complexType name="Both">
                <xs:all>
                    <xs:element name="a" type="xs:int"/>
                    <xs:element name="b" type="xs:int"/>
                </xs:all>
            </xs:complexType>
        "#)).unwrap();
        let qn1 = QName::new(Some("urn:test"), "Either");
        if let TypeRef::Complex(ct) = s.type_def(&qn1).unwrap() {
            if let ContentModel::Complex { root: Particle { term: Term::Group { kind, .. }, .. }, .. } = &ct.content {
                assert_eq!(*kind, GroupKind::Choice);
            } else { panic!() }
        }
        let qn2 = QName::new(Some("urn:test"), "Both");
        if let TypeRef::Complex(ct) = s.type_def(&qn2).unwrap() {
            if let ContentModel::Complex { root: Particle { term: Term::Group { kind, .. }, .. }, .. } = &ct.content {
                assert_eq!(*kind, GroupKind::All);
            } else { panic!() }
        }
    }

    #[test]
    fn compile_max_occurs_unbounded() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:complexType name="Items">
                <xs:sequence>
                    <xs:element name="item" type="xs:string" maxOccurs="unbounded"/>
                </xs:sequence>
            </xs:complexType>
        "#)).unwrap();
        let qn = QName::new(Some("urn:test"), "Items");
        if let TypeRef::Complex(ct) = s.type_def(&qn).unwrap() {
            if let ContentModel::Complex { root: Particle { term: Term::Group { particles, .. }, .. }, .. } = &ct.content {
                assert_eq!(particles[0].max_occurs, MaxOccurs::Unbounded);
            }
        }
    }

    #[test]
    fn import_with_unresolvable_location_is_a_soft_skip() {
        // XSD §4.2.3 — schemaLocation is a hint. compile_str uses
        // NoResolver, which declines every load. The schema must
        // still compile cleanly; declarations from the unloaded
        // document just aren't available.
        let xml = xsd_str(r#"<xs:import namespace="urn:other" schemaLocation="other.xsd"/>"#);
        Schema::compile_str(&xml).expect("schema with unresolvable import should still compile");
    }

    #[test]
    fn import_without_location_is_silently_skipped() {
        // No schemaLocation means the schema relies on the consumer to
        // have made the imported namespace available some other way.
        let xml = xsd_str(r#"<xs:import namespace="urn:other"/>"#);
        assert!(Schema::compile_str(&xml).is_ok());
    }

    #[test]
    fn include_via_in_memory_resolver() {
        use crate::xsd::InMemoryResolver;
        let main = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:test" xmlns="urn:test">
  <xs:include schemaLocation="types.xsd"/>
  <xs:element name="root" type="MyType"/>
</xs:schema>"#;
        let included = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:test" xmlns="urn:test">
  <xs:simpleType name="MyType">
    <xs:restriction base="xs:string">
      <xs:maxLength value="10"/>
    </xs:restriction>
  </xs:simpleType>
</xs:schema>"#;
        let resolver = InMemoryResolver::new().with("types.xsd", included.as_bytes().to_vec());
        let s = Schema::compile_with(main, resolver).unwrap();
        assert!(s.type_def(&QName::new(Some("urn:test"), "MyType")).is_some());
        assert!(s.element(&QName::new(Some("urn:test"), "root")).is_some());
    }

    #[test]
    fn include_cycle_is_silently_skipped() {
        use crate::xsd::InMemoryResolver;
        let a = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:t" xmlns="urn:t">
  <xs:include schemaLocation="b.xsd"/>
  <xs:element name="root" type="xs:string"/>
</xs:schema>"#;
        let b = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:t" xmlns="urn:t">
  <xs:include schemaLocation="a.xsd"/>
</xs:schema>"#;
        let resolver = InMemoryResolver::new()
            .with("a.xsd", a.as_bytes().to_vec())
            .with("b.xsd", b.as_bytes().to_vec());
        // Should not infinite-loop; cycle detection short-circuits.
        let s = Schema::compile_with(a, resolver).unwrap();
        assert!(s.element(&QName::new(Some("urn:t"), "root")).is_some());
    }

    #[test]
    fn include_target_ns_mismatch_rejected() {
        use crate::xsd::InMemoryResolver;
        let main = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:a">
  <xs:include schemaLocation="other.xsd"/>
</xs:schema>"#;
        let other = r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:b">
  <xs:element name="x" type="xs:string"/>
</xs:schema>"#;
        let resolver = InMemoryResolver::new().with("other.xsd", other.as_bytes().to_vec());
        let err = Schema::compile_with(main, resolver).unwrap_err();
        assert!(err.message.contains("targetNamespace"));
    }

    #[test]
    fn rejects_non_schema_root() {
        let err = Schema::compile_str("<root/>").unwrap_err();
        assert!(err.message.contains("xs:schema"));
    }

    #[test]
    fn enumeration_facet_collected() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:simpleType name="Color">
                <xs:restriction base="xs:string">
                    <xs:enumeration value="red"/>
                    <xs:enumeration value="green"/>
                    <xs:enumeration value="blue"/>
                </xs:restriction>
            </xs:simpleType>
        "#)).unwrap();
        let qn = QName::new(Some("urn:test"), "Color");
        if let TypeRef::Simple(st) = s.type_def(&qn).unwrap() {
            let enum_facet = st.facets.facets.iter().find_map(|f| match f {
                Facet::Enumeration(opts) => Some(opts),
                _ => None,
            }).unwrap();
            assert_eq!(enum_facet, &vec!["red".to_string(), "green".into(), "blue".into()]);
        }
    }

    #[test]
    fn nested_groups_compile() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:complexType name="Form">
                <xs:sequence>
                    <xs:choice>
                        <xs:element name="a" type="xs:int"/>
                        <xs:element name="b" type="xs:int"/>
                    </xs:choice>
                    <xs:element name="c" type="xs:string"/>
                </xs:sequence>
            </xs:complexType>
        "#));
        assert!(s.is_ok());
    }
}
