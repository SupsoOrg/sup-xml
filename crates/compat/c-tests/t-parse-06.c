/* T-PARSE-06: malformed XML → xmlReadMemory returns NULL, and the
 * last-error slot is populated with a non-zero code (so callers can
 * distinguish "successful parse of empty doc" from "parse failed").
 *
 * Functions exercised:
 *   xmlReadMemory          (the error-path)
 *   xmlGetLastError
 *   xmlResetLastError
 */

#include <stdio.h>
#include <stddef.h>
#include <string.h>

typedef struct _xmlDoc xmlDoc;

/* xmlError layout — must match the Rust-side `xmlError` struct.
 * T-ERR-03 verifies these offsets via _Static_assert; we just need a
 * declaration here that reads the fields we care about. */
typedef struct _xmlError {
    int   domain;
    int   code;
    char *message;
    int   level;
    int   _pad_level;
    char *file;
    int   line;
    int   _pad_line;
    char *str1;
    char *str2;
    char *str3;
    int   int1;
    int   int2;
    void *ctxt;
    void *node;
} xmlError;

extern xmlDoc *xmlReadMemory(const char *buffer, int size,
                              const char *url, const char *encoding,
                              int options);
extern void    xmlFreeDoc(xmlDoc *doc);
extern const xmlError *xmlGetLastError(void);
extern void    xmlResetLastError(void);

int main(void) {
    /* Clean slate. */
    xmlResetLastError();
    if (xmlGetLastError() != NULL) {
        fprintf(stderr, "expected no last-error after reset\n");
        return 1;
    }

    /* Unclosed tag — well-formedness violation, must be rejected. */
    const char *bad = "<r>oops";
    xmlDoc *doc = xmlReadMemory(bad, (int) strlen(bad), NULL, NULL, 0);
    if (doc != NULL) {
        fprintf(stderr, "expected NULL doc for malformed input, got non-NULL\n");
        xmlFreeDoc(doc);
        return 2;
    }

    const xmlError *err = xmlGetLastError();
    if (err == NULL) {
        fprintf(stderr, "expected last-error to be populated\n");
        return 3;
    }
    if (err->code == 0) {
        fprintf(stderr, "expected non-zero error code, got 0\n");
        return 4;
    }
    if (err->level == 0) {
        fprintf(stderr, "expected non-zero error level, got 0\n");
        return 5;
    }
    if (err->message == NULL || err->message[0] == '\0') {
        fprintf(stderr, "expected non-empty error message\n");
        return 6;
    }

    /* Reset should clear. */
    xmlResetLastError();
    if (xmlGetLastError() != NULL) {
        fprintf(stderr, "expected no last-error after reset\n");
        return 7;
    }

    printf("T-PARSE-06 OK\n");
    return 0;
}
