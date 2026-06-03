/* T-UPSTREAM-LAYOUT: byte-exact layout check against the *actual
 * installed* libxml2 headers — not a local re-typedef.
 *
 * Why this exists separately from t-layout-03 / t-layout-04 / t-err-03:
 * those tests declare their own `typedef struct _xmlError { ... }`
 * (and friends), then assert offsets against those local typedefs.
 * That catches "compat's Rust struct disagrees with compat's expected
 * layout" — useful — but it does NOT catch the case the user actually
 * worries about: libxml2 upstream changing their public struct in a
 * future release, with our compat layer still vending the old
 * offsets.  When that happens, every C tree-walker compiled against
 * the new libxml2 headers but linked against `libsup_xml_compat`
 * reads garbage.
 *
 * To close that gap this test #includes the real libxml2 headers and
 * runs `_Static_assert` on offsets / sizes / enum discriminants of
 * every upstream type our compat ABI commits to.  Compat is pinned
 * to libxml2 2.9.13 and 2.15.3 (the versions we've verified).  We
 * make no forward-compatibility claim: if a future libxml2 release
 * appends fields, renumbers an enum, or otherwise drifts from those
 * pinned layouts, the build of this test fails on a host that has
 * the newer headers installed.  That's the alarm — compat then
 * needs a coordinated bump.  It is NOT auto-detection of "what
 * libxml2 happens to ship today"; it's a regression check against
 * an explicit version envelope.
 *
 * Coverage in this file is kept to the structs C consumers read
 * fields out of via the documented public layout.  Opaque-pointer
 * types (xmlParserCtxt, xmlValidCtxt, xmlSchemaParserCtxt, …) are
 * intentionally NOT covered — callers go through accessor functions
 * for those, so the size/layout doesn't matter to the ABI contract.
 *
 * Sections (in declaration order):
 *
 *   xmlError                 — error.rs                     [stable]
 *   xmlElementType           — node-type discriminants 1..20
 *   _xmlNs                   — tree/dom.rs Namespace
 *   _xmlNode                 — tree/dom.rs Node
 *   _xmlAttr                 — tree/dom.rs Attribute
 *   _xmlDoc                  — tree/dom.rs XmlDoc
 *   _xmlDtd                  — compat/dtd.rs xmlDtd
 *   _xmlURI                  — compat/uri.rs xmlURI
 *   _xmlBuffer               — compat/outbuf.rs xmlBuffer
 *   _xmlOutputBuffer         — compat/outbuf.rs xmlOutputBuffer
 *   _xmlNodeSet              — compat/xpath.rs xmlNodeSet
 *   _xmlXPathObject          — compat/xpath.rs xmlXPathObject
 *   _xmlXPathContext         — compat/xpath.rs xmlXPathContext  [big]
 */

#include <stdio.h>
#include <stddef.h>

#include <libxml/xmlversion.h>
#include <libxml/xmlerror.h>
#include <libxml/tree.h>
#include <libxml/uri.h>
#include <libxml/xmlIO.h>
#include <libxml/xpath.h>

/* ── xmlError ─────────────────────────────────────────────────────── */

_Static_assert(offsetof(xmlError, domain)  ==  0, "xmlError::domain @ 0");
_Static_assert(offsetof(xmlError, code)    ==  4, "xmlError::code @ 4");
_Static_assert(offsetof(xmlError, message) ==  8, "xmlError::message @ 8");
_Static_assert(offsetof(xmlError, level)   == 16, "xmlError::level @ 16");
_Static_assert(offsetof(xmlError, file)    == 24, "xmlError::file @ 24");
_Static_assert(offsetof(xmlError, line)    == 32, "xmlError::line @ 32");
_Static_assert(offsetof(xmlError, str1)    == 40, "xmlError::str1 @ 40");
_Static_assert(offsetof(xmlError, str2)    == 48, "xmlError::str2 @ 48");
_Static_assert(offsetof(xmlError, str3)    == 56, "xmlError::str3 @ 56");
_Static_assert(offsetof(xmlError, int1)    == 64, "xmlError::int1 @ 64");
_Static_assert(offsetof(xmlError, int2)    == 68, "xmlError::int2 @ 68");
_Static_assert(offsetof(xmlError, ctxt)    == 72, "xmlError::ctxt @ 72");
_Static_assert(offsetof(xmlError, node)    == 80, "xmlError::node @ 80");
_Static_assert(sizeof(xmlError)            == 88, "sizeof(xmlError) == 88");

