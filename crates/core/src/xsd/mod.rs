//! XML Schema 1.0 — schema compiler and instance validator.
//!
//! This module is feature-gated behind `xsd`.  The public surface is small:
//!
//! ```ignore
//! use sup_xml_core::xsd::Schema;
//!
//! let schema  = Schema::compile_str(xsd_text)?;
//! schema.validate_str(instance_xml)?;
//! ```
//!
//! See `thoughts/xsd_plan.txt` in the repo for the full architecture and
//! scope.

#![forbid(unsafe_code)]  // see CONTRIBUTING.md § "Unsafe policy"

pub mod datetime;
pub mod dfa;
mod error;
mod facets;
pub mod identity;
mod lexical;
mod parser;
mod particle_restriction;
/// Re-exported for backwards compatibility — the regex engine
/// itself lives at [`crate::regex`] and is feature-independent.
pub use crate::regex;
mod resolver;
pub mod schema;
pub mod types;
mod assertion;
mod validate;
mod whitespace;

pub use error::{
    SchemaCompileError, ValidationError, ValidationIssue, ValidationKind, ValidationOptions,
};
pub use facets::{Facet, FacetSet, FacetViolation, TimezoneRequirement};
pub use identity::{ConstraintKind, IdentityConstraint, FieldPath, NameTest, PathExpr,
    PathStep, SelectorPath};
pub use resolver::{FsResolver, InMemoryResolver, NoResolver, SchemaResolver};
pub use schema::{
    AttributeDecl, AttributeGroup, AttributeUse, AttributeUseKind, BlockSet, ContentModel,
    ElementDecl, GroupKind, MaxOccurs, ModelGroup, NamespaceConstraint, NotationDecl, Particle,
    ProcessContents, QName, Schema, SchemaOptions, SchemaVersion, Term, TypeRef, Wildcard,
};
pub use types::{
    BuiltinType, ComplexType, Derivation, DerivationMethod, SimpleType, TypeDef, Value,
};
pub use whitespace::WhitespaceMode;
