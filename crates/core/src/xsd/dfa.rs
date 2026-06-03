//! Deterministic finite automaton for content-model matching.
//!
//! Built once at schema-compile time from a [`ContentModel`].  At
//! validation time the validator just walks the DFA: O(1) state lookup
//! per child element instead of tree-walking the particle list.
//!
//! ## Coverage
//!
//! Sequence, choice, single elements, nested groups (flattened in), and
//! wildcards all compile to DFA transitions.  `xs:all` does **not** —
//! its semantics (each particle in any order) require either subset
//! construction (worst-case 2ⁿ states) or runtime bitset tracking.  We
//! keep the existing particle-walk matcher for all-groups; the DFA path
//! is opt-in per ComplexType via [`ContentMatcher`].
//!
//! ## Determinism
//!
//! XSD §3.8.6 requires content models to satisfy *Unique Particle
//! Attribution*: each child must match at most one particle without
//! lookahead.  This means the resulting state machine is naturally
//! deterministic.  The compiler enforces UPA — any duplicate
//! transition out of the same state is a compile error.
//!
//! ## Substitution groups
//!
//! When a transition matches an element with substitutes, the
//! substitutes are pre-expanded into separate transitions.  Lookup at
//! validation time stays a linear scan (small N typical), no
//! per-validate substitution-group lookup needed.

use std::collections::HashMap;
use std::sync::Arc;

use super::error::SchemaCompileError;
use super::schema::{
    ContentModel, GroupKind, MaxOccurs, NamespaceConstraint, Particle, ProcessContents,
    QName, Term, Wildcard,
};

// ── public DFA types ─────────────────────────────────────────────────────────

pub type StateId = u32;

#[derive(Debug)]
pub struct Dfa {
    pub states:  Vec<DfaState>,
    pub initial: StateId,
    /// Names of every element declaration that appears statically in
    /// the source content model.  Consulted by `notQName="##definedSibling"`
    /// wildcards (XSD 1.1 §3.10.4) to exclude names declared as
    /// siblings in the enclosing complex type.  Sorted by `(namespace,
    /// local)` so membership checks can binary-search if the list
    /// grows; typical schemas keep this short.
    pub defined_siblings: Arc<[QName]>,
}

#[derive(Debug, Default)]
pub struct DfaState {
    /// True iff the parent's content is allowed to end at this state
    /// (all required particles satisfied so far).
    pub accept: bool,
    /// Element-name transitions.  Each carries the [`ElementDecl`]
    /// the matched element resolves to — stored inline so the
    /// validator gets both the next state AND the decl in one lookup.
    /// Linear scan — for typical N≤8 it beats a HashMap on cache use.
    pub on_element: Vec<ElementTransition>,
    /// Wildcard transitions — consulted *only* after element-name
    /// transitions don't match (specific declarations take priority
    /// over wildcards per spec).
    pub on_wildcard: Vec<WildcardTransition>,
}

#[derive(Debug, Clone)]
pub struct WildcardTransition {
    pub wc:   Wildcard,
    pub next: StateId,
    /// Synthetic transitions stand in for a `Term::GroupRef` cycle
    /// whose referenced group can't be expanded inline.  They admit
    /// `##any` so the validator over-accepts the cycle's content;
    /// UPA cannot meaningfully clash against them.
    pub synthetic: bool,
}

#[derive(Debug, Clone)]
pub struct ElementTransition {
    pub name: QName,
    pub next: StateId,
    /// Resolved declaration for the matched element — its own decl
    /// for direct matches, or the substitute's decl when the
    /// transition was generated for a substitution-group member.
    pub decl: Arc<super::schema::ElementDecl>,
}

impl Dfa {
    /// Look up the next state for an incoming element with name `qn`.
    /// Element-name transitions return both the next state and the
    /// matched [`ElementDecl`]; wildcard transitions return the next
    /// state and the wildcard's `processContents`.
    ///
    /// `wildcard_admits` decides whether a candidate wildcard accepts
    /// `qn` — the caller supplies the wildcard-semantics policy
    /// (positive namespace constraint plus the XSD 1.1 `notQName` /
    /// `notNamespace` exclusions, including `##defined` and
    /// `##definedSibling` lookups).  See [`wildcard_admits`] for the
    /// canonical implementation.
    pub fn step(
        &self,
        state: StateId,
        qn: &QName,
        mut wildcard_admits: impl FnMut(&Wildcard, &QName) -> bool,
    ) -> Option<DfaTransition> {
        let s = &self.states[state as usize];
        for t in &s.on_element {
            if &t.name == qn {
                return Some(DfaTransition::Element {
                    next: t.next,
                    decl: t.decl.clone(),
                });
            }
        }
        for t in &s.on_wildcard {
            if wildcard_admits(&t.wc, qn) {
                return Some(DfaTransition::Wildcard {
                    next: t.next,
                    process_contents: t.wc.process_contents,
                });
            }
        }
        None
    }

    pub fn is_accept(&self, state: StateId) -> bool {
        self.states[state as usize].accept
    }
}

#[derive(Debug)]
pub enum DfaTransition {
    Element { next: StateId, decl: Arc<super::schema::ElementDecl> },
    Wildcard { next: StateId, process_contents: ProcessContents },
}

/// What a [`ComplexType`](super::types::ComplexType) uses to validate
/// its content at runtime.  The Builder picks one based on the
/// content-model shape.
#[derive(Debug)]
pub enum ContentMatcher {
    /// DFA-driven matching for sequence/choice/element/wildcard models.
    Dfa(Arc<Dfa>),
    /// Particle-walk matching for `xs:all` (the runtime tracks a
    /// bitset of consumed particles).
    All,
    /// Empty or simple-content type — no matcher needed.
    None,
}