/* `xmlErrorLevel` is declared as an enum.  -fshort-enums could
 * narrow it to 1 byte, shifting every field after it.  Pin int. */
_Static_assert(sizeof(xmlErrorLevel) == sizeof(int),
               "xmlErrorLevel must be int-width (no -fshort-enums)");

/* ── xmlElementType discriminants ────────────────────────────────── */

_Static_assert(XML_ELEMENT_NODE       ==  1, "XML_ELEMENT_NODE == 1");
_Static_assert(XML_ATTRIBUTE_NODE     ==  2, "XML_ATTRIBUTE_NODE == 2");
_Static_assert(XML_TEXT_NODE          ==  3, "XML_TEXT_NODE == 3");
_Static_assert(XML_CDATA_SECTION_NODE ==  4, "XML_CDATA_SECTION_NODE == 4");
_Static_assert(XML_ENTITY_REF_NODE    ==  5, "XML_ENTITY_REF_NODE == 5");
_Static_assert(XML_ENTITY_NODE        ==  6, "XML_ENTITY_NODE == 6");
_Static_assert(XML_PI_NODE            ==  7, "XML_PI_NODE == 7");
_Static_assert(XML_COMMENT_NODE       ==  8, "XML_COMMENT_NODE == 8");
_Static_assert(XML_DOCUMENT_NODE      ==  9, "XML_DOCUMENT_NODE == 9");
_Static_assert(XML_DOCUMENT_TYPE_NODE == 10, "XML_DOCUMENT_TYPE_NODE == 10");
_Static_assert(XML_DOCUMENT_FRAG_NODE == 11, "XML_DOCUMENT_FRAG_NODE == 11");
_Static_assert(XML_NOTATION_NODE      == 12, "XML_NOTATION_NODE == 12");
_Static_assert(XML_HTML_DOCUMENT_NODE == 13, "XML_HTML_DOCUMENT_NODE == 13");
_Static_assert(XML_DTD_NODE           == 14, "XML_DTD_NODE == 14");
_Static_assert(XML_ELEMENT_DECL       == 15, "XML_ELEMENT_DECL == 15");
_Static_assert(XML_ATTRIBUTE_DECL     == 16, "XML_ATTRIBUTE_DECL == 16");
_Static_assert(XML_ENTITY_DECL        == 17, "XML_ENTITY_DECL == 17");
_Static_assert(XML_NAMESPACE_DECL     == 18, "XML_NAMESPACE_DECL == 18");
_Static_assert(XML_XINCLUDE_START     == 19, "XML_XINCLUDE_START == 19");
_Static_assert(XML_XINCLUDE_END       == 20, "XML_XINCLUDE_END == 20");

_Static_assert(sizeof(xmlElementType) == sizeof(int),
               "xmlElementType must be int-width (no -fshort-enums)");

/* ── _xmlNs ───────────────────────────────────────────────────────
 * Cross-checked vs Namespace in crates/tree/src/dom.rs:300.
 * One of the few public libxml2 structs that DOESN'T put `_private`
 * at offset 0 — `next` is first instead.  Easy to get wrong by
 * analogy with xmlNode/xmlDoc/xmlAttr.
 */

_Static_assert(offsetof(xmlNs, next)     ==  0, "xmlNs::next @ 0");
_Static_assert(offsetof(xmlNs, type)     ==  8, "xmlNs::type @ 8");
_Static_assert(offsetof(xmlNs, href)     == 16, "xmlNs::href @ 16");
_Static_assert(offsetof(xmlNs, prefix)   == 24, "xmlNs::prefix @ 24");
_Static_assert(offsetof(xmlNs, _private) == 32, "xmlNs::_private @ 32");
_Static_assert(offsetof(xmlNs, context)  == 40, "xmlNs::context @ 40");
_Static_assert(sizeof(xmlNs)             == 48, "sizeof(xmlNs) == 48");

/* ── _xmlNode ─────────────────────────────────────────────────────
 * Cross-checked vs Node in crates/tree/src/dom.rs:541.
 * The hot path: every generic tree walker (xpath, xinclude, c14n,
 * lxml's element iterators) reads `name`, `children`, `next`,
 * `parent`, `doc`, `ns`, `content`, `properties`.  ABI drift here
 * silently corrupts every consumer.
 */

