/* T-MEM-02: xmlFree on an arena-resident pointer is a safe no-op
 * (the pointer remains readable afterwards).
 *
 * libxml2's historical behavior is to silently no-op when xmlFree is
 * called on a pointer that wasn't malloc'd by the library — for example
 * `node->name`, which lives inside the document's internal allocation.
 * Consumer code that doesn't always know which kind of pointer it
 * holds relies on this leniency.
 *
 * We replicate the contract: xmlFree consults an allocator registry;
 * pointers we didn't hand out are left alone.
 */

#include <stdio.h>
#include <stddef.h>
#include <string.h>

typedef struct _xmlDoc  xmlDoc;
typedef struct _xmlNode xmlNode;
typedef unsigned char   xmlChar;
typedef int             xmlElementType;

/* Minimal layout for reading node->name @ offset 16. */
typedef struct {
    void           *_private;
    xmlElementType  type;
    int             _pad;
    const xmlChar  *name;
} xmlNode_min;

extern xmlDoc  *xmlReadMemory(const char *, int, const char *, const char *, int);
extern void     xmlFreeDoc(xmlDoc *);
extern xmlNode *xmlDocGetRootElement(const xmlDoc *);
/* `xmlFree` is a global function pointer in libxml2's headers, not a
 * function — callers compile to `LDR + BLR` instead of a direct `BL`.
 * Forward-declaring it as a function would emit the wrong call shape
 * and jump into the data segment at runtime. */
typedef void (*xmlFreeFunc)(void *mem);
extern xmlFreeFunc xmlFree;

int main(void) {
    const char *src = "<root/>";
    xmlDoc *doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (!doc) { fprintf(stderr, "parse failed\n"); return 1; }

    xmlNode *root = xmlDocGetRootElement(doc);
    const xmlChar *name_before = ((xmlNode_min *) root)->name;
    if (!name_before || strcmp((const char *) name_before, "root") != 0) {
        fprintf(stderr, "name_before is wrong\n");
        xmlFreeDoc(doc); return 2;
    }

    /* xmlFree on an arena pointer must NOT crash or free it.  We pass
     * the const-qualified pointer through a void* cast. */
    xmlFree((void *) name_before);

    /* Re-read: the pointer must still resolve to the same bytes. */
    const xmlChar *name_after = ((xmlNode_min *) root)->name;
    if (name_after != name_before) {
        fprintf(stderr, "name pointer changed\n");
        xmlFreeDoc(doc); return 3;
    }
    if (!name_after || strcmp((const char *) name_after, "root") != 0) {
        fprintf(stderr, "name_after corrupted: %s\n",
                name_after ? (const char *) name_after : "(null)");
        xmlFreeDoc(doc); return 4;
    }

    /* xmlFree on NULL is always a no-op. */
    xmlFree(NULL);

    xmlFreeDoc(doc);
    printf("T-MEM-02 OK\n");
    return 0;
}