/// Build a [`ContentMatcher`] for a [`ContentModel`].  Substitution
/// groups need to be available so transitions can pre-expand to all
/// substitutes.
/// Set of derivation methods used to derive `child` from `ancestor`
/// via `types`. Used at DFA-compile time to filter substitution-group
/// members whose type derivation is blocked by their head's `block`.
pub(super) fn derivation_methods_between(
    child:    &super::schema::TypeRef,
    ancestor: &super::schema::TypeRef,
    types:    &HashMap<QName, super::schema::TypeRef>,
) -> Option<super::schema::BlockSet> {
    use super::schema::{BlockSet, TypeRef};
    use super::types::DerivationMethod;
    fn resolve(tr: &TypeRef, types: &HashMap<QName, TypeRef>) -> TypeRef {
        if let TypeRef::Simple(st) = tr {
            if let Some(name) = &st.name {
                if let Some(rest) = name.strip_prefix("UNRESOLVED:") {
                    // Same encoding as parser::resolve_typeref_to_qname.
                    let qn = if let Some(rest) = rest.strip_prefix('{') {
                        if let Some(end) = rest.find('}') {
                            let ns    = &rest[..end];
                            let local = &rest[end + 1..];
                            QName::new(if ns.is_empty() { None } else { Some(ns) }, local)
                        } else { QName::new(None, rest) }
                    } else { QName::new(None, rest) };
                    if let Some(real) = types.get(&qn) { return real.clone(); }
                }
            }
        }
        tr.clone()
    }
    fn eq(a: &TypeRef, b: &TypeRef) -> bool {
        match (a, b) {
            (TypeRef::Simple(x), TypeRef::Simple(y))   => Arc::ptr_eq(x, y) || x.name == y.name,
            (TypeRef::Complex(x), TypeRef::Complex(y)) => Arc::ptr_eq(x, y) || x.name == y.name,
            _ => false,
        }
    }
    fn is_any_type(tr: &TypeRef) -> bool {
        match tr {
            TypeRef::Complex(c) => c.name.as_ref().map(|n|
                n.namespace.as_deref() == Some(QName::XSD_NS) && &*n.local == "anyType"
            ).unwrap_or(false),
            _ => false,
        }
    }
    let child = resolve(child, types);
    let ancestor = resolve(ancestor, types);
    if eq(&child, &ancestor) { return Some(BlockSet::empty()); }
    // XSD §3.4.7 — every type, including every simple type, transitively
    // derives from xs:anyType (the ur-type root).  Substitution-group
    // typing checks routinely use anyType as the head's type to mean
    // "any element body is acceptable as a substitute," so this short
    // cut keeps that working without walking a simple-type chain that
    // doesn't carry an explicit pointer up through anySimpleType.
    if is_any_type(&ancestor) {
        return Some(BlockSet::RESTRICTION | BlockSet::EXTENSION);
    }
    if let TypeRef::Complex(ct) = &child {
        let mut methods = BlockSet::empty();
        let mut cur: Arc<super::types::ComplexType> = ct.clone();
        for _ in 0..64 {
            let d = match cur.derivation.as_ref() {
                Some(d) => d,
                // XSD §3.4.7 — a complex type without an explicit
                // derivation is implicitly an extension of xs:anyType.
                // When the ancestor we're matching is anyType itself,
                // accumulate that final extension step and succeed.
                None => return if is_any_type(&ancestor) {
                    Some(methods | BlockSet::EXTENSION)
                } else { None },
            };
            methods |= match d.method {
                DerivationMethod::Restriction => BlockSet::RESTRICTION,
                DerivationMethod::Extension   => BlockSet::EXTENSION,
            };
            let base = resolve(&d.base, types);
            if eq(&base, &ancestor) { return Some(methods); }
            match base {
                TypeRef::Complex(next) => { cur = next; }
                TypeRef::Simple(_)     => return None,
            }
        }
        None
    } else if let (TypeRef::Simple(c_st), TypeRef::Simple(a_st)) = (&child, &ancestor) {
        // XSD §3.16.6 / cos-st-derived-ok — a simple type derives from
        // another only along a restriction chain or up to xs:anySimpleType.
        // List and union types derive from xs:anySimpleType only; they do
        // NOT derive from their item / member types.
        use super::types::Variety;
        fn is_any_simple_type(st: &super::types::SimpleType) -> bool {
            // Two forms reach this branch: a real "xs:anySimpleType"
            // simple-type carrier (name = "anySimpleType"), or the
            // type_ref_for placeholder for it (the parser doesn't have
            // a BuiltinType variant for anySimpleType so the placeholder
            // path is taken).
            match st.name.as_deref() {
                Some("anySimpleType") => true,
                Some(s) => s.starts_with("UNRESOLVED:") && s.ends_with("anySimpleType"),
                None => false,
            }
        }
        if is_any_simple_type(a_st) {
            return Some(super::schema::BlockSet::RESTRICTION);
        }
        // For atomic-atomic, walk the built-in lineage.  This works for
        // the common case where the schema author derives a custom
        // simple type with the same `builtin` field as one of its
        // ancestors in the XSD type hierarchy.
        match (&c_st.variety, &a_st.variety) {
            (Variety::Atomic, Variety::Atomic) => {
                if c_st.builtin.derives_from(a_st.builtin) {
                    Some(super::schema::BlockSet::RESTRICTION)
                } else {
                    None
                }
            }
            // List/union widen the value space relative to their items
            // or members, so they aren't restrictions of any non-
            // anySimpleType ancestor.
            (Variety::List { .. }, _) | (Variety::Union { .. }, _) => None,
            _ => None,
        }
    } else {
        None
    }
}

fn resolve_typeref_via_types(
    tr: &super::schema::TypeRef,
    types: &HashMap<QName, super::schema::TypeRef>,
) -> super::schema::TypeRef {
    use super::schema::TypeRef;
    if let TypeRef::Simple(st) = tr {
        if let Some(name) = &st.name {
            if let Some(rest) = name.strip_prefix("UNRESOLVED:") {
                let qn = if let Some(rest) = rest.strip_prefix('{') {
                    if let Some(end) = rest.find('}') {
                        let ns    = &rest[..end];
                        let local = &rest[end + 1..];
                        QName::new(if ns.is_empty() { None } else { Some(ns) }, local)
                    } else { QName::new(None, rest) }
                } else { QName::new(None, rest) };
                if let Some(real) = types.get(&qn) { return real.clone(); }
            }
        }
    }
    tr.clone()
}

pub fn build_matcher(
    cm: &ContentModel,
    substitutions: &HashMap<QName, Vec<Arc<super::schema::ElementDecl>>>,
    types: &HashMap<QName, super::schema::TypeRef>,
) -> Result<ContentMatcher, SchemaCompileError> {
    build_matcher_with_target_ns(cm, substitutions, types, None)
}