_Static_assert(offsetof(xmlNode, _private)   ==   0, "xmlNode::_private @ 0");
_Static_assert(offsetof(xmlNode, type)       ==   8, "xmlNode::type @ 8");
_Static_assert(offsetof(xmlNode, name)       ==  16, "xmlNode::name @ 16");
_Static_assert(offsetof(xmlNode, children)   ==  24, "xmlNode::children @ 24");
_Static_assert(offsetof(xmlNode, last)       ==  32, "xmlNode::last @ 32");
_Static_assert(offsetof(xmlNode, parent)     ==  40, "xmlNode::parent @ 40");
_Static_assert(offsetof(xmlNode, next)       ==  48, "xmlNode::next @ 48");
_Static_assert(offsetof(xmlNode, prev)       ==  56, "xmlNode::prev @ 56");
_Static_assert(offsetof(xmlNode, doc)        ==  64, "xmlNode::doc @ 64");
_Static_assert(offsetof(xmlNode, ns)         ==  72, "xmlNode::ns @ 72");
_Static_assert(offsetof(xmlNode, content)    ==  80, "xmlNode::content @ 80");
_Static_assert(offsetof(xmlNode, properties) ==  88, "xmlNode::properties @ 88");
_Static_assert(offsetof(xmlNode, nsDef)      ==  96, "xmlNode::nsDef @ 96");
_Static_assert(offsetof(xmlNode, psvi)       == 104, "xmlNode::psvi @ 104");
_Static_assert(offsetof(xmlNode, line)       == 112, "xmlNode::line @ 112");
_Static_assert(offsetof(xmlNode, extra)      == 114, "xmlNode::extra @ 114");
_Static_assert(sizeof(xmlNode)               == 120, "sizeof(xmlNode) == 120");

/* ── _xmlAttr ────────────────────────────────────────────────────
 * Cross-checked vs Attribute in crates/tree/src/dom.rs:402.
 * Shares its first 8 fields with `_xmlNode` so generic walkers can
 * cast `xmlAttr*` → `xmlNode*` and read `.type` / `.name`.  Drift
 * here breaks every attribute walker.
 */

_Static_assert(offsetof(xmlAttr, _private) ==  0, "xmlAttr::_private @ 0");
_Static_assert(offsetof(xmlAttr, type)     ==  8, "xmlAttr::type @ 8");
_Static_assert(offsetof(xmlAttr, name)     == 16, "xmlAttr::name @ 16");
_Static_assert(offsetof(xmlAttr, children) == 24, "xmlAttr::children @ 24");
_Static_assert(offsetof(xmlAttr, last)     == 32, "xmlAttr::last @ 32");
_Static_assert(offsetof(xmlAttr, parent)   == 40, "xmlAttr::parent @ 40");
_Static_assert(offsetof(xmlAttr, next)     == 48, "xmlAttr::next @ 48");
_Static_assert(offsetof(xmlAttr, prev)     == 56, "xmlAttr::prev @ 56");
_Static_assert(offsetof(xmlAttr, doc)      == 64, "xmlAttr::doc @ 64");
_Static_assert(offsetof(xmlAttr, ns)       == 72, "xmlAttr::ns @ 72");
_Static_assert(offsetof(xmlAttr, atype)    == 80, "xmlAttr::atype @ 80");
_Static_assert(offsetof(xmlAttr, psvi)     == 88, "xmlAttr::psvi @ 88");
/* The trailing `xmlID *id` field was added on master (xml-2.14-dev)
 * — we don't commit to its presence.  sizeof check would be fragile
 * across libxml2 versions; we trust that the prefix-through-psvi
 * matching is sufficient. */

/* ── _xmlDoc ─────────────────────────────────────────────────────
 * Cross-checked vs XmlDoc in crates/tree/src/dom.rs:2270 and
 * t-layout-03.c's locally-typedef'd version.
 */

