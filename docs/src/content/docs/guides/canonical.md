---
title: Canonical XML
description: Canonicalize documents for XML-DSig, SAML, eIDAS / XAdES, WS-Security — Canonical XML 1.0 and Exclusive C14N 1.0, with or without comments.
---

XML canonicalization (C14N) produces a byte-stable serialization of a
document so two structurally-equivalent inputs hash to the same value.
It's the primitive under XML digital signatures: XML-DSig, SAML
assertions, eIDAS / XAdES, WS-Security, and various other protocols
hash the canonical form, not the wire form.

SupXML implements **Canonical XML 1.0** and **Exclusive C14N 1.0**, each
with and without comments. Both ship in the default feature set (no
extra crate).

## Canonical XML 1.0 (Inclusive)

The original W3C canonicalization spec. Most XML-DSig deployments
predating Exclusive C14N use it.

```rust
use sup_xml::{parse_str, canonicalize_to_bytes, CanonicalizeOptions, C14nMode};

let doc = parse_str("<r b='2' a='1'/>", &Default::default())?;
let opts = CanonicalizeOptions {
    mode: C14nMode::C14n10,
    with_comments: false,
};
let c14n: Vec<u8> = canonicalize_to_bytes(&doc, &opts);
// → b"<r a=\"1\" b=\"2\"></r>"
```

Inclusive C14N pulls every in-scope namespace into each canonicalized
element. That's exactly what you want when the document is signed
*together with* its ambient namespace context — and exactly what you
*don't* want when you sign a subtree that'll later be embedded into a
different document with different namespaces (the inherited
declarations would then become "wrong" but the signature would still
demand them).

## Exclusive C14N 1.0 — for SAML, WS-Security, XAdES

What modern XML-DSig deployments use. Drops in-scope namespaces that
aren't actually used in the canonicalized output, so a signed subtree
can be moved into a different document without invalidating the
signature.

```rust
let opts = CanonicalizeOptions {
    mode: C14nMode::ExcC14n10 { inclusive_prefixes: vec![] },
    with_comments: false,
};
let c14n = canonicalize_to_bytes(&doc, &opts);
```

`inclusive_prefixes` pins specific prefixes that must stay in scope
regardless of whether the canonical form references them (common when
downstream consumers expect a particular `wsse:` / `saml:` / `xenc:`
binding):

```rust
let opts = CanonicalizeOptions {
    mode: C14nMode::ExcC14n10 {
        inclusive_prefixes: vec!["wsse".into(), "ds".into()],
    },
    with_comments: false,
};
```

Pass `""` (empty string) to force the default namespace into the
inclusive set.

## With or without comments

`with_comments: true` emits `<!-- … -->` nodes in the canonical
output. Default is **without** — comments don't carry semantics and
including them makes signatures sensitive to non-load-bearing edits
(reformatting, license-header insertion). Use the with-comments form
only when the protocol explicitly requires it.

```rust
let opts = CanonicalizeOptions {
    mode: C14nMode::C14n10,
    with_comments: true,
};
```

## Subset canonicalization

For signing a *portion* of a document — the typical Enveloped
Signature pattern in XML-DSig where the signature itself sits next to
the signed subtree — use `canonicalize_with` and pass a visibility
predicate that filters out the nodes you want excluded. The predicate
sees one `CanonicalizeVisitTarget` per node and per attribute; returning `false`
on an element skips its entire subtree, returning `false` on a
non-element node or an attribute skips just that node / attribute.

```rust
use sup_xml::{canonicalize_with, CanonicalizeVisitTarget};

let mut buf = Vec::new();
canonicalize_with(&doc, &opts, &mut buf, |target| {
    match target {
        // Skip the <Signature> element and everything under it
        CanonicalizeVisitTarget::Node(n) => n.name() != "Signature",
        CanonicalizeVisitTarget::Attribute(_) => true,
    }
})?;
```

For canonicalizing a single subtree rather than the whole document
(no inherited ancestor namespaces), use `canonicalize_node_to_bytes`
or `canonicalize_node_with`:

```rust
use sup_xml::canonicalize_node_to_bytes;

let target_node = /* … walk to the node you want */;
let c14n: Vec<u8> = canonicalize_node_to_bytes(&target_node, &opts);
```

## Streaming canonicalization

For large documents where materialising the full canonical form in
memory is expensive (hashing into a signature, streaming to a network
socket), pass a `Write` sink directly:

```rust
use sup_xml::canonicalize_with;
use sha2::{Sha256, Digest};

let mut hasher = Sha256::new();
canonicalize_with(&doc, &opts, &mut hasher, |_| true)?;
let digest = hasher.finalize();
```

`sha2::Sha256` implements `Write` (it consumes the bytes into the
running hash without buffering), so the canonical form never lands
in a `Vec` — useful when the document is hundreds of MB and you only
need the digest.

## From the shell

```bash
sup-xml c14n input.xml                        # Canonical XML 1.0, no comments
sup-xml c14n --exclusive input.xml            # Exclusive C14N 1.0
sup-xml c14n --with-comments input.xml        # any mode + comments
```

## What it doesn't do

- **Signing.** C14N is the *primitive* under signing — the byte
  stream you feed into HMAC / RSA-SHA256 / ECDSA. SupXML doesn't
  ship the signing layer itself; pair it with the `rsa` /
  `ed25519-dalek` / `ring` crate of your choice.
- **Canonical XML 1.1.** The 1.0 modes cover the overwhelming
  majority of XML-DSig and SAML deployments in the wild. 1.1's
  `xml:id` / `xml:base` propagation refinements are tracked for a
  future release; if you need them today, file an issue.
- **Signature verification across implementations.** When a
  signature you produce verifies against libxml2's
  `xmlSecDSigCtxVerify` but not the other way round (or vice versa),
  it's almost always a C14N mode / namespace-inheritance disagreement
  — both ends must agree on Exclusive vs Inclusive, comments on/off,
  and any `inclusive_prefixes` set.
