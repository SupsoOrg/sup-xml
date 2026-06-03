//! XSLT 1.0 transformation engine.
//!
//! Public API surface:
//!
//! ```
//! use sup_xml_xslt::Stylesheet;
//! use sup_xml_core::{parse_str, ParseOptions};
//!
//! let xslt = Stylesheet::compile_str(r#"<xsl:stylesheet version="1.0"
//!     xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
//!   <xsl:output method="xml" omit-xml-declaration="yes"/>
//!   <xsl:template match="/"><out><xsl:value-of select="/r"/></out></xsl:template>
//! </xsl:stylesheet>"#).unwrap();
//!
//! let src = parse_str("<r>hello</r>", &ParseOptions::default()).unwrap();
//! let result = xslt.apply(&src).unwrap();
//! assert_eq!(result.to_string().unwrap(), "<out>hello</out>");
//! ```
//!
//! The engine is built *on top of* `sup-xml-core`'s XPath
//! evaluator — XSLT match patterns and `select=` expressions parse
//! to the same AST, and template-body XPath calls reach the
//! engine's binding hook for the XSLT-added functions (`current()`,
//! `document()`, `key()`, `format-number()` etc.).  EXSLT functions
//! (math/date/str/set) are already always-on in the XPath engine,
//! so XSLT stylesheets that use them work without any additional
//! registration step.

#![forbid(unsafe_code)]

pub mod ast;
pub mod compiler;
pub mod error;
pub mod eval;
pub mod extensions;
pub mod format_number;
pub mod functions;
pub mod loader;
pub mod number;
pub mod output;
pub mod pattern;
pub mod result_tree;
pub mod schematron;
pub mod sort;
pub mod walk;
pub mod whitespace;

pub use loader::{FilesystemLoader, InMemoryLoader, Loader, NullLoader};

pub use ast::StylesheetAst;
pub use error::XsltError;
pub use extensions::{ExtensionFunctions, Extensions};
/// The XPath value type passed to extension functions and returned
/// by them.  Re-exported from `sup-xml-core` for convenience.
pub use sup_xml_core::xpath::XPathValue;

/// The XSLT namespace URI — all `xsl:*` instructions are
/// recognised by belonging to this URI.
pub const XSLT_NS: &str = "http://www.w3.org/1999/XSL/Transform";

/// A compiled XSLT 1.0 stylesheet, ready to apply to source
/// documents.  Compilation walks the stylesheet tree once,
/// pre-parses every XPath expression, and resolves
/// xsl:import/xsl:include chains.
///
/// A compiled XSLT 1.0 stylesheet.
#[derive(Debug)]
pub struct Stylesheet {
    /// The compiled AST — pre-parsed XPath expressions, decomposed
    /// AVTs, indexed templates.  Public to ease debugging /
    /// introspection from downstream tools; the apply() machinery
    /// is the supported public surface.
    pub ast: StylesheetAst,
}

impl Stylesheet {
    /// Compile a parsed stylesheet document into a reusable
    /// transformer.  Walks the stylesheet once, pre-parses every
    /// XPath expression in select/match/test/use attributes,
    /// decomposes Attribute Value Templates, and indexes templates
    /// for later pattern-matching.
    ///
    /// The supplied document MUST have been parsed in
    /// namespace-aware mode (`ParseOptions { namespace_aware: true,
    /// .. }`) — XSLT is fundamentally namespace-driven and the
    /// compiler detects `xsl:*` instructions via their namespace
    /// URI.  Use [`Stylesheet::compile_str`] for the common case
    /// where you have the stylesheet source as text and want the
    /// parse handled for you.
    pub fn compile(
        stylesheet_doc: &sup_xml_tree::dom::Document,
    ) -> Result<Self, XsltError> {
        let ast = compiler::compile(stylesheet_doc)?;
        Ok(Stylesheet { ast })
    }

    /// Parse + compile a stylesheet from source text.  Convenience
    /// wrapper that sets `namespace_aware: true` for you.
    ///
    /// Stylesheets that use `xsl:import` / `xsl:include` will
    /// successfully compile, but the imports' templates won't be
    /// loaded — references to imported templates surface as
    /// runtime errors.  For stylesheets that need import
    /// resolution, use [`Stylesheet::compile_str_with_loader`].
    pub fn compile_str(stylesheet_text: &str) -> Result<Self, XsltError> {
        Self::compile_str_with_loader(stylesheet_text, &loader::NullLoader, None)
    }

