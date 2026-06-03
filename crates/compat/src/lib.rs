//! C ABI compatibility shim for `libsupxml2.so` (eventually
//! `libxml2.so.2`).  This crate is the boundary where Rust types
//! become libxml2-shaped C structs visible to downstream C/C++
//! callers via the cdylib.
//!
//! # Status
//!
//! Pre-1.0.  The function surface and struct layouts here are NOT
//! frozen yet — see `thoughts/libxml2_abi_plan.txt` for the rollout
//! plan.  Once we ship a 1.0 release the C ABI is contractual
//! forever (every offset, every symbol, every signature).
//!
//! # Layout invariants
//!
//! This crate builds against `sup-xml-tree` with the `c-abi`
//! feature enabled (see `Cargo.toml`), which switches `Node`,
//! `Attribute`, and `Namespace` from their lean Rust layouts to the
//! byte-exact `_xmlNode`/`_xmlAttr`/`_xmlNs` shapes.  Compile-time
//! `offset_of!` assertions in `sup-xml-tree/src/dom.rs` guard the
//! layout; this crate's `c-tests/` add a second-line check via
//! `_Static_assert` against libxml2's documented offsets.
//!
//! # Unsafe policy
//!
//! Every public function in this crate is `extern "C"` and works
//! with raw pointers from caller-supplied C code.  The crate-level
//! `#[allow(unsafe_code)]` lift is intentional — there's no
//! `unsafe`-free way to build a C ABI surface.  Module-level docs
//! explain each `unsafe { ... }` block's safety contract.
//!
//! # Deliberate behavioral divergences from libxml2 / lxml
//!
//! A handful of lxml-suite tests assert behavior we intentionally do
//! NOT reproduce, either because our behavior is more correct or because
//! matching would mean reproducing a libxml2 quirk / bug.  They are
//! recorded here so a future reader doesn't "fix" a divergence that is
//! working as designed.  (Separately, lxml's `_XIncludeTestCase` /
//! `_IOTestCaseBase` "errors" when a suite is run module-wide are a
//! unittest harness artifact — abstract base classes with no
//! `self.include` / fixtures — not shim behavior.)
//!
//! ## Better-by-design (we are more correct than libxml2)
//!
//! - **`test_large_sourceline_XML`** — `Element.sourceline` past line
//!   65535.  libxml2 stores `node->line` in an `unsigned short`, caps it
//!   at 65535, and `xmlGetLineNo` then *recurses* into a text child /
//!   sibling and returns *that text run's end line* — so a big-file
//!   element reports a neighbouring node's position, and a childless
//!   `<br/>` reports its next sibling's.  We keep the element's real
//!   start line at full width (`Node::full_line`, see
//!   [`nodeacc::xmlGetLineNo`]).  Correct number, not the recursion
//!   artifact the test pins.
//! - **`test_module_parse_html_error`** (`<html></body>`) and
//!   **`test_module_HTML_broken`** / **`test_default_parser_HTML_broken`**
//!   (a stray `</p>` yielding an implied `<p></p>`) — our HTML parser is
//!   html5ever, which follows the WHATWG/HTML5 spec exactly as browsers
//!   do: it recovers a stray `</body>` silently and inserts the implied
//!   `<p>`.  libxml2's ad-hoc HTML parser flags the former and drops the
//!   latter.  We match what browsers actually build; reproducing
//!   libxml2 would mean re-implementing its pre-HTML5 recovery quirks.
//!
//! ## Won't-fix without a large redesign (correctness unaffected)
//!
//! - **`test_html_iterparse_broken_no_recover`**,
//!   **`test_html_iterparse_stop_short`**,
//!   **`test_html_parser_target_exceptions`** — lxml fires SAX events
//!   *incrementally during* `feed()`.  Our HTML push parser buffers to
//!   the terminating chunk, then replays SAX from the finished tree
//!   (see [`pushparse`]): html5ever's tree builder performs adoption-
//!   agency / reconstruction, so a parse of a *prefix* is not stable
//!   against the full document the way libxml2's streaming parser is.
//!   The final tree is correct; only event *timing* during `feed()`
//!   differs.  True incremental HTML SAX is a separate design effort.
//! - **`test_parse_error_logging`** — on malformed XML we emit one fatal
//!   error and stop; libxml2 internally recovers and logs cascading
//!   errors (the test wants an entry at column 15 from a *second*,
//!   recovered mismatch).  We still correctly reject the document
//!   (`XMLSyntaxError` is raised) — only the error-log richness differs.
//!   Multi-error recovery is a separate parser feature.
//!
//! ## Version-reporting cosmetic
//!
//! - **`test_html_prefix_nsmap`** — `etree.HTML('<hha:page-description>')`.
//!   We keep the `hha:` prefix in the (namespace-free) tag name, which is
//!   libxml2's behavior from 2.10.4 onward and in current releases; the
//!   test branches on our reported `LIBXML_VERSION` (2.9.13) and so
//!   expects the older prefix-stripping.  Our tree is the modern, correct
//!   shape; matching would mean either misreporting a newer version
//!   (rippling through many lxml version checks) or regressing HTML
//!   parsing to the superseded behavior.

#![allow(unsafe_code)] // see crate-level docs
// libxml2's ABI uses lowerCamelCase struct fields and snake-ish
// upperCamel type names (`xmlNode`, `xmlSchemaValidCtxt`,
// `nodeTab`, `funcHash`, etc.).  Mirroring them byte-for-byte is
// the whole point of this crate, so the conventional Rust
// snake_case / UpperCamelCase lints are noise here — every match
// would be a wrong fix.  Silenced crate-wide rather than per
// item.
#![allow(non_snake_case, non_camel_case_types)]
// Miri rejects transmuting non-variadic fn pointers to variadic ones
// (strictly UB by Rust's type-system rules, even when ABI-identical
// at the hardware level).  Under Miri we use the nightly `c_variadic`
// feature to define test handlers with a real variadic signature, so
// no transmute is needed.  Miri implies nightly; stable builds never
// see this attribute.
#![cfg_attr(miri, feature(c_variadic))]

pub mod alloc;
pub mod attr;
pub mod c14n;
pub mod charclass;
pub mod dict;
pub mod dtd;
pub mod dtddecl;
pub mod encoding;
pub mod error;
pub mod exslt;
pub mod hash;
pub mod html;
pub mod idindex;
pub mod init;
pub mod input_callbacks;
pub mod misc;
pub mod mutate;
pub mod mutex;
pub mod nodeacc;
pub mod ns;
pub mod outbuf;
pub mod parse;
pub mod parsectx;
pub mod pushincr;
pub mod pushparse;
pub mod reader;
pub mod relaxng;
pub mod save;
pub mod saxreplay;
pub mod serialize;
pub mod strutil;
pub mod pattern;
pub mod perl_stubs;
pub mod php_stubs;
pub mod regex;
pub mod stubs;
pub mod xmlwriter;
pub mod tree;
pub mod uri;
pub mod xinclude;
pub mod xpath;
pub mod xsd;
pub mod xslt;