_Static_assert(offsetof(xmlDoc, _private)    ==   0, "xmlDoc::_private @ 0");
_Static_assert(offsetof(xmlDoc, type)        ==   8, "xmlDoc::type @ 8");
_Static_assert(offsetof(xmlDoc, name)        ==  16, "xmlDoc::name @ 16");
_Static_assert(offsetof(xmlDoc, children)    ==  24, "xmlDoc::children @ 24");
_Static_assert(offsetof(xmlDoc, last)        ==  32, "xmlDoc::last @ 32");
_Static_assert(offsetof(xmlDoc, parent)      ==  40, "xmlDoc::parent @ 40");
_Static_assert(offsetof(xmlDoc, next)        ==  48, "xmlDoc::next @ 48");
_Static_assert(offsetof(xmlDoc, prev)        ==  56, "xmlDoc::prev @ 56");
_Static_assert(offsetof(xmlDoc, doc)         ==  64, "xmlDoc::doc @ 64");
_Static_assert(offsetof(xmlDoc, compression) ==  72, "xmlDoc::compression @ 72");
_Static_assert(offsetof(xmlDoc, standalone)  ==  76, "xmlDoc::standalone @ 76");
_Static_assert(offsetof(xmlDoc, intSubset)   ==  80, "xmlDoc::intSubset @ 80");
_Static_assert(offsetof(xmlDoc, extSubset)   ==  88, "xmlDoc::extSubset @ 88");
_Static_assert(offsetof(xmlDoc, oldNs)       ==  96, "xmlDoc::oldNs @ 96");
_Static_assert(offsetof(xmlDoc, version)     == 104, "xmlDoc::version @ 104");
_Static_assert(offsetof(xmlDoc, encoding)    == 112, "xmlDoc::encoding @ 112");
_Static_assert(offsetof(xmlDoc, ids)         == 120, "xmlDoc::ids @ 120");
_Static_assert(offsetof(xmlDoc, refs)        == 128, "xmlDoc::refs @ 128");
_Static_assert(offsetof(xmlDoc, URL)         == 136, "xmlDoc::URL @ 136");
_Static_assert(offsetof(xmlDoc, charset)     == 144, "xmlDoc::charset @ 144");
_Static_assert(offsetof(xmlDoc, dict)        == 152, "xmlDoc::dict @ 152");
_Static_assert(offsetof(xmlDoc, psvi)        == 160, "xmlDoc::psvi @ 160");
_Static_assert(offsetof(xmlDoc, parseFlags)  == 168, "xmlDoc::parseFlags @ 168");
_Static_assert(offsetof(xmlDoc, properties)  == 172, "xmlDoc::properties @ 172");
_Static_assert(sizeof(xmlDoc)                == 176, "sizeof(xmlDoc) == 176");

/* ── _xmlDtd ─────────────────────────────────────────────────────
 * Cross-checked vs xmlDtd in crates/compat/src/dtd.rs:140.
 * lxml's `_dtdFactory` reads ExternalID/SystemID via direct field
 * access.  Drift breaks `tree.docinfo.internalDTD`.
 */

_Static_assert(offsetof(xmlDtd, _private)   ==   0, "xmlDtd::_private @ 0");
_Static_assert(offsetof(xmlDtd, type)       ==   8, "xmlDtd::type @ 8");
_Static_assert(offsetof(xmlDtd, name)       ==  16, "xmlDtd::name @ 16");
_Static_assert(offsetof(xmlDtd, children)   ==  24, "xmlDtd::children @ 24");
_Static_assert(offsetof(xmlDtd, last)       ==  32, "xmlDtd::last @ 32");
_Static_assert(offsetof(xmlDtd, parent)     ==  40, "xmlDtd::parent @ 40");
_Static_assert(offsetof(xmlDtd, next)       ==  48, "xmlDtd::next @ 48");
_Static_assert(offsetof(xmlDtd, prev)       ==  56, "xmlDtd::prev @ 56");
_Static_assert(offsetof(xmlDtd, doc)        ==  64, "xmlDtd::doc @ 64");
_Static_assert(offsetof(xmlDtd, notations)  ==  72, "xmlDtd::notations @ 72");
_Static_assert(offsetof(xmlDtd, elements)   ==  80, "xmlDtd::elements @ 80");
_Static_assert(offsetof(xmlDtd, attributes) ==  88, "xmlDtd::attributes @ 88");
_Static_assert(offsetof(xmlDtd, entities)   ==  96, "xmlDtd::entities @ 96");
_Static_assert(offsetof(xmlDtd, ExternalID) == 104, "xmlDtd::ExternalID @ 104");
_Static_assert(offsetof(xmlDtd, SystemID)   == 112, "xmlDtd::SystemID @ 112");
_Static_assert(offsetof(xmlDtd, pentities)  == 120, "xmlDtd::pentities @ 120");
_Static_assert(sizeof(xmlDtd)               == 128, "sizeof(xmlDtd) == 128");

