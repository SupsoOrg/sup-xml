/* T-ATTR-05: xmlGetNoNsProp ignores namespaced attrs.
 *
 *   <r xmlns:x="..." id="plain" x:id="prefixed"/>
 *
 * xmlGetNoNsProp(root, "id") → "plain" (skips x:id)
 * xmlGetProp(root, "id") → "plain" (first local-name match — xmlns:x
 *                                    is filtered out, id comes before
 *                                    x:id in document order)
 * xmlGetNsProp(root, "id", "http://example.com/x") → "prefixed"
 */

#include <stdio.h>
#include <string.h>

typedef struct _xmlDoc  xmlDoc;
typedef struct _xmlNode xmlNode;
typedef unsigned char   xmlChar;

extern xmlDoc  *xmlReadMemory(const char *, int, const char *, const char *, int);
extern void     xmlFreeDoc(xmlDoc *);
extern xmlNode *xmlDocGetRootElement(const xmlDoc *);
extern xmlChar *xmlGetProp(const xmlNode *, const xmlChar *);
extern xmlChar *xmlGetNoNsProp(const xmlNode *, const xmlChar *);
extern xmlChar *xmlGetNsProp(const xmlNode *, const xmlChar *, const xmlChar *);
/* `xmlFree` is a global function pointer in libxml2's headers, not a
 * function — callers compile to `LDR + BLR` instead of a direct `BL`.
 * Forward-declaring it as a function would emit the wrong call shape
 * and jump into the data segment at runtime. */
typedef void (*xmlFreeFunc)(void *mem);
extern xmlFreeFunc xmlFree;

int main(void) {
    const char *src =
        "<r xmlns:x=\"http://example.com/x\" id=\"plain\" x:id=\"prefixed\"/>";
    xmlDoc *doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (!doc) { fprintf(stderr, "parse failed\n"); return 1; }

    xmlNode *root = xmlDocGetRootElement(doc);

    xmlChar *v = xmlGetNoNsProp(root, (const xmlChar *) "id");
    if (!v || strcmp((const char *) v, "plain") != 0) {
        fprintf(stderr, "NoNsProp(id) expected \"plain\", got %s\n",
                v ? (const char *) v : "(null)");
        xmlFreeDoc(doc); return 2;
    }
    xmlFree(v);

    v = xmlGetProp(root, (const xmlChar *) "id");
    if (!v || strcmp((const char *) v, "plain") != 0) {
        fprintf(stderr, "GetProp(id) expected \"plain\", got %s\n",
                v ? (const char *) v : "(null)");
        xmlFreeDoc(doc); return 3;
    }
    xmlFree(v);

    v = xmlGetNsProp(root, (const xmlChar *) "id",
                     (const xmlChar *) "http://example.com/x");
    if (!v || strcmp((const char *) v, "prefixed") != 0) {
        fprintf(stderr, "NsProp(id, x) expected \"prefixed\", got %s\n",
                v ? (const char *) v : "(null)");
        xmlFreeDoc(doc); return 4;
    }
    xmlFree(v);

    /* Wrong namespace → NULL. */
    v = xmlGetNsProp(root, (const xmlChar *) "id",
                     (const xmlChar *) "http://nope/");
    if (v != NULL) {
        fprintf(stderr, "NsProp on wrong ns should be NULL\n");
        xmlFree(v); xmlFreeDoc(doc); return 5;
    }

    xmlFreeDoc(doc);
    printf("T-ATTR-05 OK\n");
    return 0;
}
