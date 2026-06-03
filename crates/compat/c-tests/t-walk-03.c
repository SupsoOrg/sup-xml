/* T-WALK-03: xmlChildElementCount matches a manual count.
 *
 * Input has 7 element children and 3 non-element siblings (text +
 * comment + PI).  Function must return 7, not 10.
 */

#include <stdio.h>
#include <string.h>

typedef struct _xmlDoc  xmlDoc;
typedef struct _xmlNode xmlNode;

extern xmlDoc  *xmlReadMemory(const char *, int, const char *, const char *, int);
extern void     xmlFreeDoc(xmlDoc *);
extern xmlNode *xmlDocGetRootElement(const xmlDoc *);
extern unsigned long xmlChildElementCount(xmlNode *);

int main(void) {
    const char *src =
        "<r>"
          "<!-- before -->"
          "<a/><b/><c/>text<d/><e/><?pi target?><f/><g/>"
        "</r>";
    xmlDoc *doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (!doc) { fprintf(stderr, "parse failed\n"); return 1; }

    xmlNode *root = xmlDocGetRootElement(doc);
    unsigned long n = xmlChildElementCount(root);
    if (n != 7) {
        fprintf(stderr, "expected 7 element children, got %lu\n", n);
        xmlFreeDoc(doc); return 2;
    }

    /* NULL is safe and returns 0. */
    if (xmlChildElementCount(NULL) != 0) {
        fprintf(stderr, "expected 0 on NULL parent\n");
        xmlFreeDoc(doc); return 3;
    }

    xmlFreeDoc(doc);
    printf("T-WALK-03 OK\n");
    return 0;
}
