/* T-ERR-01: xmlGetLastError + xmlResetLastError round-trip.
 *
 * From the test inventory:
 *   T-ERR-02  xmlGetLastError + xmlResetLastError
 *             → reset clears subsequent reads to NULL
 *
 * (Numbered 01 here as the first ERR test in the layer.)
 *
 * Steps:
 *   1. Fresh state — xmlGetLastError() should return NULL.
 *   2. Trigger a parse failure on malformed XML via the test stub.
 *   3. xmlGetLastError() returns non-NULL with the expected fields.
 *   4. xmlResetLastError() clears.
 *   5. xmlGetLastError() returns NULL again.
 */

#include <stdio.h>
#include <stddef.h>
#include <string.h>

/* Minimal subset of libxml2's xmlError layout — declared here so we
 * don't yet need vendored headers.  Field offsets must match what
 * `crates/compat/src/error.rs` exports; the const_offsetof asserts
 * in that file guarantee it.  */
typedef struct _xmlError {
    int    domain;        /* 0  */
    int    code;          /* 4  */
    char  *message;       /* 8  */
    int    level;         /* 16 */
    int    _pad_level;    /* 20 */
    char  *file;          /* 24 */
    int    line;          /* 32 */
    int    _pad_line;     /* 36 */
    char  *str1;          /* 40 */
    char  *str2;          /* 48 */
    char  *str3;          /* 56 */
    int    int1;          /* 64 */
    int    int2;          /* 68 — column */
    void  *ctxt;          /* 72 */
    void  *node;          /* 80 */
} xmlError;

extern const xmlError *xmlGetLastError(void);
extern void xmlResetLastError(void);

typedef struct _xmlDoc xmlDoc;
extern xmlDoc *xmlReadMemory(const char *, int, const char *, const char *, int);
extern void    xmlFreeDoc(xmlDoc *);

int main(void) {
    /* 1. Fresh state. */
    if (xmlGetLastError() != NULL) {
        fprintf(stderr, "FAIL: xmlGetLastError() expected NULL initially\n");
        return 1;
    }

    /* 2. Trigger a failure: `<1foo/>` — name starts with digit. */
    const char malformed[] = "<1foo/>";
    xmlDoc *doc = xmlReadMemory(malformed, (int)(sizeof(malformed) - 1),
                                NULL, NULL, 0);
    if (doc != NULL) {
        fprintf(stderr, "FAIL: malformed input should have failed parse\n");
        xmlFreeDoc(doc);
        return 1;
    }

    /* 3. Inspect. */
    const xmlError *e = xmlGetLastError();
    if (e == NULL) {
        fprintf(stderr, "FAIL: xmlGetLastError() unexpectedly NULL after failure\n");
        return 1;
    }
    if (e->domain != 1 /* XML_FROM_PARSER */) {
        fprintf(stderr, "FAIL: domain expected 1 (Parser), got %d\n", e->domain);
        return 1;
    }
    if (e->code != 68 /* XML_ERR_NAME_REQUIRED */) {
        fprintf(stderr, "FAIL: code expected 68 (NAME_REQUIRED), got %d\n", e->code);
        return 1;
    }
    if (e->level != 3 /* XML_ERR_FATAL */) {
        fprintf(stderr, "FAIL: level expected 3 (Fatal), got %d\n", e->level);
        return 1;
    }
    if (e->message == NULL) {
        fprintf(stderr, "FAIL: message unexpectedly NULL\n");
        return 1;
    }
    if (strstr(e->message, "name-start") == NULL) {
        fprintf(stderr, "FAIL: message expected to contain 'name-start', got: %s\n", e->message);
        return 1;
    }

    /* 4. Reset. */
    xmlResetLastError();

    /* 5. Cleared. */
    if (xmlGetLastError() != NULL) {
        fprintf(stderr, "FAIL: xmlGetLastError() should be NULL after reset\n");
        return 1;
    }

    printf("T-ERR-01 OK\n");
    return 0;
}
