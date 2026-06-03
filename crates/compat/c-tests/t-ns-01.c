/* T-NS-01: default namespace is in scope for descendants.
 * Reads `child->ns->href` via byte-exact field access.
 */

#include <stdio.h>
#include <string.h>
#include <stddef.h>

typedef unsigned char xmlChar;
typedef int xmlElementType;
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

/* Read node->ns (offset 72) and node->name (offset 16). */
typedef struct {
    void           *_private;     /*  0 */
    xmlElementType  type;         /*  8 */
    int             _pad;
    const xmlChar  *name;         /* 16 */
    void           *children;     /* 24 */
    void           *last;         /* 32 */
    void           *parent;       /* 40 */
    void           *next;         /* 48 */
    void           *prev;         /* 56 */
    void           *doc;          /* 64 */
    xmlNs          *ns;           /* 72 */
} xmlNode_min;

extern xmlDoc  *xmlReadMemory(const char *, int, const char *, const char *, int);
extern void     xmlFreeDoc(xmlDoc *);
extern xmlNode *xmlDocGetRootElement(const xmlDoc *);
extern xmlNode *xmlFirstElementChild(xmlNode *);

int main(void) {
    const char *src = "<r xmlns=\"http://example.com/r\"><a/></r>";
    xmlDoc *doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (!doc) { fprintf(stderr, "parse failed\n"); return 1; }

    xmlNode *root  = xmlDocGetRootElement(doc);
    xmlNode *child = xmlFirstElementChild(root);
    if (!child) { fprintf(stderr, "no child\n"); xmlFreeDoc(doc); return 2; }

    xmlNs *ns = ((xmlNode_min *) child)->ns;
    if (!ns) {
        fprintf(stderr, "child->ns is NULL — default ns not bound\n");
        xmlFreeDoc(doc); return 3;
    }
    /* Default namespace: prefix is NULL, href matches. */
    if (ns->prefix != NULL) {
        fprintf(stderr, "default ns prefix should be NULL, got %s\n",
                (const char *) ns->prefix);
        xmlFreeDoc(doc); return 4;
    }
    if (!ns->href || strcmp((const char *) ns->href, "http://example.com/r") != 0) {
        fprintf(stderr, "href mismatch: %s\n",
                ns->href ? (const char *) ns->href : "(null)");
        xmlFreeDoc(doc); return 5;
    }

    xmlFreeDoc(doc);
    printf("T-NS-01 OK\n");
    return 0;
}
