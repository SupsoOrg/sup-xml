/* T-MEM-03: xmlMemSetup actually routes allocations through the
 * caller's hooks.
 *
 * The Rust unit tests in `crates/compat/src/alloc.rs` install
 * counter-bumping hooks via xmlMemSetup and verify the counters
 * fire — but they reach the static-mut globals (`xmlMalloc` /
 * `xmlFree`) through their Rust-mangled paths.  Only a C program
 * that LDR/BLRs the bare global symbol catches the case where
 * the fn-pointer global isn't being written, or isn't being
 * dispatched through, in the libxml2-compat way.
 *
 * Functions exercised:
 *   xmlMemSetup    (caller installs allocator hooks)
 *   xmlMemGet      (snapshot the defaults for restore)
 *   xmlMalloc      (dispatch through the installed hook)
 *   xmlFree        (same)
 *
 * Asserts:
 *   - xmlMemSetup with all three required hooks returns 0
 *   - a direct call through the `xmlMalloc` fn-ptr global hits
 *     our test_malloc, not the cdylib's internal allocator
 *   - the corresponding xmlFree call hits test_free
 *   - xmlMemSetup with NULL malloc returns -1 (validation)
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* libxml2 allocator hook types (xmlmemory.h). */
typedef void  (*xmlFreeFunc)   (void *mem);
typedef void *(*xmlMallocFunc) (size_t size);
typedef void *(*xmlReallocFunc)(void *mem, size_t size);
typedef char *(*xmlStrdupFunc) (const char *str);

/* libxml2's allocator entry points are *function-pointer globals*,
 * not functions — we read+call through them, never `BL` straight. */
extern xmlMallocFunc  xmlMalloc;
extern xmlReallocFunc xmlRealloc;
extern xmlFreeFunc    xmlFree;

extern int xmlMemSetup(xmlFreeFunc, xmlMallocFunc, xmlReallocFunc, xmlStrdupFunc);
extern int xmlMemGet  (xmlFreeFunc *, xmlMallocFunc *, xmlReallocFunc *, xmlStrdupFunc *);

/* Counters bumped by our hooks. */
static int test_malloc_calls = 0;
static int test_free_calls   = 0;

static void *test_malloc(size_t size) {
    test_malloc_calls++;
    return malloc(size);
}

static void *test_realloc(void *p, size_t size) {
    return realloc(p, size);
}

static void test_free(void *p) {
    test_free_calls++;
    free(p);
}

int main(void) {
    /* Validation: NULL malloc must be rejected and leave the
     * defaults unchanged.  Verify by ensuring a follow-up successful
     * setup still works. */
    int rc = xmlMemSetup(test_free, NULL, test_realloc, NULL);
    if (rc != -1) {
        fprintf(stderr, "xmlMemSetup with NULL malloc returned %d; expected -1\n", rc);
        return 1;
    }

    /* Snapshot the defaults for restore at the end. */
    xmlFreeFunc    saved_free    = NULL;
    xmlMallocFunc  saved_malloc  = NULL;
    xmlReallocFunc saved_realloc = NULL;
    xmlMemGet(&saved_free, &saved_malloc, &saved_realloc, NULL);
    if (!saved_free || !saved_malloc || !saved_realloc) {
        fprintf(stderr, "xmlMemGet returned NULL hooks before any swap\n");
        return 2;
    }

    /* Install our hooks. */
    rc = xmlMemSetup(test_free, test_malloc, test_realloc, NULL);
    if (rc != 0) {
        fprintf(stderr, "xmlMemSetup returned %d; expected 0\n", rc);
        return 3;
    }

    /* Allocate + free through the dispatch globals.  After the swap
     * these LDR/BLR sequences must land in our test_malloc / test_free,
     * not the cdylib's internal impl_xml_malloc. */
    void *p = xmlMalloc(64);
    if (p == NULL) {
        fprintf(stderr, "xmlMalloc(64) returned NULL via test hook\n");
        return 4;
    }
    xmlFree(p);

    if (test_malloc_calls != 1) {
        fprintf(stderr, "test_malloc was called %d times; expected 1\n",
                test_malloc_calls);
        return 5;
    }
    if (test_free_calls != 1) {
        fprintf(stderr, "test_free was called %d times; expected 1\n",
                test_free_calls);
        return 6;
    }

    /* Restore defaults — clean hand-off so cdylib teardown doesn't
     * try to free through our (about-to-be-popped) test_free stack
     * frame.  Not strictly necessary in a one-shot process, but
     * matches the contract we'd want a real consumer to honour. */
    rc = xmlMemSetup(saved_free, saved_malloc, saved_realloc, NULL);
    if (rc != 0) {
        fprintf(stderr, "restore xmlMemSetup returned %d; expected 0\n", rc);
        return 7;
    }

    printf("T-MEM-03 OK\n");
    return 0;
}
