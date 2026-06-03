/* T-LAYOUT-03: xmlDoc struct layout — C-side _Static_assert.
 *
 * Pairs with the const-offset assertions on `XmlDoc` in
 * `crates/tree/src/dom.rs`.  Both sides must agree byte-for-byte
 * or libxml2 callers' field reads through `xmlDocPtr` will land at
 * the wrong place.
 *
 * Reference: libxml2's `_xmlDoc` on 64-bit (Linux/macOS, x86-64 /
 * aarch64).  Verified against libxml2 2.9.13 and 2.15.3; compat is
 * pinned to that layout.  Future libxml2 versions are NOT covered
 * by this assertion — `t-upstream-layout.c` compiles against the
 * actual installed `<libxml/tree.h>` and fails the build if the
 * header disagrees, surfacing the moment a coordinated update is
 * needed on our side.
 */

#include <stdio.h>
#include <stddef.h>

typedef int xmlElementType;            /* enum, fits in int */

/* Forward-declare to keep declarations local — we don't include
 * libxml2 headers (we ARE the implementation of libxml2). */
struct _xmlNode;
struct _xmlDoc;
struct _xmlDtd;
struct _xmlNs;
struct _xmlDict;

typedef struct _xmlDoc {
    void           *_private;
    xmlElementType  type;
    char           *name;
    struct _xmlNode *children;
    struct _xmlNode *last;
    struct _xmlNode *parent;
    struct _xmlNode *next;
    struct _xmlNode *prev;
    struct _xmlDoc  *doc;
    int             compression;
    int             standalone;
    struct _xmlDtd  *intSubset;
    struct _xmlDtd  *extSubset;
    struct _xmlNs   *oldNs;
    const unsigned char *version;
    const unsigned char *encoding;
    void           *ids;
    void           *refs;
    const unsigned char *URL;
    int             charset;
    struct _xmlDict *dict;
    void           *psvi;
    int             parseFlags;
    int             properties;
} xmlDoc;

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

int main(void) {
    /* If we got here, every _Static_assert above passed. */
    printf("T-LAYOUT-03 OK\n");
    return 0;
}