pub fn build_matcher_with_target_ns(
    cm: &ContentModel,
    substitutions: &HashMap<QName, Vec<Arc<super::schema::ElementDecl>>>,
    types: &HashMap<QName, super::schema::TypeRef>,
    target_ns: Option<&str>,
) -> Result<ContentMatcher, SchemaCompileError> {
    match cm {
        ContentModel::Empty | ContentModel::Simple(_) => Ok(ContentMatcher::None),
        ContentModel::Complex { root, .. } => {
            // Top-level all-group → keep on the existing matcher.
            if let Term::Group { kind: GroupKind::All, .. } = &root.term {
                return Ok(ContentMatcher::All);
            }
            // Collect sibling-element names up front so the UPA
            // checks against `notQName="##definedSibling"` wildcards
            // see the full sibling set when each transition is added,
            // not only the prefix walked so far.
            let defined_siblings = collect_defined_siblings(root);
            let mut b = DfaBuilder::new(defined_siblings.clone(),
                                        target_ns.map(|s| Arc::<str>::from(s)));
            let frag = b.compile_particle(root, substitutions, types)?;
            // `frag.entries` are the start states; collapse to one
            // explicit initial state with all entries' outgoing
            // transitions copied in (UPA keeps this clean).
            let initial = b.merge_into_initial(&frag)?;
            // Every exit state is an accept state.
            for &x in &frag.exits {
                b.states[x as usize].accept = true;
            }
            // The fragment is "skippable" — accepts zero input — iff
            // at least one entry is also one of its exits (covers
            // choice fragments where some branch is skippable while
            // others aren't).  In that case the initial state is
            // itself an accept state.
            if frag.entries.iter().any(|e| frag.exits.contains(e)) {
                b.states[initial as usize].accept = true;
            }
            Ok(ContentMatcher::Dfa(Arc::new(Dfa {
                states: b.states,
                initial,
                defined_siblings,
            })))
        }
    }
}

// ── DFA construction ─────────────────────────────────────────────────────────

/// One sub-DFA fragment during compilation.  Consists of the entry
/// state(s) — where matching starts when the surrounding context hands
/// control to this fragment — and the exit state(s) — where matching
/// returns to the surrounding context.  Entries and exits are kept as
/// state-id sets because particles can be skipped (`minOccurs=0`),
/// which means the entries also serve as exits for the empty path.
struct Fragment {
    entries: Vec<StateId>,
    exits:   Vec<StateId>,
}

struct DfaBuilder {
    states: Vec<DfaState>,
    /// All element names declared as siblings in the source content
    /// model.  Consulted by `wildcard_might_admit` when a wildcard
    /// carries `notQName="##definedSibling"` (XSD 1.1 §3.10.4).
    defined_siblings: Arc<[QName]>,
    /// The schema's targetNamespace, used to give the `##other` /
    /// `##targetNamespace` UPA overlap checks the precise answer
    /// they need.  `None` when the schema has no targetNamespace
    /// (i.e., declarations live in the absent namespace).
    target_ns:        Option<Arc<str>>,
}

impl DfaBuilder {
    fn new(defined_siblings: Arc<[QName]>, target_ns: Option<Arc<str>>) -> Self {
        // State 0 is reserved as the global initial; we'll fill it in
        // at the top.
        Self { states: vec![DfaState::default()], defined_siblings, target_ns }
    }

    fn new_state(&mut self) -> StateId {
        let id = self.states.len() as StateId;
        self.states.push(DfaState::default());
        id
    }

    /// Add a transition `from --(name)--> to` carrying the matched
    /// element's declaration.  Errors on UPA violation (two transitions
    /// from the same state on the same element name).
    fn add_element(
        &mut self,
        from: StateId,
        name: QName,
        to: StateId,
        decl: Arc<super::schema::ElementDecl>,
    ) -> Result<(), SchemaCompileError> {
        let s = &mut self.states[from as usize];
        if let Some(existing) = s.on_element.iter().find(|t| t.name == name) {
            // Identical particle attribution is fine — same element
            // decl seen twice from the same state happens when a
            // bounded repeat is concatenated and the next iteration's
            // first element overlaps with the previous iteration's
            // last.  We keep the existing transition; the resulting
            // DFA can under-count iterations across the boundary but
            // still attributes every accepted element to a single
            // particle (which is what UPA actually requires).
            if Arc::ptr_eq(&existing.decl, &decl) {
                return Ok(());
            }
            return Err(SchemaCompileError::msg(format!(
                "Unique Particle Attribution violation: element <{name}> reachable two ways from the same content-model position"
            )));
        }
        // XSD §3.8.6 UPA — an incoming element-name transition can't
        // share its source state with a wildcard that also admits the
        // same name, unless both attribute to the same next state.
        // Synthetic wildcards (cycle stand-ins) are excluded.
        let target_ns = self.target_ns.as_deref();
        for t in &s.on_wildcard {
            if t.synthetic { continue; }
            if t.next == to { continue; }
            if wildcard_might_admit(&t.wc, &name, &self.defined_siblings, target_ns) {
                return Err(SchemaCompileError::msg(format!(
                    "Unique Particle Attribution violation: element <{name}> \
                     is also admitted by a wildcard reachable from the same \
                     content-model position"
                )));
            }
        }
        s.on_element.push(ElementTransition { name, next: to, decl });
        Ok(())
    }

    /// `synthetic` means the wildcard wasn't authored by the user —
    /// the builder fabricates one when a `Term::GroupRef` participates
    /// in a cycle (the inner group's shape isn't known here, so an
    /// `##any` wildcard stands in).  UPA cannot meaningfully clash
    /// against a synthetic wildcard since the real shape lives in
    /// the referenced group; skip UPA for those.
    fn add_wildcard_inner(&mut self, from: StateId, wc: Wildcard, to: StateId, synthetic: bool)
        -> Result<(), SchemaCompileError>
    {
        if synthetic {
            self.states[from as usize].on_wildcard.push(WildcardTransition {
                wc, next: to, synthetic,
            });
            return Ok(());
        }
        self.add_wildcard_checked(from, wc, to)
    }

    fn add_wildcard(&mut self, from: StateId, wc: Wildcard, to: StateId)
        -> Result<(), SchemaCompileError>
    {
        self.add_wildcard_inner(from, wc, to, false)
    }

