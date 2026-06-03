/* T-SER-01: parse → dump → reparse round-trip.
 *
 * The dump byte stream must stabilise after one cycle — i.e. dumping
 * a reparsed document produces the same bytes as the original dump.
 */

#include <stdio.h>
#include <string.h>

typedef struct _xmlDoc xmlDoc;
typedef unsigned char  xmlChar;

extern xmlDoc *xmlReadMemory(const char *, int, const char *, const char *, int);
extern void    xmlFreeDoc(xmlDoc *);
extern void    xmlDocDumpMemory(const xmlDoc *, xmlChar **, int *);
/* `xmlFree` is a global function pointer in libxml2's headers, not a
 * function — callers compile to `LDR + BLR` instead of a direct `BL`.
 * Forward-declaring it as a function would emit the wrong call shape
 * and jump into the data segment at runtime. */
typedef void (*xmlFreeFunc)(void *mem);
extern xmlFreeFunc xmlFree;

int main(void) {
    const char *src = "<r><a id=\"42\"/><b>text<c/></b></r>";
    xmlDoc *d1 = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (!d1) { fprintf(stderr, "parse1 failed\n"); return 1; }

    xmlChar *m1 = NULL; int s1 = 0;
    xmlDocDumpMemory(d1, &m1, &s1);
    if (!m1 || s1 <= 0) {
        fprintf(stderr, "dump1 failed\n");
        xmlFreeDoc(d1); return 2;
    }

    /* Reparse the dump.  Feed size1 bytes (NUL not included). */
    xmlDoc *d2 = xmlReadMemory((const char *) m1, s1, NULL, NULL, 0);
    if (!d2) {
        fprintf(stderr, "reparse failed\n");
        xmlFree(m1); xmlFreeDoc(d1); return 3;
    }

    xmlChar *m2 = NULL; int s2 = 0;
    xmlDocDumpMemory(d2, &m2, &s2);
    if (!m2 || s2 != s1 || memcmp(m1, m2, s1) != 0) {
        fprintf(stderr, "round-trip dumps differ:\n  s1=%d s2=%d\n  m1=\"%s\"\n  m2=\"%s\"\n",
                s1, s2, (const char *) m1, (const char *) m2);
        xmlFree(m1); xmlFree(m2); xmlFreeDoc(d1); xmlFreeDoc(d2); return 4;
    }

    xmlFree(m1);
    xmlFree(m2);
    xmlFreeDoc(d1);
    xmlFreeDoc(d2);
    printf("T-SER-01 OK\n");
    return 0;
}
