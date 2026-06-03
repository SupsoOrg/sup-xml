//! Stubs for libxml2 / libxslt symbols that PHP's XML extensions
//! reference but that we have not yet implemented behaviourally.
//!
//! Same shape as [`crate::perl_stubs`]: each entry is here purely to
//! satisfy dyld's load-time symbol resolution under macOS chained
//! fixups.  When a consumer actually calls into one we log the name
//! via `record_call("…")` (visible with `SUPXML_TRACE_STUBS=1`) and
//! return a safe default (NULL / 0 / -1 / void).
//!
//! Real implementations get moved out of this file into their proper
//! compat module (`tree.rs`, `xsd.rs`, `xslt.rs`, ...) over time.

#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

use std::os::raw::c_int;

// ── libxslt int data symbols (variables, not functions) ────────────────────
//
// libxslt declares these as `extern int` globals (xsltLibxmlVersion as
// `const int`, xsltMaxVars as mutable int).  PHP's xsl extension reads
// them by value at module init.  Exposing as `pub static` ints lets
// the linker resolve the symbol; reading them returns the documented
// defaults.

/// `xsltLibxmlVersion` — the libxml2 version number xslt was compiled
/// against (`XML_VERSION_NUMERIC`).  PHP's xsl extension reads this at
/// module init.  Value matches what we report from
/// `__xmlParserVersion` (libxml2 2.9.13 numerically).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub static xsltLibxmlVersion: c_int = 20913;

/// `xsltMaxVars` — soft cap on the number of XSLT variables tracked
/// per template scope.  libxslt default is 15000; PHP's xsl extension
/// reads this once at init.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub static xsltMaxVars: c_int = 15000;

// htmlCreateFileParserCtxt, htmlGetMetaEncoding, htmlDocDumpMemoryFormat,
// and htmlSaveFileFormat live in compat::html (real impls).

// The xmlTextWriter family lives in `compat::xmlwriter` (real impls).

// xmlBuildQName, xmlSplitQName3, xmlCharStrdup, xmlUTF8Strsub live in
// compat::strutil.  xmlEncodeSpecialChars, xmlGetPredefinedEntity live
// in compat::encoding.
// xmlNewDocTextLen lives in compat::mutate.
// xmlPathToURI, xmlCanonicPath live in compat::uri.
// xmlAddChildList lives in compat::misc.

// xmlNewTextReader, xmlTextReaderReadString, xmlTextReaderSetup
// live in compat::reader (real impls).

// xmlOutputBufferCreateBuffer / GetContent live in compat::outbuf (real impls).
// xmlOutputBufferCreateFilenameDefault, xmlParserInputBufferCreateFilenameDefault
// live in compat::misc.

// xmlParserInputBufferCreateIO lives in compat::misc alongside
// xmlParserInputBufferCreateMem.

// xmlReadFile lives in compat::parse.

// xmlTextReaderReadString lives in compat::reader.

// xsltXPathGetTransformContext lives in compat::xslt.

// xmlDictSetLimit lives in compat::misc (returns size_t, not ptr).

// HTML
// htmlDocDumpMemoryFormat, htmlSaveFileFormat live in compat::html.
// htmlParseDocument moved to compat::perl_stubs (real wrapper around xmlParseDocument).

// All xmlTextWriter Start/End/Write* methods now live in compat::xmlwriter.

// xmlTextReaderSetup lives in compat::reader.

// Misc xml.  xmlGetUTF8Char lives in compat::strutil;
// xmlLineNumbersDefault / xmlPedanticParserDefault /
// xmlSubstituteEntitiesDefault in compat::misc; xmlOutputBufferGetSize
// in compat::outbuf.
// xmlCopyError, xmlSwitchToEncoding live in compat::misc.
// xmlParseURIReference lives in compat::uri.
// xmlSaveFormatFileEnc lives in compat::save.
// xmlDOMWrapAdoptNode lives in compat::mutate (delegates to adopt_subtree).
// xmlC14NExecute is implemented in compat::c14n.

/// `xmlValidateName(value, space)` — XML 1.0 Name production
/// validation.  Returns 0 if valid, non-zero if invalid.  PHP's
/// DOMDocument calls this on attribute / element name strings before
/// applying them; a -1 (invalid) stub return makes PHP throw
/// `DOMException: Invalid Character Error` for every name.
///
/// We delegate to `compat::strutil::xmlValidateNameValue` which
/// already implements the full Name production check; the libxml2
/// `space` parameter (allows leading/trailing whitespace) is
/// currently ignored, matching the no-whitespace fast path.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlValidateName(
    value: *const std::os::raw::c_char,
    _space: c_int,
) -> c_int {
    if value.is_null() { return 1; }
    // SAFETY: caller asserts NUL-terminated.
    // xmlValidateNameValue returns 1 if valid, 0 if invalid (opposite
    // convention!).  Translate.
    let ok = unsafe { crate::strutil::xmlValidateNameValue(value) };
    if ok == 1 { 0 } else { 1 }
}

// xsltRegisterExtModuleFunction / xsltUnregisterExtModuleFunction /
// xsltSaveResultToFilename live in compat::xslt.

// htmlFreeParserCtxt, xmlFreeParserInputBuffer, xmlStopParser live in
// compat::misc (real impls).
// xmlFreeTextWriter moved to compat::xmlwriter.
// xmlNodeSetContentLen, xmlNodeSetLang live in compat::mutate.
// xmlRelaxNG{CleanupTypes,SetParserErrors,SetValidErrors} live in compat::relaxng.
// xmlStopParser moved to compat::misc.