    fn add_wildcard_checked(&mut self, from: StateId, wc: Wildcard, to: StateId)
        -> Result<(), SchemaCompileError>
    {
        // XSD §3.8.6 UPA — two distinct wildcards reachable from the
        // same state with overlapping namespace sets and different
        // next states cannot be attributed unambiguously.  Wildcards
        // sharing a `to` (which happens when concatenation glues the
        // same particle into adjacent positions) are NOT a clash —
        // both attributions resolve to the same particle.  Synthetic
        // cycle stand-ins are excluded.
        let target_ns = self.target_ns.as_deref();
        for t in &self.states[from as usize].on_wildcard {
            if t.synthetic { continue; }
            if t.next == to { continue; }
            if wildcards_overlap(&t.wc, &wc, target_ns) {
                return Err(SchemaCompileError::msg(
                    "Unique Particle Attribution violation: two wildcards reachable \
                     from the same content-model position have overlapping namespace \
                     constraints and attribute to different particles"
                ));
            }
        }
        // …and the same rule applies symmetrically against any
        // already-recorded element-name transitions whose name the
        // new wildcard would admit.
        for t in &self.states[from as usize].on_element {
            if t.next == to { continue; }
            if wildcard_might_admit(&wc, &t.name, &self.defined_siblings, target_ns) {
                return Err(SchemaCompileError::msg(format!(
                    "Unique Particle Attribution violation: wildcard admits \
                     element <{}> which is also reachable as a named transition \
                     from the same content-model position",
                    t.name,
                )));
            }
        }
        self.states[from as usize].on_wildcard.push(WildcardTransition {
            wc, next: to, synthetic: false,
        });
        Ok(())
    }

    /// Copy every outgoing transition of `src` into `dst`.  Used when
    /// gluing sub-DFAs together — the prior fragment's exit takes over
    /// the next fragment's entry transitions.
    fn copy_outgoing(&mut self, src: StateId, dst: StateId)
        -> Result<(), SchemaCompileError>
    {
        // Borrow checker: clone source's transition list before mutating dst.
        let src_state = &self.states[src as usize];
        let elems    = src_state.on_element.clone();
        let wilds    = src_state.on_wildcard.clone();
        let src_accept = src_state.accept;
        for t in elems {
            self.add_element(dst, t.name, t.next, t.decl)?;
        }
        for t in wilds {
            self.add_wildcard_inner(dst, t.wc, t.next, t.synthetic)?;
        }
        if src_accept {
            self.states[dst as usize].accept = true;
        }
        Ok(())
    }

    /// Build a single state collecting all entry transitions of a
    /// fragment.  Used to give the top-level Dfa one canonical
    /// `initial`.  Returns a UPA error if two entries share an
    /// outgoing transition key.
    fn merge_into_initial(&mut self, frag: &Fragment)
        -> Result<StateId, SchemaCompileError>
    {
        let initial = 0; // reserved at construction time
        for &e in &frag.entries {
            self.copy_outgoing(e, initial)?;
        }
        Ok(initial)
    }

    // ── particle compilation ───────────────────────────────────────────

