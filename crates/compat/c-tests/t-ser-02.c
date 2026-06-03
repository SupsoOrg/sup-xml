/* T-SER-02: xmlDocDumpFormatMemory(format=1) produces indented output
 * with newlines between children.
 */

#include <stdio.h>
#include <string.h>

typedef struct _xmlDoc xmlDoc;
typedef unsigned char  xmlChar;

extern xmlDoc *xmlReadMemory(const char *, int, const char *, const char *, int);
extern void    xmlFreeDoc(xmlDoc *);
extern void    xmlDocDumpFormatMemory(const xmlDoc *, xmlChar **, int *, int);
/* `xmlFree` is a global function pointer in libxml2's headers, not a
 * function — callers compile to `LDR + BLR` instead of a direct `BL`.
 * Forward-declaring it as a function would emit the wrong call shape
 * and jump into the data segment at runtime. */
typedef void (*xmlFreeFunc)(void *mem);
extern xmlFreeFunc xmlFree;

int main(void) {
    const char *src = "<r><a/><b/></r>";
    xmlDoc *doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (!doc) { fprintf(stderr, "parse failed\n"); return 1; }

    xmlChar *mem = NULL; int size = 0;
    xmlDocDumpFormatMemory(doc, &mem, &size, /*format=*/1);
    if (!mem || size <= 0) {
        fprintf(stderr, "formatted dump failed\n");
        xmlFreeDoc(doc); return 2;
    }

    if (strchr((const char *) mem, '\n') == NULL) {
        fprintf(stderr, "formatted dump should contain newlines: %s\n",
                (const char *) mem);
        xmlFree(mem); xmlFreeDoc(doc); return 3;
    }

    xmlFree(mem);
    xmlFreeDoc(doc);
    printf("T-SER-02 OK\n");
    return 0;
}
