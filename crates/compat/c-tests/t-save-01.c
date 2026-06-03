/* T-SAVE-01: round-trip a parsed doc through the xmlSave* streaming
 * serializer, then read the file back and verify the bytes.
 *
 * Functions exercised:
 *   xmlReadMemory       (smoke test from t-parse-01)
 *   xmlSaveToFilename   (new this session)
 *   xmlSaveDoc          (new this session)
 *   xmlSaveClose        (new this session)
 *   xmlFreeDoc
 *
 * Asserts:
 *   - xmlSaveToFilename returns a non-NULL context
 *   - xmlSaveDoc returns a positive byte count
 *   - xmlSaveClose returns the cumulative byte count, not -1
 *   - the resulting file is readable, non-empty, and contains the
 *     element we wrote (we don't assert byte-exact output because
 *     attribute ordering and self-close style are serializer-
 *     internal choices that needn't match libxml2)
 *
 * Why a C test (vs the existing Rust unit tests in save.rs): the
 * Rust tests reach `xmlSaveToFilename` through `crate::save::*`
 * which still resolves under `cdylib-exports = off`.  Only a C
 * program that dlopens the cdylib actually exercises the
 * `#[no_mangle]` path — if a future refactor accidentally drops
 * the gate on these functions, this test fails at link time.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

typedef struct _xmlDoc      xmlDoc;
typedef struct _xmlSaveCtxt xmlSaveCtxt;

extern xmlDoc      *xmlReadMemory(const char *buffer, int size,
                                  const char *url, const char *encoding,
                                  int options);
extern void         xmlFreeDoc(xmlDoc *doc);
extern xmlSaveCtxt *xmlSaveToFilename(const char *filename,
                                     const char *encoding,
                                     int options);
extern long         xmlSaveDoc(xmlSaveCtxt *ctxt, xmlDoc *doc);
extern int          xmlSaveClose(xmlSaveCtxt *ctxt);

int main(void) {
    /* Build a per-process temp path so concurrent test runs don't
     * collide.  Test cleanup unlinks on every exit path. */
    char path[256];
    snprintf(path, sizeof path, "/tmp/t-save-01.%d.xml", (int) getpid());

    const char *src = "<root><a/><b/></root>";
    xmlDoc *doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (doc == NULL) {
        fprintf(stderr, "xmlReadMemory failed on valid input\n");
        return 1;
    }

    xmlSaveCtxt *ctx = xmlSaveToFilename(path, NULL, 0);
    if (ctx == NULL) {
        fprintf(stderr, "xmlSaveToFilename returned NULL\n");
        xmlFreeDoc(doc);
        return 2;
    }

    long written = xmlSaveDoc(ctx, doc);
    if (written <= 0) {
        fprintf(stderr, "xmlSaveDoc returned %ld; expected > 0\n", written);
        xmlSaveClose(ctx);
        xmlFreeDoc(doc);
        unlink(path);
        return 3;
    }

    int total = xmlSaveClose(ctx);
    if (total < 0) {
        fprintf(stderr, "xmlSaveClose returned %d (latched error)\n", total);
        xmlFreeDoc(doc);
        unlink(path);
        return 4;
    }

    /* Read the file back and confirm it contains the elements we
     * wrote.  Byte-exact comparison would be too strict — we don't
     * commit to libxml2's specific formatting choices. */
    FILE *f = fopen(path, "rb");
    if (f == NULL) {
        fprintf(stderr, "fopen(%s) failed after xmlSaveClose\n", path);
        xmlFreeDoc(doc);
        unlink(path);
        return 5;
    }
    char buf[4096] = {0};
    size_t n = fread(buf, 1, sizeof(buf) - 1, f);
    fclose(f);
    if (n == 0) {
        fprintf(stderr, "saved file is empty\n");
        xmlFreeDoc(doc);
        unlink(path);
        return 6;
    }
    if (strstr(buf, "<root") == NULL ||
        strstr(buf, "<a")    == NULL ||
        strstr(buf, "<b")    == NULL)
    {
        fprintf(stderr, "saved file missing expected elements; got %.200s\n", buf);
        xmlFreeDoc(doc);
        unlink(path);
        return 7;
    }

    xmlFreeDoc(doc);
    unlink(path);

    printf("T-SAVE-01 OK\n");
    return 0;
}