/* ── _xmlURI ─────────────────────────────────────────────────────
 * Cross-checked vs xmlURI in crates/compat/src/uri.rs:18.
 */

_Static_assert(offsetof(xmlURI, scheme)    ==  0, "xmlURI::scheme @ 0");
_Static_assert(offsetof(xmlURI, opaque)    ==  8, "xmlURI::opaque @ 8");
_Static_assert(offsetof(xmlURI, authority) == 16, "xmlURI::authority @ 16");
_Static_assert(offsetof(xmlURI, server)    == 24, "xmlURI::server @ 24");
_Static_assert(offsetof(xmlURI, user)      == 32, "xmlURI::user @ 32");
_Static_assert(offsetof(xmlURI, port)      == 40, "xmlURI::port @ 40");
_Static_assert(offsetof(xmlURI, path)      == 48, "xmlURI::path @ 48");
_Static_assert(offsetof(xmlURI, query)     == 56, "xmlURI::query @ 56");
_Static_assert(offsetof(xmlURI, fragment)  == 64, "xmlURI::fragment @ 64");
_Static_assert(offsetof(xmlURI, cleanup)   == 72, "xmlURI::cleanup @ 72");
_Static_assert(offsetof(xmlURI, query_raw) == 80, "xmlURI::query_raw @ 80");
_Static_assert(sizeof(xmlURI)              == 88, "sizeof(xmlURI) == 88");

/* ── _xmlBuffer ──────────────────────────────────────────────────
 * Cross-checked vs xmlBuffer in crates/compat/src/outbuf.rs:18.
 * libxml2's field is named `use` — a Rust keyword; compat
 * mirrors it as `use_`.
 */

_Static_assert(offsetof(xmlBuffer, content)    ==  0, "xmlBuffer::content @ 0");
_Static_assert(offsetof(xmlBuffer, use)        ==  8, "xmlBuffer::use @ 8");
_Static_assert(offsetof(xmlBuffer, size)       == 12, "xmlBuffer::size @ 12");
_Static_assert(offsetof(xmlBuffer, alloc)      == 16, "xmlBuffer::alloc @ 16");
_Static_assert(offsetof(xmlBuffer, contentIO)  == 24, "xmlBuffer::contentIO @ 24");
_Static_assert(sizeof(xmlBuffer)               == 32, "sizeof(xmlBuffer) == 32");

/* ── _xmlOutputBuffer ────────────────────────────────────────────
 * Cross-checked vs xmlOutputBuffer in crates/compat/src/outbuf.rs:38.
 */

_Static_assert(offsetof(xmlOutputBuffer, context)       ==  0, "xmlOutputBuffer::context @ 0");
_Static_assert(offsetof(xmlOutputBuffer, writecallback) ==  8, "xmlOutputBuffer::writecallback @ 8");
_Static_assert(offsetof(xmlOutputBuffer, closecallback) == 16, "xmlOutputBuffer::closecallback @ 16");
_Static_assert(offsetof(xmlOutputBuffer, encoder)       == 24, "xmlOutputBuffer::encoder @ 24");
_Static_assert(offsetof(xmlOutputBuffer, buffer)        == 32, "xmlOutputBuffer::buffer @ 32");
_Static_assert(offsetof(xmlOutputBuffer, conv)          == 40, "xmlOutputBuffer::conv @ 40");
_Static_assert(offsetof(xmlOutputBuffer, written)       == 48, "xmlOutputBuffer::written @ 48");
_Static_assert(offsetof(xmlOutputBuffer, error)         == 52, "xmlOutputBuffer::error @ 52");
_Static_assert(sizeof(xmlOutputBuffer)                  == 56, "sizeof(xmlOutputBuffer) == 56");

/* ── _xmlNodeSet ─────────────────────────────────────────────────
 * Cross-checked vs xmlNodeSet in crates/compat/src/xpath.rs:57.
 */

_Static_assert(offsetof(xmlNodeSet, nodeNr)  == 0, "xmlNodeSet::nodeNr @ 0");
_Static_assert(offsetof(xmlNodeSet, nodeMax) == 4, "xmlNodeSet::nodeMax @ 4");
_Static_assert(offsetof(xmlNodeSet, nodeTab) == 8, "xmlNodeSet::nodeTab @ 8");
_Static_assert(sizeof(xmlNodeSet)            == 16, "sizeof(xmlNodeSet) == 16");