    /// Parse + compile + resolve imports.  Each `xsl:import` /
    /// `xsl:include` is followed via `loader`, recursively, with
    /// `base` supplying the URI to resolve relative hrefs against.
    ///
    /// Imported templates are merged into the resulting AST with
    /// lower [`Template::import_precedence`] than the importing
    /// stylesheet's own templates, so XSLT 1.0 §2.6.2 import
    /// precedence is honoured at pattern-match time.
    pub fn compile_str_with_loader(
        stylesheet_text: &str,
        loader:          &dyn Loader,
        base:            Option<&str>,
    ) -> Result<Self, XsltError> {
        let ast = compiler::compile_with_imports(
            stylesheet_text, loader, base,
            ast::StylesheetAst::default(),
            &mut 0,
        )?;
        Self::finalize(ast)
    }

    /// Compile with a package library available for `xsl:use-package`
    /// (XSLT 3.0 §3.5.1) — `packages` maps a package name to its
    /// (source text, base URI).  Imports/includes still resolve via
    /// `loader`.
    pub fn compile_str_with_packages(
        stylesheet_text: &str,
        loader:          &dyn Loader,
        base:            Option<&str>,
        packages: std::collections::HashMap<String, (String, Option<String>)>,
    ) -> Result<Self, XsltError> {
        let ast = compiler::compile_with_packages(stylesheet_text, loader, base, packages)?;
        Self::finalize(ast)
    }

    /// Shared post-compilation cleanup + validation for the
    /// `compile_str_with_*` entry points.
    fn finalize(mut ast: ast::StylesheetAst) -> Result<Self, XsltError> {
        // ASTs accumulated during recursive compilation might
        // double-count includes vs. imports for the same file —
        // dedup at the top level.
        ast.includes.sort();
        ast.includes.dedup();
        ast.imports.sort();
        ast.imports.dedup();
        ast.documents_to_load.sort();
        ast.documents_to_load.dedup();
        // XSLT 1.0 §7.1.4 (XTSE0710): every `use-attribute-sets`
        // reference must name a declared attribute-set.  Validate
        // at the end so cross-stylesheet (imported) declarations
        // are in scope.
        compiler::validate_attribute_set_refs(&ast)?;
        compiler::validate_named_template_uniqueness(&ast)?;
        // validate_global_variable_uniqueness is disabled: the
        // current Variable / Param structs don't carry an
        // import_precedence field, so the validator can't tell
        // apart "same precedence" duplicates (XTSE0630) from
        // legitimate shadowing from imports / includes.  Re-enable
        // once per-global precedence tracking lands.
        compiler::validate_output_declarations(&ast)?;
        compiler::validate_call_template_with_params(&ast)?;
        compiler::validate_iterate_constraints(&ast)?;
        compiler::validate_input_type_annotations(&ast)?;
        Ok(Stylesheet { ast })
    }

    /// Apply this stylesheet to `source_doc`, returning the
    /// materialised result tree.  Serialise via
    /// [`ResultTree::to_string`] (honours `<xsl:output method=…>`)
    /// or inspect [`ResultTree::children`] directly.
    pub fn apply(
        &self,
        source_doc: &sup_xml_tree::dom::Document,
    ) -> Result<ResultTree, XsltError> {
        eval::apply_stylesheet(&self.ast, source_doc)
    }

    /// Apply this stylesheet using `loader` to resolve any
    /// `document()` URIs the stylesheet references with string
    /// literals.  `base` supplies the base URI for relative-href
    /// resolution (passed through to [`Loader::load`]).
    ///
    /// Dynamic forms of `document()` — node-set arguments, the
    /// empty-string URI, or any expression that isn't a string
    /// literal — return a clear runtime error explaining the
    /// limitation.  Stylesheets that don't call `document()` at all
    /// behave identically to [`Stylesheet::apply`].
    pub fn apply_with_loader(
        &self,
        source_doc: &sup_xml_tree::dom::Document,
        loader:     &dyn Loader,
        base:       Option<&str>,
    ) -> Result<ResultTree, XsltError> {
        eval::apply_stylesheet_with_loader(&self.ast, source_doc, loader, base)
    }

