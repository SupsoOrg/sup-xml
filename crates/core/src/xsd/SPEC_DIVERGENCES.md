# XSD spec divergences

A registry of places where our XSD validator's behaviour differs
from one or more of:

- The XSD 1.0 / 1.1 W3C Recommendations (the spec).
- The W3C XML Schema Test Suite (XSTS) "expected validity" verdicts,
  contributed by Microsoft / Sun / Boeing / NIST.
- Other implementations, principally libxml2 and Xerces.

The rule of thumb: **be accurate to the spec text first**, the XSTS
expectation second, libxml2's behaviour third. This file documents
the cases where those three disagree so future contributors don't
silently "fix" a spec-strict choice to match a deployed quirk.

## Format

Each entry names the issue, cites the spec, summarises libxml2/XSTS
behaviour, and points at the code site that implements our choice.
"Direction" is one of:

- **spec-strict**: we follow the spec text. libxml2 / XSTS are
  permissive.
- **deployed-lenient**: we follow libxml2 / XSTS. The spec text
  would require stricter behaviour but the deployed-validator
  consensus has settled the other way.

---

## 1. `<xs:any>` `processContents` restriction strictness

- **Site**: `particle_restriction.rs`, `is_valid_restriction`
  (Wildcard → Wildcard case).
- **Spec**: XSD 1.0 §3.9.6.2.3.2 NSSubset, clause 1.4:
  > R's `{process contents}` must be identical to or **stronger
  > than** B's `{process contents}`, where `strict` is stronger
  > than `lax` is stronger than `skip`.
- **XSTS**: `msData/particles/particlesOb001.xsd` through
  `Ob018.xsd` (and similar) restrict `<xs:any processContents="strict">`
  to `<xs:any processContents="lax">` and are marked **valid**.
- **libxml2 / Xerces / MSXML**: accept.
- **Our choice**: **spec-strict** — we reject. lax is weaker than
  strict, so derived (lax) cannot restrict base (strict).
- **Cost**: ~13 XSTS tests show as us-rejecting.

## 2. Unicode database version for `\d` / `\p{Lu}` etc.

- **Site**: `regex/unicode.rs`, `category_set` (built on top of
  `unicode_properties`).
- **Spec**: XSD 1.0 Appendix F.1.4 pins `\d` to `\p{Nd}` from the
  Unicode Character Database **as of the runtime's Unicode version**.
- **XSTS**: e.g. `msData/regex/reS17.xml` patterns `\d` against
  U+1369 (Ethiopic Digit One), marked **valid**. U+1369..1371 were
  `Nd` in Unicode 3.2 (when XSD 1.0 shipped) but reclassified to
  `No` in Unicode 6.2 (2012).
- **libxml2**: pinned to an older Unicode db, still treats Ethiopic
  digits as `Nd`.
- **Our choice**: **spec-strict against the current Unicode db**.
  We track whatever `unicode_properties` ships.
- **Cost**: ~7 regex instance tests.

## 3. `<xs:key>` field xpath finding no node

- **Site**: `validate.rs`, `finalize_key_scope`.
- **Spec**: XSD 1.0 §3.11.4 cvc-identity-constraint, requirement
  for `xs:key` (vs `xs:unique`): every selected element must have
  a value for every field.
- **XSTS**: `msData/identityConstraint/idG022.xml` declares
  `<xs:field xpath="fooNS:row">` where the instance only has
  `myNS:row` (different namespace), and is marked **valid** —
  reading the rule as "if no node matches, no tuple is recorded,
  the key is vacuously satisfied."
- **libxml2**: accepts the lenient reading.
- **Our choice**: **spec-strict** — `xs:key` requires every field
  to evaluate to a node. Absent fields are a key violation.
- **Cost**: handful of identity-constraint tests.

## 4. PCRE-style lone `{` outside a quantifier

- **Site**: `regex/parser.rs`.
- **Spec**: XSD Appendix F.1 only defines `{` as the opening of
  `{n}`, `{n,}`, `{n,m}` — outside that context, `{` has no role,
  and the simplest spec reading rejects it.
- **XSTS**: `msData/regex/RegexTest_24.xsd` (`{5`),
  `RegexTest_25.xsd` (`{5,`), `RegexTest_26.xsd` (`{5,6`) — all
  marked **valid**. Some implementations parse these as literal
  `{` characters.
- **libxml2**: parses lone `{` as a literal.
- **Our choice**: **spec-strict** — reject malformed quantifiers.
- **Cost**: ~3 regex schema tests.

---

## Cases where libxml2 is wrong (we are right) — for context

When this file was last updated, the XSTS vs libxml2 vs us
breakdown looked like:

- ~256 tests where we are wrong-per-XSTS and libxml2 is right.
- ~866 tests where libxml2 is wrong-per-XSTS and we are right.

The bulk of the libxml2-wrong cases:

- regex: ~478 (libxml2's `\p{IsX}` block coverage, character
  subtraction, and several quantifier interactions are incomplete).
- particles: ~188 (libxml2's particle-restriction algorithms
  diverge from §3.9.6 in several documented bugs).
- datatypes: ~89 (libxml2 facet inheritance and value-space
  arithmetic).
- everything else: smaller buckets.

So a "we lose to libxml2 here" finding is much rarer than the
reverse. Most XSTS gaps are real spec-correctness wins for us.

---

## How to add an entry

When you discover a new gray-area case:

1. Decide which side of the spec text it falls on.
2. If the spec text is unambiguous and we follow it, add a
   **spec-strict** entry.
3. If the spec text is ambiguous and the deployed-validator
   consensus differs from a literal reading, add a **deployed-lenient**
   entry — but only after checking the W3C `xmlschema-dev` archive
   and Xerces/libxml2 trackers, since some "consensus" turns out
   to be one implementation copying another's bug.
4. Reference this file from the code site that implements the
   choice (so future readers know where to update if the
   interpretation changes).