/* ── _xmlXPathObject ─────────────────────────────────────────────
 * Cross-checked vs xmlXPathObject in crates/compat/src/xpath.rs:71.
 */

_Static_assert(offsetof(xmlXPathObject, type)       ==  0, "xmlXPathObject::type @ 0");
_Static_assert(offsetof(xmlXPathObject, nodesetval) ==  8, "xmlXPathObject::nodesetval @ 8");
_Static_assert(offsetof(xmlXPathObject, boolval)    == 16, "xmlXPathObject::boolval @ 16");
_Static_assert(offsetof(xmlXPathObject, floatval)   == 24, "xmlXPathObject::floatval @ 24");
_Static_assert(offsetof(xmlXPathObject, stringval)  == 32, "xmlXPathObject::stringval @ 32");
_Static_assert(offsetof(xmlXPathObject, user)       == 40, "xmlXPathObject::user @ 40");
_Static_assert(offsetof(xmlXPathObject, index)      == 48, "xmlXPathObject::index @ 48");
_Static_assert(offsetof(xmlXPathObject, user2)      == 56, "xmlXPathObject::user2 @ 56");
_Static_assert(offsetof(xmlXPathObject, index2)     == 64, "xmlXPathObject::index2 @ 64");
_Static_assert(sizeof(xmlXPathObject)               == 72, "sizeof(xmlXPathObject) == 72");

/* ── _xmlXPathContext ────────────────────────────────────────────
 * Cross-checked vs xmlXPathContext in crates/compat/src/xpath.rs:115.
 *
 * This is the biggest libxml2 struct compat mirrors at byte-exact
 * layout (~30 fields, 88-byte embedded `xmlError lastError`).  It's
 * also the most volatile: libxml2 added the resource-limit fields
 * (opLimit/opCount/depth) in 2.10 behind a `#ifdef
 * LIBXML_HAS_XPATH_RESOURCE_LIMITS` guard, then dropped the ifdef
 * in 2.13+ so the fields are unconditionally present.
 *
 * Strategy: assert offsets of every field up through `cache`
 * unconditionally — those are at identical offsets in libxml2
 * 2.9.13 and 2.15.3, the versions we verify against.  Assert the
 * tail fields only when the compiler can see them — either the
 * macro is defined OR LIBXML_VERSION >= 21300 (where the ifdef was
 * removed and the fields are always declared).
 */

