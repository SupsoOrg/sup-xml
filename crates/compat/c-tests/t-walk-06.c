/* T-WALK-06: read xmlDoc properties via direct field access at the
 * byte-exact offsets that T-LAYOUT-03 pins.
 *
 * Asserts:
 *   doc->version   == "1.0"
 *   doc->encoding  is NULL (source had no `<?xml encoding="…"?>`
 *                  declaration — libxml2's documented behaviour)
 *   doc->children  is non-NULL and matches xmlDocGetRootElement
 *   doc->standalone == -1 (no `standalone=` declaration — libxml2's
 *                    "unspecified" value; xmlNewDoc initialises it to -1)
 */

#include <stdio.h>
#include <string.h>
#include <stddef.h>

typedef struct _xmlNode xmlNode;
typedef unsigned char   xmlChar;
typedef int             xmlElementType;

/* Minimal xmlDoc layout — T-LAYOUT-03 verifies these offsets. */
typedef struct _xmlDoc_min {
    void           *_private;
    xmlElementType  type;
    int             _pad;
    char           *name;
    xmlNode        *children;
    xmlNode        *last;
    xmlNode        *parent;
    void           *next;
    void           *prev;
    void           *doc;
    int             compression;
    int             standalone;
    void           *intSubset;
    void           *extSubset;
    void           *oldNs;
    const xmlChar  *version;
    const xmlChar  *encoding;
} xmlDoc_min;

/* Real xmlDoc is opaque to callers — we only read through xmlDoc_min. */
typedef struct _xmlDoc xmlDoc;

extern xmlDoc  *xmlReadMemory(const char *, int, const char *, const char *, int);
extern void     xmlFreeDoc(xmlDoc *);
extern xmlNode *xmlDocGetRootElement(const xmlDoc *);

int main(void) {
    const char *src = "<r/>";
    xmlDoc *doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (!doc) { fprintf(stderr, "parse failed\n"); return 1; }

    xmlDoc_min *d = (xmlDoc_min *) doc;

    if (!d->version || strcmp((const char *) d->version, "1.0") != 0) {
        fprintf(stderr, "version: expected \"1.0\", got %s\n",
                d->version ? (const char *) d->version : "(null)");
        xmlFreeDoc(doc); return 2;
    }
    if (d->encoding != NULL) {
        fprintf(stderr, "encoding: expected NULL (no `<?xml encoding=...?>` "
                "declaration in source), got %s\n",
                (const char *) d->encoding);
        xmlFreeDoc(doc); return 3;
    }
    if (d->standalone != -1) {
        fprintf(stderr, "standalone: expected -1, got %d\n", d->standalone);
        xmlFreeDoc(doc); return 4;
    }
    if (!d->children) {
        fprintf(stderr, "children is NULL\n");
        xmlFreeDoc(doc); return 5;
    }
    if (d->children != xmlDocGetRootElement(doc)) {
        fprintf(stderr, "doc->children != xmlDocGetRootElement(doc)\n");
        xmlFreeDoc(doc); return 6;
    }

    xmlFreeDoc(doc);
    printf("T-WALK-06 OK\n");
    return 0;
}