    /// Apply this stylesheet with caller-supplied XPath extension
    /// functions registered via [`ExtensionFunctions`].
    ///
    /// Extension calls are dispatched via namespace + local name:
    /// when an XPath expression invokes `prefix:fname(…)`, the engine
    /// looks up `(namespace-uri(prefix), "fname")` in the supplied
    /// trait object.  Returning `Some(Ok(_))` provides the result;
    /// `Some(Err(_))` surfaces a runtime error; `None` falls through
    /// to the built-in EXSLT chain and finally to "unknown function".
    ///
    /// The most common use case is implemented by [`Extensions`],
    /// which lets callers register individual closures keyed by
    /// `(namespace, name)`.  For richer integrations (stateful
    /// dispatch, integration with an existing function registry,
    /// etc.) implement [`ExtensionFunctions`] on your own type.
    pub fn apply_with_extensions(
        &self,
        source_doc: &sup_xml_tree::dom::Document,
        extensions: &dyn ExtensionFunctions,
    ) -> Result<ResultTree, XsltError> {
        eval::apply_stylesheet_full(
            &self.ast, source_doc,
            &loader::NullLoader, None,
            Some(extensions),
        )
    }

    /// Apply this stylesheet with both a `document()` loader and
    /// caller-supplied XPath extension functions.  Combines
    /// [`Stylesheet::apply_with_loader`] and
    /// [`Stylesheet::apply_with_extensions`].
    pub fn apply_with_loader_and_extensions(
        &self,
        source_doc: &sup_xml_tree::dom::Document,
        loader:     &dyn Loader,
        base:       Option<&str>,
        extensions: &dyn ExtensionFunctions,
    ) -> Result<ResultTree, XsltError> {
        eval::apply_stylesheet_full(
            &self.ast, source_doc,
            loader, base,
            Some(extensions),
        )
    }

    /// Apply the stylesheet with caller-supplied overrides for
    /// top-level `xsl:param` declarations.  Each `(name, value)`
    /// pair replaces the matching param's default at apply time
    /// — XSLT 1.0 §11.4.  Match is by local-name; unmatched names
    /// are silently ignored.
    pub fn apply_with_params(
        &self,
        source_doc: &sup_xml_tree::dom::Document,
        loader:     &dyn Loader,
        base:       Option<&str>,
        params:     &[(String, String)],
    ) -> Result<ResultTree, XsltError> {
        eval::apply_stylesheet_full_with_params(
            &self.ast, source_doc,
            loader, base, None, params,
        )
    }

    /// Apply with both top-level params and a named-template entry
    /// point.  `initial_template` selects an `xsl:template name="…"`
    /// declaration to call instead of doing the default
    /// apply-templates dispatch on the document root.  This matches
    /// the XSLT 3.0 named-entry convention used by W3C test
    /// harnesses via `<initial-template name="go"/>`.
    pub fn apply_with_params_and_initial(
        &self,
        source_doc:        &sup_xml_tree::dom::Document,
        loader:            &dyn Loader,
        base:              Option<&str>,
        params:            &[(String, String)],
        initial_template:  Option<&str>,
    ) -> Result<ResultTree, XsltError> {
        eval::apply_stylesheet_full_with_params_and_initial(
            &self.ast, source_doc,
            loader, base, None, params, initial_template, None,
        )
    }

    /// Apply with top-level params, an optional named-template entry
    /// point, and an optional initial mode for the default
    /// `apply-templates` dispatch.  XSLT 3.0 / W3C test conventions
    /// pass `<initial-mode name="X"/>` so the entry-point dispatch
    /// sees the named mode rather than the unnamed default.
    pub fn apply_with_params_initial_and_mode(
        &self,
        source_doc:        &sup_xml_tree::dom::Document,
        loader:            &dyn Loader,
        base:              Option<&str>,
        params:            &[(String, String)],
        initial_template:  Option<&str>,
        initial_mode:      Option<&str>,
    ) -> Result<ResultTree, XsltError> {
        eval::apply_stylesheet_full_with_params_and_initial(
            &self.ast, source_doc,
            loader, base, None, params, initial_template, initial_mode,
        )
    }
}

pub use result_tree::ResultTree;
