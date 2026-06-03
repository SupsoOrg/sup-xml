/* T-ATTR-01: xmlGetProp returns the attribute value as a malloc'd
 * NUL-terminated string; missing attribute returns NULL; xmlHasProp
 * returns the xmlAttr* (or NULL).
 *
 * Functions exercised:
 *   xmlReadMemory, xmlDocGetRootElement, xmlFreeDoc
 *   xmlGetProp, xmlHasProp, xmlFree
 */

#include <stdio.h>
#include <string.h>

typedef struct _xmlDoc  xmlDoc;
typedef struct _xmlNode xmlNode;
typedef struct _xmlAttr xmlAttr;
typedef unsigned char   xmlChar;

extern xmlDoc  *xmlReadMemory(const char *, int, const char *, const char *, int);
extern void     xmlFreeDoc(xmlDoc *);
extern xmlNode *xmlDocGetRootElement(const xmlDoc *);
extern xmlChar *xmlGetProp(const xmlNode *, const xmlChar *);
extern xmlAttr *xmlHasProp(const xmlNode *, const xmlChar *);
/* `xmlFree` is a global function pointer in libxml2's headers, not a
 * function — callers compile to `LDR + BLR` instead of a direct `BL`.
 * Forward-declaring it as a function would emit the wrong call shape
 * and jump into the data segment at runtime. */
typedef void (*xmlFreeFunc)(void *mem);
extern xmlFreeFunc xmlFree;

int main(void) {
    const char *src = "<r id=\"42\" name=\"hello\"/>";
    xmlDoc *doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (!doc) { fprintf(stderr, "parse failed\n"); return 1; }

    xmlNode *root = xmlDocGetRootElement(doc);

    xmlChar *v_id = xmlGetProp(root, (const xmlChar *) "id");
    if (!v_id || strcmp((const char *) v_id, "42") != 0) {
        fprintf(stderr, "id mismatch: %s\n", v_id ? (const char *) v_id : "(null)");
        xmlFreeDoc(doc); return 2;
    }
    xmlFree(v_id);

    xmlChar *v_name = xmlGetProp(root, (const xmlChar *) "name");
    if (!v_name || strcmp((const char *) v_name, "hello") != 0) {
        fprintf(stderr, "name mismatch\n");
        xmlFreeDoc(doc); return 3;
    }
    xmlFree(v_name);

    /* Missing attribute → NULL. */
    if (xmlGetProp(root, (const xmlChar *) "missing") != NULL) {
        fprintf(stderr, "missing should be NULL\n");
        xmlFreeDoc(doc); return 4;
    }

    /* xmlHasProp returns xmlAttr* for present, NULL for absent. */
    if (xmlHasProp(root, (const xmlChar *) "id") == NULL) {
        fprintf(stderr, "xmlHasProp(id) should be non-NULL\n");
        xmlFreeDoc(doc); return 5;
    }
    if (xmlHasProp(root, (const xmlChar *) "missing") != NULL) {
        fprintf(stderr, "xmlHasProp(missing) should be NULL\n");
        xmlFreeDoc(doc); return 6;
    }

    xmlFreeDoc(doc);
    printf("T-ATTR-01 OK\n");
    return 0;
}