    fn compile_particle(
        &mut self,
        p: &Particle,
        subs: &HashMap<QName, Vec<Arc<super::schema::ElementDecl>>>,
        types: &HashMap<QName, super::schema::TypeRef>,
    ) -> Result<Fragment, SchemaCompileError> {
        // XSD 1.0 §3.9.6: a particle with `maxOccurs="0"` contributes
        // no transitions to the content model — the schema author has
        // declared it absent.  Short-circuit to an empty fragment
        // (entry doubles as exit, no edges added) so the inner shape
        // never reaches the state graph; otherwise the loopback paths
        // in `repeat_fragment` keep the transitions live and we admit
        // children the schema forbids.
        if matches!(p.max_occurs, MaxOccurs::Bounded(0)) {
            let s = self.new_state();
            return Ok(Fragment { entries: vec![s], exits: vec![s] });
        }
        match &p.term {
            Term::Element(decl) => {
                // Build (name, decl) pairs for the element itself plus
                // every member of its substitution group.  Each
                // substitute resolves to its own decl, not the head's,
                // so the validator validates against the correct type.
                //
                // XSD 1.0 §3.3.4 / cvc-elt-2.2: an anchor with
                // `block="substitution"` (or `#all`) admits no
                // substitution-group members at all in instance
                // documents — only the anchor's own name.  Skip
                // injecting substitutes in that case.  Anchors that
                // are themselves abstract pass through the validator's
                // separate abstract-rejection path.
                //
                // §3.3.6 (cvc-elt-substitution) also says: an anchor
                // with block="restriction" / "extension" forbids
                // substitutes whose type derives from the anchor's
                // type via that method. Filter those out here.
                let mut targets: Vec<(QName, Arc<super::schema::ElementDecl>)> = Vec::new();
                targets.push((decl.name.clone(), decl.clone()));
                let blocks_substitution = decl.block
                    .contains(super::schema::BlockSet::SUBSTITUTION);
                if !blocks_substitution
                    && let Some(subs_list) = subs.get(&decl.name)
                {
                    // XSD §3.3.6 (cvc-elt-substitution): both the
                    // head element's `block` and the head element's
                    // *type*'s `block` contribute. The element's
                    // block applies methods listed on `<xs:element>`;
                    // the type's block applies methods listed on
                    // the head's `<xs:complexType>`.
                    let head_type_block: super::schema::BlockSet =
                        match resolve_typeref_via_types(&decl.type_def, types) {
                            super::schema::TypeRef::Complex(ct) => ct.block,
                            _ => super::schema::BlockSet::empty(),
                        };
                    let blocked_methods = (decl.block | head_type_block) & (
                        super::schema::BlockSet::RESTRICTION
                        | super::schema::BlockSet::EXTENSION
                    );
                    for sub in subs_list {
                        if !blocked_methods.is_empty() {
                            if let Some(used) = derivation_methods_between(
                                &sub.type_def, &decl.type_def, types,
                            ) {
                                if !(used & blocked_methods).is_empty() {
                                    continue;
                                }
                            }
                        }
                        targets.push((sub.name.clone(), sub.clone()));
                    }
                }
                self.compile_repeated(p.min_occurs, p.max_occurs,
                    |b, from, to| {
                        for (n, d) in &targets {
                            b.add_element(from, n.clone(), to, d.clone())?;
                        }
                        Ok(())
                    })
            }
            Term::Wildcard(wc) => {
                let wc = wc.clone();
                self.compile_repeated(p.min_occurs, p.max_occurs,
                    |b, from, to| b.add_wildcard(from, wc.clone(), to))
            }
            Term::Group { kind, particles } => {
                // XSD §3.8.6 — for a sequence/choice with `min > 1`,
                // `max = unbounded` wrapping a SINGLE inner particle
                // whose own `max = unbounded`, the effective bound is
                // `inner_min * outer_min .. unbounded` on a chain of
                // copies of the inner particle.  The general
                // [`repeat_group`] concat-then-loopback path drops
                // the concat's "advance" transition when the inner
                // already has a self-loop on the same element name
                // (the UPA-conflict-tolerant `add_element` keeps the
                // existing self-loop instead) — so a min=2 outer
                // wrapping a max=unbounded inner accepts only the
                // self-loop and never reaches the second-copy exit.
                // Collapse the bounds and compile the inner directly
                // when it's a single particle, sidestepping the
                // concat boundary entirely.
                if matches!(p.max_occurs, MaxOccurs::Unbounded)
                    && p.min_occurs > 1
                    && particles.len() == 1
                    && matches!(kind, GroupKind::Sequence | GroupKind::Choice)
                {
                    let inner = &particles[0];
                    let inner_min = inner.min_occurs.saturating_mul(p.min_occurs);
                    let collapsed = Particle {
                        term:       inner.term.clone(),
                        min_occurs: inner_min,
                        max_occurs: MaxOccurs::Unbounded,
                    };
                    return self.compile_particle(&collapsed, subs, types);
                }
                let build_inner = |this: &mut Self| -> Result<Fragment, SchemaCompileError> {
                    match kind {
                        GroupKind::Sequence => this.compile_sequence(particles, subs, types),
                        GroupKind::Choice   => this.compile_choice(particles, subs, types),
                        GroupKind::All      => Err(SchemaCompileError::msg(
                            "nested xs:all is not supported in v1 (use sequence/choice)"
                        )),
                    }
                };
                self.repeat_group(p.min_occurs, p.max_occurs, build_inner)
            }
            Term::GroupRef(_name) => {
                // An unresolved GroupRef at DFA build time means the
                // ref participates in a cycle that crosses an
                // element boundary (the `resolve_group_refs` pass
                // leaves these intact rather than recursing forever).
                //
                // Model the cycle position as a wildcard.  The
                // particle's own `min_occurs` / `max_occurs` apply
                // to the WHOLE referenced group; the effective range
                // contributed by this position is
                // `(p.min * inner_min, p.max * inner_max)`.  Since
                // the inner is unknown — it could collapse to zero
                // elements at runtime or expand arbitrarily — we
                // use `(0, ∞)` regardless of `p`'s outer bounds.
                // Over-accept rather than mis-reject.
                let _ = p.min_occurs;
                let _ = p.max_occurs;
                let wc = Wildcard {
                    namespaces:                    NamespaceConstraint::Any,
                    process_contents:              ProcessContents::Lax,
                    not_qnames:                    Vec::new(),
                    not_namespaces:                Vec::new(),
                    not_qname_defined:             false,
                    not_qname_defined_sibling:     false,
                };
                self.compile_repeated(0, MaxOccurs::Unbounded,
                    |b, from, to| b.add_wildcard_inner(from, wc.clone(), to, true))
            }
        }
    }

    /// Build a fragment for a single transition repeated `min..max`
    /// times.  Used by both element and wildcard particles.  The
    /// `add` closure plugs the transition definition into a state pair.
    fn compile_repeated<F>(
        &mut self,
        min: u32,
        max: MaxOccurs,
        mut add: F,
    ) -> Result<Fragment, SchemaCompileError>
    where
        F: FnMut(&mut Self, StateId, StateId) -> Result<(), SchemaCompileError>,
    {
        // Build a chain of states: s0 -> s1 -> ... -> s_n on the
        // element name.  States from index `min` onward are exits.
        // For unbounded: s0 -> s1 -> s1 (self-loop after min).
        //
        // Bounded maxOccurs larger than `LARGE_BOUND` is treated as
        // unbounded: a 9_999_999-state chain blows up DFA size and
        // memory for negligible practical benefit (the loopback
        // accepts everything the chain would up to that point and
        // more — over-accepting beyond max instead of mis-rejecting
        // before max).  Real schemas almost never use precise upper
        // bounds higher than a few dozen.
        const LARGE_BOUND: u32 = 128;
        let treat_as_unbounded = matches!(max, MaxOccurs::Unbounded)
            || matches!(max, MaxOccurs::Bounded(n) if n > LARGE_BOUND);
        let max_n: u32 = match max {
            MaxOccurs::Unbounded   => u32::MAX, // sentinel
            MaxOccurs::Bounded(n)  => n,
        };
        let chain_len = if treat_as_unbounded {
            min.max(1)
        } else {
            max_n
        };
        let chain_len = chain_len.min(LARGE_BOUND);

        let mut state_ids = Vec::with_capacity(chain_len as usize + 1);
        for _ in 0..=chain_len {
            state_ids.push(self.new_state());
        }
        for i in 0..chain_len as usize {
            add(self, state_ids[i], state_ids[i + 1])?;
        }
        if treat_as_unbounded {
            // Self-loop on the last state.
            let last = *state_ids.last().unwrap();
            add(self, last, last)?;
        }

        let entries = vec![state_ids[0]];
        let exits = if min == 0 {
            // Skippable: entry itself is also an exit.
            let mut e = vec![state_ids[0]];
            e.extend_from_slice(&state_ids[1..]);
            e
        } else {
            state_ids[(min as usize).min(state_ids.len() - 1)..].to_vec()
        };
        Ok(Fragment { entries, exits })
    }

