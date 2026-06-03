/* T-LINK-03: symbols that ABSOLUTELY must not leak from the cdylib.
 *
 * Pairs with T-LINK-01 (which asserts our 250-ish libxml2 ABI
 * symbols ARE present).  This test guards the OTHER direction —
 * symbols the version script (`src/symbols.{txt,ld}`) should be
 * hiding.
 *
 * Since v0.1 ships *stub* implementations for every libxml2 symbol
 * lxml references (so `import lxml` doesn't fail to link), the
 * "absent" list isn't libxml2 names — those are all in the export
 * set on purpose.  Instead we check that:
 *
 *   1. Rust standard library symbols don't leak (e.g. `__rust_alloc`,
 *      `_rust_panic`, `__rdl_*` allocator hooks).
 *   2. Random non-libxml2 names don't resolve (smoke check that
 *      `dlsym` returning NULL actually means "not exported", not
 *      "always succeeds").
 *
 * If any of these resolve, the version script regressed.
 */

#include <stdio.h>
#include <stdlib.h>
#include <dlfcn.h>

static const char *ABSENT[] = {
    /* Rust runtime — should never be visible to C consumers. */
    "__rust_alloc",
    "__rust_dealloc",
    "__rust_realloc",
    "__rust_alloc_zeroed",
    "rust_eh_personality",
    "rust_panic",
    /* Rust stdlib internals (mangled or near-public names). */
    "__rdl_alloc",
    "__rdl_dealloc",
    "_ZN3std9panicking",
    /* Smoke check: a completely fictional name. */
    "this_function_definitely_does_not_exist",
    "fnord_xyzzy_42",
};

#if defined(__APPLE__)
#  define LIBNAME "libsup_xml_compat.dylib"
#else
#  define LIBNAME "libsup_xml_compat.so"
#endif

int main(void) {
    void *h = dlopen(LIBNAME, RTLD_NOW);
    if (!h) {
        fprintf(stderr, "dlopen(%s) failed: %s\n", LIBNAME, dlerror());
        return 1;
    }

    int n = (int) (sizeof(ABSENT) / sizeof(ABSENT[0]));
    int leaked = 0;
    for (int i = 0; i < n; ++i) {
        /* Clear any prior dlerror so we can distinguish "symbol
         * absent" from "previous error".  POSIX requires this dance:
         * dlsym returning NULL is only meaningful if dlerror() also
         * returns non-NULL afterwards (NULL is a legitimate return
         * value if a symbol genuinely points there). */
        (void) dlerror();
        void *p = dlsym(h, ABSENT[i]);
        const char *e = dlerror();
        if (p != NULL || e == NULL) {
            fprintf(stderr, "LEAKED: %s resolved to %p (no dlerror)\n",
                    ABSENT[i], p);
            ++leaked;
        }
    }
    dlclose(h);

    if (leaked > 0) {
        fprintf(stderr, "%d/%d unimplemented symbols leaked\n", leaked, n);
        return 2;
    }

    printf("T-LINK-03 OK\n");
    return 0;
}
