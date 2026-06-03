/* T-ERR-02: xmlSetStructuredErrorFunc invokes the registered handler.
 *
 * From the test inventory:
 *   T-ERR-01  structured error callback
 *             xmlSetStructuredErrorFunc, parse malformed input
 *             → callback invoked with domain/level/code/line/column set
 *
 * Steps:
 *   1. Register a structured error callback that captures `code`.
 *   2. Trigger a parse failure.
 *   3. Verify the callback was invoked AND captured the right code.
 */

#include <stdio.h>
#include <stddef.h>

typedef struct _xmlError {
    int    domain;
    int    code;
    char  *message;
    int    level;
    int    _pad_level;
    char  *file;
    int    line;
    int    _pad_line;
    char  *str1;
    char  *str2;
    char  *str3;
    int    int1;
    int    int2;
    void  *ctxt;
    void  *node;
} xmlError;

typedef void (*xmlStructuredErrorFunc)(void *user_data, const xmlError *err);
extern void xmlSetStructuredErrorFunc(void *user_data, xmlStructuredErrorFunc fn);
extern void xmlResetLastError(void);

typedef struct _xmlDoc xmlDoc;
extern xmlDoc *xmlReadMemory(const char *, int, const char *, const char *, int);
extern void    xmlFreeDoc(xmlDoc *);

static void trigger_parse_failure(const char *buf, size_t len) {
    xmlDoc *d = xmlReadMemory(buf, (int) len, NULL, NULL, 0);
    if (d) xmlFreeDoc(d);  /* shouldn't happen on malformed input */
}

static int g_captured_code = 0;
static int g_captured_domain = 0;
static int g_callbacks_seen = 0;
static void *g_seen_user_data = NULL;

static void capture_handler(void *user_data, const xmlError *err) {
    g_captured_code = err->code;
    g_captured_domain = err->domain;
    g_seen_user_data = user_data;
    g_callbacks_seen++;
}

int main(void) {
    xmlResetLastError();

    int sentinel = 42;
    xmlSetStructuredErrorFunc(&sentinel, capture_handler);

    const char malformed[] = "<1foo/>";
    trigger_parse_failure(malformed, sizeof(malformed) - 1);

    if (g_callbacks_seen != 1) {
        fprintf(stderr, "FAIL: expected 1 callback, got %d\n", g_callbacks_seen);
        return 1;
    }
    if (g_seen_user_data != &sentinel) {
        fprintf(stderr, "FAIL: user_data not passed through\n");
        return 1;
    }
    if (g_captured_code != 68 /* NAME_REQUIRED */) {
        fprintf(stderr, "FAIL: expected code 68, got %d\n", g_captured_code);
        return 1;
    }
    if (g_captured_domain != 1 /* Parser */) {
        fprintf(stderr, "FAIL: expected domain 1, got %d\n", g_captured_domain);
        return 1;
    }

    /* Unregister; another error should NOT call the handler. */
    xmlSetStructuredErrorFunc(NULL, NULL);
    trigger_parse_failure(malformed, sizeof(malformed) - 1);
    if (g_callbacks_seen != 1) {
        fprintf(stderr, "FAIL: handler still fired after unregister, count=%d\n",
                g_callbacks_seen);
        return 1;
    }

    printf("T-ERR-02 OK\n");
    return 0;
}