    fn compile_sequence(
        &mut self,
        particles: &[Particle],
        subs: &HashMap<QName, Vec<Arc<super::schema::ElementDecl>>>,
        types: &HashMap<QName, super::schema::TypeRef>,
    ) -> Result<Fragment, SchemaCompileError> {
        if particles.is_empty() {
            // Empty sequence → fragment that accepts immediately.
            let s = self.new_state();
            return Ok(Fragment { entries: vec![s], exits: vec![s] });
        }

        let first = self.compile_particle(&particles[0], subs, types)?;
        if particles.len() == 1 {
            return Ok(first);
        }

        let mut current = first;
        for p in &particles[1..] {
            let next = self.compile_particle(p, subs, types)?;
            current = self.concat(current, next)?;
        }
        Ok(current)
    }

    /// Concatenate two fragments — every exit of the first picks up
    /// the entries' transitions of the second.
    fn concat(&mut self, a: Fragment, b: Fragment)
        -> Result<Fragment, SchemaCompileError>
    {
        // For each exit of a, copy each entry of b's outgoing transitions in.
        for &exit in &a.exits {
            for &entry in &b.entries {
                self.copy_outgoing(entry, exit)?;
            }
        }
        // The combined fragment's entries are a's entries.  If a was
        // skippable (entry ∈ exits), the combined fragment can start
        // by using b's entries — handled implicitly by the
        // `merge_into_initial` step.
        let entries = a.entries.clone();
        // Combined exits: b's exits, plus a's exits if b is skippable.
        let mut exits = b.exits.clone();
        if b.entries.iter().any(|e| b.exits.contains(e)) {
            for e in &a.exits {
                if !exits.contains(e) { exits.push(*e); }
            }
        }
        Ok(Fragment { entries, exits })
    }

    fn compile_choice(
        &mut self,
        particles: &[Particle],
        subs: &HashMap<QName, Vec<Arc<super::schema::ElementDecl>>>,
        types: &HashMap<QName, super::schema::TypeRef>,
    ) -> Result<Fragment, SchemaCompileError> {
        if particles.is_empty() {
            let s = self.new_state();
            return Ok(Fragment { entries: vec![s], exits: vec![s] });
        }
        let mut entries = Vec::new();
        let mut exits   = Vec::new();
        for p in particles {
            let frag = self.compile_particle(p, subs, types)?;
            entries.extend(frag.entries);
            exits.extend(frag.exits);
        }
        Ok(Fragment { entries, exits })
    }

    /// Wrap a fragment in min/max occurrence handling — used when an
    /// `xs:sequence` or `xs:choice` itself has `minOccurs`/`maxOccurs`
    /// other than 1.  For v1 we support the common cases (max=1 + min
    /// ≥ 0; arbitrary min with unbounded max via a self-concat).  Other
    /// shapes return the unwrapped fragment with a documented note.
    /// Apply `minOccurs` / `maxOccurs` to a model-group particle.
    ///
    /// Strategy: when `minOccurs > 1`, we *concatenate* `min` fresh
    /// copies of the inner fragment so the validator can't shortcut
    /// through fewer iterations.  For everything after the minimum we
    /// fall back to the cheap loopback (treating the tail as
    /// unbounded) — this can over-accept when `maxOccurs` is bounded,
    /// but never crashes with state-explosion and avoids the spurious
    /// UPA conflicts that a full unroll triggers when an inner
    /// element name overlaps between consecutive iterations.
    fn repeat_group<F>(
        &mut self,
        min: u32,
        max: MaxOccurs,
        mut build_inner: F,
    ) -> Result<Fragment, SchemaCompileError>
    where
        F: FnMut(&mut Self) -> Result<Fragment, SchemaCompileError>,
    {
        if min <= 1 {
            let inner = build_inner(self)?;
            return self.repeat_fragment(inner, min, max);
        }
        // Cap the mandatory copy count.  XSD permits arbitrary
        // values, but anything beyond a handful is either pathological
        // or wraps a large content model — in both cases we'd rather
        // accept a few extra (over-permissive) instances than blow up
        // DFA state count.
        let required = (min as usize).min(8);

        let mut current = build_inner(self)?;
        let mut last_entries = current.entries.clone();
        for _ in 1..required {
            let next = build_inner(self)?;
            last_entries = next.entries.clone();
            current = self.concat(current, next)?;
        }
        // After the mandatory copies, loop the final iteration back
        // to itself so the validator can take any number of
        // additional rounds — bounded `max` becomes over-permissive
        // (we accept a few extra iterations the spec forbids) but
        // we never under-accept.  The loopback uses the *last*
        // copy's entries (which match the current exits' position
        // in iteration space) so the targets remain accepting.
        let _ = max;
        let pseudo = Fragment { entries: last_entries, exits: current.exits.clone() };
        self.add_loopback_transitions(&pseudo)?;
        Ok(current)
    }

    fn repeat_fragment(
        &mut self,
        frag: Fragment,
        min: u32,
        max: MaxOccurs,
    ) -> Result<Fragment, SchemaCompileError> {
        // Single occurrence — most common case.
        if min == 1 && matches!(max, MaxOccurs::Bounded(1)) {
            return Ok(frag);
        }
        // Optional (0..=1) — entry doubles as exit.
        if min == 0 && matches!(max, MaxOccurs::Bounded(1)) {
            let mut frag = frag;
            for &e in &frag.entries.clone() {
                if !frag.exits.contains(&e) {
                    frag.exits.push(e);
                }
            }
            return Ok(frag);
        }
        // Unbounded — splice each entry's outgoing transitions onto
        // every exit so the fragment can be re-entered.  For min=0
        // entries are also exits (zero iterations allowed).
        if matches!(max, MaxOccurs::Unbounded) {
            let mut frag = frag;
            self.add_loopback_transitions(&frag)?;
            if min == 0 {
                for &e in &frag.entries.clone() {
                    if !frag.exits.contains(&e) {
                        frag.exits.push(e);
                    }
                }
            }
            return Ok(frag);
        }
        // Bounded re-iteration (max ≥ 2): exact bound enforcement
        // would require either an NFA → DFA pass or a state-per-
        // iteration unroll, both of which interact badly with our
        // single-transition-per-(state, name) UPA check.  The
        // pragmatic tradeoff: treat the upper bound as unbounded
        // (a few extra iterations may slip through) but enforce
        // the lower bound exactly via the caller's mandatory-copy
        // concat.  For `min == 0 || min == 1` reps the optional
        // first iteration also needs entries to be exits.
        let mut frag = frag;
        self.add_loopback_transitions(&frag)?;
        if min == 0 {
            for &e in &frag.entries.clone() {
                if !frag.exits.contains(&e) {
                    frag.exits.push(e);
                }
            }
        }
        Ok(frag)
    }

