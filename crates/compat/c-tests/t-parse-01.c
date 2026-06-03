/* T-PARSE-01: parse a tiny doc from memory, walk to the root, read
 * its name + content, free everything.  End-to-end exercise of the
 * smallest interesting libxml2 surface.
 *
 * Functions exercised:
 *   xmlReadMemory
 *   xmlDocGetRootElement
 *   xmlNodeGetContent
 *   xmlFree
 *   xmlFreeDoc
 *
 * Asserts:
 *   - returned doc is non-NULL
 *   - root element exists, name == "r"
 *   - content == "hello"
 *   - no crash on free
 */

#include <stdio.h>
#include <stddef.h>
#include <string.h>

/* libxml2-shape forward declarations.  We don't include any libxml2
 * headers (we ARE the libxml2-compatible side); the layout pieces we
 * need are the ones T-LAYOUT-* asserts byte-exact.  Pointers to
 * forward-declared structs are 8 bytes and that's all we touch
 * through this typedef.
 */
typedef struct _xmlDoc  xmlDoc;
typedef struct _xmlNode xmlNode;

/* `xmlChar` is libxml2's UTF-8 byte type — really `unsigned char`. */
typedef unsigned char xmlChar;

/* Reach into xmlNode->name (offset 16) via a minimal layout.  We pin
 * only the fields we read; the C compiler will lay them out at the
 * same offsets the Rust side asserts.
 */
typedef int xmlElementType;
typedef struct _xmlNode_min {
    void           *_private;     /* offset  0 */
    xmlElementType  type;         /* offset  8 */
    int             _pad_type;
    const xmlChar  *name;         /* offset 16 */
} xmlNode_min;

extern xmlDoc  *xmlReadMemory(const char *buffer, int size,
                               const char *url, const char *encoding,
                               int options);
extern void     xmlFreeDoc(xmlDoc *doc);
extern xmlNode *xmlDocGetRootElement(const xmlDoc *doc);
extern xmlChar *xmlNodeGetContent(const xmlNode *cur);
/* `xmlFree` is a global function pointer in libxml2's headers, not a
 * function — callers compile to `LDR + BLR` instead of a direct `BL`.
 * Forward-declaring it as a function would emit the wrong call shape
 * and jump into the data segment at runtime. */
typedef void (*xmlFreeFunc)(void *mem);
extern xmlFreeFunc xmlFree;

int main(void) {
    const char *src = "<r>hello</r>";
    xmlDoc *doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (doc == NULL) {
        fprintf(stderr, "xmlReadMemory returned NULL on valid input\n");
        return 1;
    }

    xmlNode *root = xmlDocGetRootElement(doc);
    if (root == NULL) {
        fprintf(stderr, "xmlDocGetRootElement returned NULL\n");
        xmlFreeDoc(doc);
        return 2;
    }

    /* Read root->name directly via the byte-exact layout. */
    const xmlChar *name = ((xmlNode_min *) root)->name;
    if (name == NULL || strcmp((const char *) name, "r") != 0) {
        fprintf(stderr, "root->name expected \"r\", got %s\n",
                name ? (const char *) name : "(null)");
        xmlFreeDoc(doc);
        return 3;
    }

    xmlChar *content = xmlNodeGetContent(root);
    if (content == NULL) {
        fprintf(stderr, "xmlNodeGetContent returned NULL\n");
        xmlFreeDoc(doc);
        return 4;
    }
    if (strcmp((const char *) content, "hello") != 0) {
        fprintf(stderr, "content expected \"hello\", got \"%s\"\n",
                (const char *) content);
        xmlFree(content);
        xmlFreeDoc(doc);
        return 5;
    }

    xmlFree(content);
    xmlFreeDoc(doc);

    printf("T-PARSE-01 OK\n");
    return 0;
}
