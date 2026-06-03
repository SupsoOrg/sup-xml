/* T-PARSE-11: xmlInitParser is idempotent.
 *
 * Multi-threaded coverage lives in the Rust unit test
 * `init::tests::init_is_thread_safe`; the C version here just
 * exercises the linkage + single-thread idempotency contract.
 */

#include <stdio.h>
#include <string.h>

typedef struct _xmlDoc xmlDoc;

extern void xmlInitParser(void);
extern void xmlCleanupParser(void);
extern xmlDoc *xmlReadMemory(const char *, int, const char *, const char *, int);
extern void xmlFreeDoc(xmlDoc *);

int main(void) {
    /* Call N times in a row: must not crash, must succeed for
     * parsing afterwards. */
    for (int i = 0; i < 100; ++i) {
        xmlInitParser();
    }

    const char *src = "<r/>";
    xmlDoc *doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (!doc) { fprintf(stderr, "parse failed after init\n"); return 1; }
    xmlFreeDoc(doc);

    /* Cleanup then re-init works. */
    xmlCleanupParser();
    xmlInitParser();
    doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (!doc) { fprintf(stderr, "parse failed after cleanup+reinit\n"); return 2; }
    xmlFreeDoc(doc);

    printf("T-PARSE-11 OK\n");
    return 0;
}