_Static_assert(offsetof(xmlXPathContext, doc)                  ==   0, "xmlXPathContext::doc @ 0");
_Static_assert(offsetof(xmlXPathContext, node)                 ==   8, "xmlXPathContext::node @ 8");
_Static_assert(offsetof(xmlXPathContext, nb_variables_unused)  ==  16, "xmlXPathContext::nb_variables_unused @ 16");
_Static_assert(offsetof(xmlXPathContext, max_variables_unused) ==  20, "xmlXPathContext::max_variables_unused @ 20");
_Static_assert(offsetof(xmlXPathContext, varHash)              ==  24, "xmlXPathContext::varHash @ 24");
_Static_assert(offsetof(xmlXPathContext, nb_types)             ==  32, "xmlXPathContext::nb_types @ 32");
_Static_assert(offsetof(xmlXPathContext, max_types)            ==  36, "xmlXPathContext::max_types @ 36");
_Static_assert(offsetof(xmlXPathContext, types)                ==  40, "xmlXPathContext::types @ 40");
_Static_assert(offsetof(xmlXPathContext, nb_funcs_unused)      ==  48, "xmlXPathContext::nb_funcs_unused @ 48");
_Static_assert(offsetof(xmlXPathContext, max_funcs_unused)     ==  52, "xmlXPathContext::max_funcs_unused @ 52");
_Static_assert(offsetof(xmlXPathContext, funcHash)             ==  56, "xmlXPathContext::funcHash @ 56");
_Static_assert(offsetof(xmlXPathContext, nb_axis)              ==  64, "xmlXPathContext::nb_axis @ 64");
_Static_assert(offsetof(xmlXPathContext, max_axis)             ==  68, "xmlXPathContext::max_axis @ 68");
_Static_assert(offsetof(xmlXPathContext, axis)                 ==  72, "xmlXPathContext::axis @ 72");
_Static_assert(offsetof(xmlXPathContext, namespaces)           ==  80, "xmlXPathContext::namespaces @ 80");
_Static_assert(offsetof(xmlXPathContext, nsNr)                 ==  88, "xmlXPathContext::nsNr @ 88");
_Static_assert(offsetof(xmlXPathContext, user)                 ==  96, "xmlXPathContext::user @ 96");
_Static_assert(offsetof(xmlXPathContext, contextSize)          == 104, "xmlXPathContext::contextSize @ 104");
_Static_assert(offsetof(xmlXPathContext, proximityPosition)    == 108, "xmlXPathContext::proximityPosition @ 108");
_Static_assert(offsetof(xmlXPathContext, xptr)                 == 112, "xmlXPathContext::xptr @ 112");
_Static_assert(offsetof(xmlXPathContext, here)                 == 120, "xmlXPathContext::here @ 120");
_Static_assert(offsetof(xmlXPathContext, origin)               == 128, "xmlXPathContext::origin @ 128");
_Static_assert(offsetof(xmlXPathContext, nsHash)               == 136, "xmlXPathContext::nsHash @ 136");
_Static_assert(offsetof(xmlXPathContext, varLookupFunc)        == 144, "xmlXPathContext::varLookupFunc @ 144");
_Static_assert(offsetof(xmlXPathContext, varLookupData)        == 152, "xmlXPathContext::varLookupData @ 152");
_Static_assert(offsetof(xmlXPathContext, extra)                == 160, "xmlXPathContext::extra @ 160");
_Static_assert(offsetof(xmlXPathContext, function)             == 168, "xmlXPathContext::function @ 168");
_Static_assert(offsetof(xmlXPathContext, functionURI)          == 176, "xmlXPathContext::functionURI @ 176");
_Static_assert(offsetof(xmlXPathContext, funcLookupFunc)       == 184, "xmlXPathContext::funcLookupFunc @ 184");
_Static_assert(offsetof(xmlXPathContext, funcLookupData)       == 192, "xmlXPathContext::funcLookupData @ 192");
_Static_assert(offsetof(xmlXPathContext, tmpNsList)            == 200, "xmlXPathContext::tmpNsList @ 200");
_Static_assert(offsetof(xmlXPathContext, tmpNsNr)              == 208, "xmlXPathContext::tmpNsNr @ 208");
_Static_assert(offsetof(xmlXPathContext, userData)             == 216, "xmlXPathContext::userData @ 216");
_Static_assert(offsetof(xmlXPathContext, error)                == 224, "xmlXPathContext::error @ 224");
_Static_assert(offsetof(xmlXPathContext, lastError)            == 232, "xmlXPathContext::lastError @ 232");
_Static_assert(offsetof(xmlXPathContext, debugNode)            == 320, "xmlXPathContext::debugNode @ 320");
_Static_assert(offsetof(xmlXPathContext, dict)                 == 328, "xmlXPathContext::dict @ 328");
_Static_assert(offsetof(xmlXPathContext, flags)                == 336, "xmlXPathContext::flags @ 336");
_Static_assert(offsetof(xmlXPathContext, cache)                == 344, "xmlXPathContext::cache @ 344");

/* Resource-limit tail.  Either the legacy ifdef is defined (libxml2
 * built with the feature on) or we're on a modern enough version
 * that dropped the ifdef.  Use the version cutoff: 2.13.0 = 21300. */
#if defined(LIBXML_HAS_XPATH_RESOURCE_LIMITS) || LIBXML_VERSION >= 21300
_Static_assert(offsetof(xmlXPathContext, opLimit) == 352, "xmlXPathContext::opLimit @ 352");
_Static_assert(offsetof(xmlXPathContext, opCount) == 360, "xmlXPathContext::opCount @ 360");
_Static_assert(offsetof(xmlXPathContext, depth)   == 368, "xmlXPathContext::depth @ 368");
_Static_assert(sizeof(xmlXPathContext)            == 376, "sizeof(xmlXPathContext) == 376 (with resource limits)");
#else
_Static_assert(sizeof(xmlXPathContext)            == 352, "sizeof(xmlXPathContext) == 352 (no resource limits)");
#endif

int main(void) {
    /* If we got here, every _Static_assert above passed. */
    printf("T-UPSTREAM-LAYOUT OK\n");
    return 0;
}
