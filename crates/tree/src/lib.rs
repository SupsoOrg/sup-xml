//! Tree types for the SupXML document model.
//!
//! This crate defines the in-memory representation of an XML document —
//! the libxml2-shaped arena DOM in [`dom`].  You rarely need to depend on
//! this crate directly; everything is re-exported from the top-level
//! `sup-xml` crate.
//!
//! # Unsafe policy
//!
//! The [`dom`] module contains *contained* unsafe for the self-referential
//! `Document` wrapper (the `Document` owns a `Bump` and the root pointer into
//! that `Bump`).  See [`dom`] module docs for the safety argument.
#![deny(unsafe_code)]  // see CONTRIBUTING.md § "Unsafe policy" — `dom` module opts in locally

pub mod dict;
pub mod dom;

pub use dict::Dict;
pub use dom::{HtmlDoctype, HtmlMeta, QuirksMode, UnparsedEntity};
