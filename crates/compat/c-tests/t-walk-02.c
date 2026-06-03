/* T-WALK-02: element-aware iteration helpers skip non-elements.
 *
 * Functions exercised:
 *   xmlReadMemory, xmlDocGetRootElement, xmlFreeDoc
 *   xmlFirstElementChild, xmlLastElementChild
 *   xmlNextElementSibling, xmlPreviousElementSibling
 *
 * Input has three element children interleaved with text and a comment.
 * The element walk must surface exactly the three elements in order.
 */

#include <stdio.h>
#include <string.h>

typedef struct _xmlDoc  xmlDoc;
typedef struct _xmlNode xmlNode;
typedef unsigned char   xmlChar;

/* Minimal layout fragment for reading node->name @ offset 16. */
typedef int xmlElementType;
typedef struct {
    void           *_private;
    xmlElementType  type;
    int             _pad;
    const xmlChar  *name;
} xmlNode_min;

extern xmlDoc  *xmlReadMemory(const char *, int, const char *, const char *, int);
extern void     xmlFreeDoc(xmlDoc *);
extern xmlNode *xmlDocGetRootElement(const xmlDoc *);
extern xmlNode *xmlFirstElementChild(xmlNode *);
extern xmlNode *xmlLastElementChild(xmlNode *);
extern xmlNode *xmlNextElementSibling(xmlNode *);
extern xmlNode *xmlPreviousElementSibling(xmlNode *);

static const char *name_of(xmlNode *n) {
    return n ? (const char *) ((xmlNode_min *) n)->name : "(null)";
}

int main(void) {
    const char *src = "<r><!-- c --><a/>text<b/><c/>tail</r>";
    xmlDoc *doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (!doc) { fprintf(stderr, "parse failed\n"); return 1; }

    xmlNode *root  = xmlDocGetRootElement(doc);
    xmlNode *first = xmlFirstElementChild(root);
    xmlNode *last  = xmlLastElementChild(root);
    if (!first || strcmp(name_of(first), "a") != 0) {
        fprintf(stderr, "first: expected a, got %s\n", name_of(first));
        xmlFreeDoc(doc); return 2;
    }
    if (!last || strcmp(name_of(last), "c") != 0) {
        fprintf(stderr, "last: expected c, got %s\n", name_of(last));
        xmlFreeDoc(doc); return 3;
    }

    xmlNode *b = xmlNextElementSibling(first);
    if (!b || strcmp(name_of(b), "b") != 0) {
        fprintf(stderr, "next(a): expected b, got %s\n", name_of(b));
        xmlFreeDoc(doc); return 4;
    }
    xmlNode *c = xmlNextElementSibling(b);
    if (!c || strcmp(name_of(c), "c") != 0) {
        fprintf(stderr, "next(b): expected c, got %s\n", name_of(c));
        xmlFreeDoc(doc); return 5;
    }
    if (xmlNextElementSibling(c) != NULL) {
        fprintf(stderr, "next(c): expected NULL\n");
        xmlFreeDoc(doc); return 6;
    }

    /* Walk backwards from c. */
    xmlNode *b2 = xmlPreviousElementSibling(c);
    if (!b2 || strcmp(name_of(b2), "b") != 0) {
        fprintf(stderr, "prev(c): expected b, got %s\n", name_of(b2));
        xmlFreeDoc(doc); return 7;
    }
    xmlNode *a2 = xmlPreviousElementSibling(b2);
    if (!a2 || strcmp(name_of(a2), "a") != 0) {
        fprintf(stderr, "prev(b): expected a, got %s\n", name_of(a2));
        xmlFreeDoc(doc); return 8;
    }
    if (xmlPreviousElementSibling(a2) != NULL) {
        fprintf(stderr, "prev(a): expected NULL\n");
        xmlFreeDoc(doc); return 9;
    }

    xmlFreeDoc(doc);
    printf("T-WALK-02 OK\n");
    return 0;
}