    /// For every (exit, entry) pair in the fragment, copy the entry's
    /// outgoing element + wildcard transitions onto the exit.  This
    /// turns the fragment into a Kleene-plus loop.  Skipped when
    /// entry == exit (the loop is already implicit on that state).
    fn add_loopback_transitions(&mut self, frag: &Fragment) -> Result<(), SchemaCompileError> {
        for &exit in &frag.exits {
            for &entry in &frag.entries {
                if exit == entry { continue; }
                let entry_elems  = self.states[entry as usize].on_element.clone();
                let entry_wilds  = self.states[entry as usize].on_wildcard.clone();
                let exit_state   = &mut self.states[exit as usize];
                for t in entry_elems {
                    if !exit_state.on_element.iter().any(|e| e.name == t.name) {
                        exit_state.on_element.push(t);
                    }
                }
                for t in entry_wilds {
                    let already_present = exit_state.on_wildcard.iter()
                        .any(|other| std::ptr::eq(&other.wc as *const _, &t.wc as *const _));
                    if !already_present {
                        exit_state.on_wildcard.push(t);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Conservative test: does the wildcard's positive namespace
/// constraint admit `qn`?  An exact `notQName="…"` exclusion is
/// honoured; `notNamespace` and the `##defined` / `##definedSibling`
/// tokens require schema context the DFA builder doesn't have, so
/// we ignore them here — the conservative answer (treat as admitted)
/// is what makes the UPA check report the clash rather than miss it.
fn wildcard_might_admit(
    wc: &Wildcard,
    qn: &QName,
    defined_siblings: &[QName],
    target_ns: Option<&str>,
) -> bool {
    use NamespaceConstraint::*;
    // notQName= literal QNames is the one exclusion we can apply
    // unambiguously at compile time.
    if !qn.local.is_empty() {
        for forbidden in &wc.not_qnames {
            if forbidden.namespace == qn.namespace && forbidden.local == qn.local {
                return false;
            }
        }
    }
    // `##definedSibling` excludes the names that the source content
    // model declares as siblings of this wildcard — the same set
    // the validator consults at instance time.
    if wc.not_qname_defined_sibling && defined_siblings.iter().any(|s| s == qn) {
        return false;
    }
    let ns = qn.namespace.as_deref();
    match &wc.namespaces {
        Any => true,
        Other => ns.is_some() && ns != target_ns,
        List(allowed) => allowed.iter().any(|item| match (item.as_deref(), ns) {
            (None,    None)    => true,
            (Some(a), Some(b)) => a == b,
            _ => false,
        }),
    }
}

/// Conservative overlap predicate between two wildcards' namespace
/// constraints: returns true when there exists some namespace that
/// both constraints admit.  Used to detect UPA violations between
/// distinct wildcards at the same DFA state.  Notes:
///
/// * `notNamespace` / `notQName` exclusions are not subtracted here —
///   excluding from one side without excluding from the other can
///   still leave overlap, so the conservative answer is "overlap".
fn wildcards_overlap(a: &Wildcard, b: &Wildcard, target_ns: Option<&str>) -> bool {
    use NamespaceConstraint::*;
    // `##other` admits a list entry iff that entry is some non-target
    // URI.  An entry equal to the targetNamespace or `None`
    // (no-namespace) is excluded from `##other`.
    fn list_admits_other(list: &[Option<Arc<str>>], target_ns: Option<&str>) -> bool {
        list.iter().any(|entry| match entry.as_deref() {
            None      => false,
            Some(ns)  => target_ns != Some(ns),
        })
    }
    match (&a.namespaces, &b.namespaces) {
        (Any, _) | (_, Any) => true,
        (Other, Other)      => true,
        (Other, List(l)) | (List(l), Other) => list_admits_other(l, target_ns),
        (List(la), List(lb)) => la.iter().any(|x| lb.iter().any(|y| x == y)),
    }
}

// ── helpers shared with the validator ────────────────────────────────────────

/// Full XSD 1.1 §3.10.4 wildcard-match decision: does the wildcard
/// admit `qn` once positive namespace and all `notNamespace`,
/// `notQName`, `##defined`, and `##definedSibling` exclusions are
/// applied?
///
/// `is_defined` is consulted only when the wildcard's `notQName`
/// carries `##defined` (XSD 1.1: "any element/attribute with a
/// top-level declaration of this kind in the schema").  Element
/// wildcards pass `|q| schema.element(q).is_some()`; attribute
/// wildcards pass `|q| schema.attribute(q).is_some()`.
///
/// `is_sibling` is consulted only when `notQName` carries
/// `##definedSibling` — the set of element/attribute names declared
/// as siblings in the enclosing complex type.  For element wildcards
/// the DFA caches this on [`Dfa::defined_siblings`]; for attribute
/// wildcards the caller walks `ComplexType::attributes`.
pub(super) fn wildcard_admits(
    wc:         &Wildcard,
    qn:         &QName,
    target_ns:  Option<&str>,
    is_defined: impl FnOnce(&QName) -> bool,
    is_sibling: impl FnOnce(&QName) -> bool,
) -> bool {
    let ns = qn.namespace.as_deref();
    let ns_ok = match &wc.namespaces {
        NamespaceConstraint::Any   => true,
        NamespaceConstraint::Other => ns != target_ns && ns.is_some(),
        NamespaceConstraint::List(allowed) => allowed.iter().any(|item| {
            match (item.as_deref(), ns) {
                (None,    None)    => true,
                (Some(a), Some(b)) => a == b,
                _ => false,
            }
        }),
    };
    if !ns_ok { return false; }
    for item in &wc.not_namespaces {
        match (item.as_deref(), ns) {
            (None,    None)             => return false,
            (Some(a), Some(b)) if a == b => return false,
            _ => {}
        }
    }
    if !qn.local.is_empty() {
        for forbidden in &wc.not_qnames {
            if forbidden.namespace == qn.namespace && forbidden.local == qn.local {
                return false;
            }
        }
    }
    if wc.not_qname_defined && is_defined(qn) {
        return false;
    }
    if wc.not_qname_defined_sibling && is_sibling(qn) {
        return false;
    }
    true
}

/// Collect the names of every `Term::Element` particle reachable from
/// `root` — the static `##definedSibling` set for the enclosing
/// complex type's content model.  Walks through nested groups; does
/// not expand substitution-group members (those are independent
/// top-level declarations, not sibling declarations of this type).
fn collect_defined_siblings(root: &Particle) -> Arc<[QName]> {
    let mut out: Vec<QName> = Vec::new();
    fn walk(p: &Particle, out: &mut Vec<QName>) {
        match &p.term {
            Term::Element(decl) => {
                if !out.iter().any(|n| n == &decl.name) {
                    out.push(decl.name.clone());
                }
            }
            Term::Group { particles, .. } => {
                for c in particles.iter() { walk(c, out); }
            }
            Term::Wildcard(_) | Term::GroupRef(_) => {}
        }
    }
    walk(root, &mut out);
    out.sort_by(|a, b| (a.namespace.as_deref(), a.local.as_ref())
        .cmp(&(b.namespace.as_deref(), b.local.as_ref())));
    out.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xsd::schema::{ElementDecl, BlockSet};
    use crate::xsd::types::SimpleType;
    use crate::xsd::BuiltinType;

    fn elem_decl(name: &str) -> Arc<ElementDecl> {
        Arc::new(ElementDecl {
            name: QName::new(None, name),
            type_def: super::super::TypeRef::Simple(Arc::new(SimpleType::of_builtin(BuiltinType::String))),
            nillable: false,
            default: None, fixed: None,
            abstract_: false, substitution_group: None,
            block: BlockSet::default(), final_: BlockSet::default(),
            identity: Vec::new(),
        })
    }

    fn one_of(name: &str) -> Particle {
        Particle {
            min_occurs: 1, max_occurs: MaxOccurs::Bounded(1),
            term: Term::Element(elem_decl(name)),
        }
    }

    fn unbounded(name: &str) -> Particle {
        Particle {
            min_occurs: 1, max_occurs: MaxOccurs::Unbounded,
            term: Term::Element(elem_decl(name)),
        }
    }

    fn optional(name: &str) -> Particle {
        Particle {
            min_occurs: 0, max_occurs: MaxOccurs::Bounded(1),
            term: Term::Element(elem_decl(name)),
        }
    }

    fn sequence(particles: Vec<Particle>) -> ContentModel {
        ContentModel::Complex {
            root: Particle {
                min_occurs: 1, max_occurs: MaxOccurs::Bounded(1),
                term: Term::Group { kind: GroupKind::Sequence, particles: particles.into() },
            },
            mixed: false,
        }
    }

    fn choice(particles: Vec<Particle>) -> ContentModel {
        ContentModel::Complex {
            root: Particle {
                min_occurs: 1, max_occurs: MaxOccurs::Bounded(1),
                term: Term::Group { kind: GroupKind::Choice, particles: particles.into() },
            },
            mixed: false,
        }
    }

    fn matcher(cm: &ContentModel) -> Arc<Dfa> {
        match build_matcher(cm, &HashMap::new(), &HashMap::new()).unwrap() {
            ContentMatcher::Dfa(d) => d,
            _ => panic!("expected DFA"),
        }
    }

    fn run<'a>(dfa: &Dfa, names: impl IntoIterator<Item = &'a str>) -> Option<bool> {
        let mut s = dfa.initial;
        for n in names {
            let qn = QName::new(None, n);
            let step = dfa.step(s, &qn, |wc, qn| {
                wildcard_admits(wc, qn, None, |_| false, |q| {
                    dfa.defined_siblings.iter().any(|n| n == q)
                })
            });
            match step {
                Some(DfaTransition::Element { next, .. })  => s = next,
                Some(DfaTransition::Wildcard { next, .. }) => s = next,
                None => return None,
            }
        }
        Some(dfa.is_accept(s))
    }

    #[test]
    fn sequence_single_element() {
        let cm = sequence(vec![one_of("a")]);
        let dfa = matcher(&cm);
        assert_eq!(run(&dfa, ["a"]), Some(true));
        assert_eq!(run(&dfa, []), Some(false));        // missing
        assert_eq!(run(&dfa, ["a", "a"]), None);       // too many
        assert_eq!(run(&dfa, ["b"]), None);            // wrong name
    }

    #[test]
    fn sequence_multiple_elements() {
        let cm = sequence(vec![one_of("a"), one_of("b"), one_of("c")]);
        let dfa = matcher(&cm);
        assert_eq!(run(&dfa, ["a", "b", "c"]), Some(true));
        assert_eq!(run(&dfa, ["a", "b"]), Some(false)); // c missing
        assert_eq!(run(&dfa, ["a", "c"]), None);        // wrong order
        assert_eq!(run(&dfa, ["b", "a", "c"]), None);   // wrong order
    }

    #[test]
    fn sequence_with_unbounded() {
        let cm = sequence(vec![unbounded("item")]);
        let dfa = matcher(&cm);
        assert_eq!(run(&dfa, ["item"]), Some(true));
        assert_eq!(run(&dfa, ["item", "item", "item"]), Some(true));
        assert_eq!(run(&dfa, []), Some(false));         // min 1 unmet
    }

    #[test]
    fn sequence_with_optional_tail() {
        let cm = sequence(vec![one_of("a"), optional("b")]);
        let dfa = matcher(&cm);
        assert_eq!(run(&dfa, ["a"]), Some(true));
        assert_eq!(run(&dfa, ["a", "b"]), Some(true));
        assert_eq!(run(&dfa, []), Some(false));
    }

    #[test]
    fn choice_first_branch() {
        let cm = choice(vec![one_of("a"), one_of("b")]);
        let dfa = matcher(&cm);
        assert_eq!(run(&dfa, ["a"]), Some(true));
        assert_eq!(run(&dfa, ["b"]), Some(true));
        assert_eq!(run(&dfa, ["c"]), None);
        assert_eq!(run(&dfa, ["a", "b"]), None); // can't take both branches
    }

    #[test]
    fn upa_violation_is_compile_error() {
        // Two particles in a choice with the same name → ambiguous.
        let cm = choice(vec![one_of("a"), one_of("a")]);
        let r = build_matcher(&cm, &HashMap::new(), &HashMap::new());
        assert!(r.is_err(), "expected UPA error");
    }
}
