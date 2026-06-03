/* T-WALK-04: xmlNodeGetContent on mixed content concatenates text
 * across element children, expanding nothing else.
 *
 * Input:    <r>alpha<inner>beta<deep>gamma</deep></inner>delta</r>
 * Expected: "alphabetagammadelta"
 *
 * Comments and PIs at any depth contribute nothing to the content.
 */

#include <stdio.h>
#include <string.h>

typedef struct _xmlDoc  xmlDoc;
typedef struct _xmlNode xmlNode;
typedef unsigned char   xmlChar;

extern xmlDoc  *xmlReadMemory(const char *, int, const char *, const char *, int);
extern void     xmlFreeDoc(xmlDoc *);
extern xmlNode *xmlDocGetRootElement(const xmlDoc *);
extern xmlChar *xmlNodeGetContent(const xmlNode *);
/* `xmlFree` is a global function pointer in libxml2's headers, not a
 * function — callers compile to `LDR + BLR` instead of a direct `BL`.
 * Forward-declaring it as a function would emit the wrong call shape
 * and jump into the data segment at runtime. */
typedef void (*xmlFreeFunc)(void *mem);
extern xmlFreeFunc xmlFree;

int main(void) {
    const char *src =
        "<r>alpha<inner><!-- ignored -->beta<deep>gamma</deep>"
        "<?pi ignored?></inner>delta</r>";
    xmlDoc *doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (!doc) { fprintf(stderr, "parse failed\n"); return 1; }

    xmlNode *root = xmlDocGetRootElement(doc);
    xmlChar *content = xmlNodeGetContent(root);
    if (!content) {
        fprintf(stderr, "xmlNodeGetContent returned NULL\n");
        xmlFreeDoc(doc); return 2;
    }
    if (strcmp((const char *) content, "alphabetagammadelta") != 0) {
        fprintf(stderr, "content mismatch: got \"%s\"\n", (const char *) content);
        xmlFree(content); xmlFreeDoc(doc); return 3;
    }

    xmlFree(content);
    xmlFreeDoc(doc);
    printf("T-WALK-04 OK\n");
    return 0;
}
