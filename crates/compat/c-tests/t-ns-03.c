/* T-NS-03: xmlSearchNs from a descendant finds an ancestor declaration.
 * T-NS-04: xmlSearchNsByHref finds it by URI.
 */

#include <stdio.h>
#include <string.h>
#include <stddef.h>

typedef unsigned char xmlChar;
typedef int xmlNsType;
typedef struct _xmlDoc  xmlDoc;
typedef struct _xmlNode xmlNode;

/* libxml2 `_xmlNs` layout — see T-LAYOUT-04. */
typedef struct _xmlNs {
    struct _xmlNs  *next;
    xmlNsType       type;
    int             _pad_type;
    const xmlChar  *href;
    const xmlChar  *prefix;
    void           *_private;
    void           *context;
} xmlNs;

extern xmlDoc  *xmlReadMemory(const char *, int, const char *, const char *, int);
extern void     xmlFreeDoc(xmlDoc *);
extern xmlNode *xmlDocGetRootElement(const xmlDoc *);
extern xmlNode *xmlFirstElementChild(xmlNode *);
extern xmlNs   *xmlSearchNs(const xmlDoc *, xmlNode *, const xmlChar *);
extern xmlNs   *xmlSearchNsByHref(const xmlDoc *, xmlNode *, const xmlChar *);

int main(void) {
    const char *src =
        "<r xmlns:foo=\"http://example.com/foo\">"
          "<inner><deep/></inner>"
        "</r>";
    xmlDoc *doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (!doc) { fprintf(stderr, "parse failed\n"); return 1; }

    xmlNode *root  = xmlDocGetRootElement(doc);
    xmlNode *inner = xmlFirstElementChild(root);
    xmlNode *deep  = xmlFirstElementChild(inner);

    /* By prefix from descendant. */
    xmlNs *ns = xmlSearchNs(doc, deep, (const xmlChar *) "foo");
    if (!ns) { fprintf(stderr, "search by prefix returned NULL\n"); xmlFreeDoc(doc); return 2; }
    if (!ns->href || strcmp((const char *) ns->href, "http://example.com/foo") != 0) {
        fprintf(stderr, "href mismatch\n"); xmlFreeDoc(doc); return 3;
    }
    if (!ns->prefix || strcmp((const char *) ns->prefix, "foo") != 0) {
        fprintf(stderr, "prefix mismatch\n"); xmlFreeDoc(doc); return 4;
    }

    /* Unknown prefix → NULL. */
    if (xmlSearchNs(doc, deep, (const xmlChar *) "bar") != NULL) {
        fprintf(stderr, "search for unknown prefix should be NULL\n");
        xmlFreeDoc(doc); return 5;
    }

    /* By href from descendant. */
    xmlNs *byh = xmlSearchNsByHref(doc, deep,
                                   (const xmlChar *) "http://example.com/foo");
    if (!byh) { fprintf(stderr, "search by href returned NULL\n"); xmlFreeDoc(doc); return 6; }
    if (byh != ns) {
        fprintf(stderr, "search by href returned a different record\n");
        xmlFreeDoc(doc); return 7;
    }

    /* Unknown href → NULL. */
    if (xmlSearchNsByHref(doc, deep, (const xmlChar *) "http://nope/") != NULL) {
        fprintf(stderr, "search for unknown href should be NULL\n");
        xmlFreeDoc(doc); return 8;
    }

    xmlFreeDoc(doc);
    printf("T-NS-03 OK\n");
    return 0;
}
