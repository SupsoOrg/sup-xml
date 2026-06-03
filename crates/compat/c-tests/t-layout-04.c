/* T-LAYOUT-04: xmlNs struct layout — C-side _Static_assert.
 *
 * Reference: libxml2's `_xmlNs`.  Verified against libxml2 2.9.13
 * and 2.15.3; compat is pinned to that layout.  Future libxml2
 * versions are NOT covered — `t-upstream-layout.c` checks the live
 * installed header.  Note that `_xmlNs` is the one public libxml2
 * struct where `next` precedes `_private` (every other one —
 * `_xmlNode`, `_xmlDoc`, `_xmlAttr` — puts `_private` at offset 0).
 * Easy to get wrong by analogy; the t-libxml2-headers test catches
 * that class of bug.
 *
 * Pairs with the const-offset assertions on `Namespace` in
 * `crates/tree/src/dom.rs`.
 */

#include <stdio.h>
#include <stddef.h>

typedef int xmlNsType;
typedef unsigned char xmlChar;

typedef struct _xmlNs {
    struct _xmlNs  *next;
    xmlNsType       type;
    int             _pad_type;
    const xmlChar  *href;
    const xmlChar  *prefix;
    void           *_private;
    void           *context;
} xmlNs;

_Static_assert(offsetof(xmlNs, next)     ==  0, "xmlNs::next @ 0");
_Static_assert(offsetof(xmlNs, type)     ==  8, "xmlNs::type @ 8");
_Static_assert(offsetof(xmlNs, href)     == 16, "xmlNs::href @ 16");
_Static_assert(offsetof(xmlNs, prefix)   == 24, "xmlNs::prefix @ 24");
_Static_assert(offsetof(xmlNs, _private) == 32, "xmlNs::_private @ 32");
_Static_assert(offsetof(xmlNs, context)  == 40, "xmlNs::context @ 40");
_Static_assert(sizeof(xmlNs)             == 48, "sizeof(xmlNs) == 48");

int main(void) {
    /* If we got here, every _Static_assert above passed. */
    printf("T-LAYOUT-04 OK\n");
    return 0;
}
