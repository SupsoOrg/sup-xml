//! Stubs for libxml2 symbols that `XML::LibXML.bundle` references but
//! that we have not yet implemented behaviourally.
//!
//! macOS bundles built with chained fixups (the default since macOS 12)
//! resolve every imported symbol at load time — there is no lazy-binding
//! escape hatch. That means `use XML::LibXML` against our shim cannot
//! succeed unless every libxml2 symbol XML::LibXML links against exists
//! in our cdylib's export table, even ones the runtime never calls.
//!
//! The stubs here exist purely to satisfy dyld's load-time symbol
//! resolution. Each one:
//!
//! - Logs `record_call("name")` so `SUPXML_TRACE_STUBS=1` surfaces when
//!   a consumer actually invokes it.
//! - Returns the safest available default: `NULL` for pointer-returning
//!   functions, `-1` for int-returning functions whose convention treats
//!   negative as an error, and nothing for void-returning functions.
//!
//! When we real-implement one of these, move it out of this file into
//! the appropriate compat module (`tree.rs`, `parse.rs`, `xsd.rs`, …) so
//! this file's purpose stays clear: it's the to-do list, not the
//! reference implementation.
//!
//! Tracked by `tests/abi-system/perl/target_symbols.txt`.

#![allow(non_snake_case)]

use std::os::raw::c_int;

// htmlReadDoc / htmlReadIO live in `compat::html` alongside htmlReadMemory.

// xmlNewNode, xmlNewPI, xmlNewDocFragment live in `compat::mutate`
// (scratch-doc pattern that lifts libxml2's "no doc" idiom into our
// arena model).
// Tree mutation
// xmlCopyProp / xmlCopyNamespace / xmlAddSibling live in compat::mutate.

// xmlCharStrndup lives in `compat::strutil` alongside xmlStrdup.
// xmlEncodeEntitiesReentrant lives in `compat::encoding`.
// xmlSplitQName lives in compat::strutil alongside xmlSplitQName2/3.
// Buffer / encoding
// xmlBufferCreateStatic lives in compat::outbuf alongside xmlBufferCreate.
// xmlMemStrdup lives in compat::strutil alongside xmlStrdup.

// xmlCreateMemoryParserCtxt / xmlCreateFileParserCtxt live in `compat::parsectx`.
// xmlParserInputBufferCreateMem lives in `compat::misc`.
// xmlParseFile lives in `compat::parse`.
// xmlCtxtGetLastError lives in compat::error.

// xmlPatterncompile, xmlPatternMatch, xmlFreePattern, xmlPatternStreamable
// live in `compat::pattern`.

// xmlRegexpCompile / xmlRegexpExec / xmlRegFreeRegexp / xmlRegexpIsDeterminist
// live in `compat::regex`.

// xmlReaderForFd / xmlReaderForIO / xmlReaderWalker live in compat::reader.

// xmlSchemaNewMemParserCtxt lives in `compat::xsd`.
// xmlRelaxNGNewMemParserCtxt lives in compat::relaxng alongside
// xmlRelaxNGNew{Doc,}ParserCtxt.

// xmlHashCopy lives in compat::hash alongside xmlHashCreate.

// The xmlTextReader family lives in `compat::reader` (real impls).

// xmlNoNetExternalEntityLoader lives in compat::misc.

// xmlCharEncInFunc / xmlCharEncOutFunc live in compat::outbuf.
// xmlGcMemSetup lives in compat::misc (no-op by design).
// xmlGetDocCompressMode lives in compat::mutate alongside xmlSetDocCompressMode.
// xmlKeepBlanksDefault, xmlLoadCatalog, xmlRegisterInputCallbacks, and
// xmlParseBalancedChunkMemory live in `compat::misc` alongside the other
// global / input-plumbing helpers.
// xmlParseCharEncoding lives in `compat::encoding`.
// xmlParseDocument moved to compat::parsectx (real impl).
// htmlParseDocument shares xmlParseDocument's dispatch (parses through
// the HTML or XML reader based on which factory created the ctxt).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlParseDocument(
    ctxt: *mut crate::parsectx::XmlParserCtxt,
) -> c_int {
    unsafe { crate::parsectx::xmlParseDocument(ctxt) }
}
// xmlParserInputBufferPush lives in compat::misc.
// xmlPatternMatch moved to compat::pattern.
// xmlReconciliateNs lives in compat::mutate.
// xmlSaveFile / xmlSaveFormatFile / xmlSaveFormatFileTo live in compat::save.
// xmlTextConcat lives in compat::mutate.

// The xmlTextReader int-returners all live in compat::reader (real impls).

// xmlTextReaderByteConsumed moved to compat::reader.

// htmlDocDumpMemory lives in compat::html alongside htmlDocDumpMemoryFormat.
// xmlAttrSerializeTxtContent lives in compat::serialize.
// xmlCheckVersion / xmlCleanupInputCallbacks live in compat::misc (no-op by design).
// xmlFreePattern moved to compat::pattern.
// xmlFreeProp lives in compat::mutate.
// xmlInitializeCatalog lives in compat::misc (no-op by design).
// xmlRegFreeRegexp moved to compat::regex.
// xmlRegisterDefault{Input,Output}Callbacks live in compat::misc (no-op by design).
// xmlSchemaSetParserErrors / xmlSchemaSetValidErrors live in compat::xsd.
// xmlSetDocCompressMode lives in compat::mutate.
// xmlSetTreeDoc / xmlSetListDoc live in compat::mutate.
// xmlTextReaderGetErrorHandler / xmlTextReaderSetErrorHandler are now
// real impls in compat::reader (backed by per-reader handler storage).
