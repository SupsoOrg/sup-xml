/* T-LINK-01: every declared symbol resolves via dlsym.
 *
 * The list below mirrors the function surface this crate exports as
 * of Tier 1.  When a new function is added (xmlReadFd, xmlNewDoc,
 * etc.) it must be added here too — the test then forces a fresh
 * dlsym check, catching cases where Rust's no_mangle attribute was
 * forgotten or where a version script accidentally hides a symbol.
 *
 * Sister test T-LINK-03 (not yet implemented) will assert that
 * *unimplemented* libxml2 symbols are ABSENT.  That needs a linker
 * version script — follow-up work.
 */

#include <stdio.h>
#include <stdlib.h>
#include <dlfcn.h>
#include <string.h>

static const char *EXPECTED[] = {
    /* init */
    "xmlInitParser",
    "xmlCleanupParser",
    /* parse */
    "xmlReadMemory",
    "xmlFreeDoc",
    /* tree walking */
    "xmlDocGetRootElement",
    "xmlFirstElementChild",
    "xmlLastElementChild",
    "xmlNextElementSibling",
    "xmlPreviousElementSibling",
    "xmlChildElementCount",
    "xmlNodeGetContent",
    /* attrs */
    "xmlGetProp",
    "xmlGetNoNsProp",
    "xmlGetNsProp",
    "xmlHasProp",
    /* namespace lookups */
    "xmlSearchNs",
    "xmlSearchNsByHref",
    /* serialization */
    "xmlDocDumpMemory",
    "xmlDocDumpFormatMemory",
    "xmlNodeDump",
    /* errors */
    "xmlGetLastError",
    "xmlResetLastError",
    "xmlResetError",
    "xmlSetStructuredErrorFunc",
    "xmlSetGenericErrorFunc",
    /* allocator */
    "xmlFree",
};

#if defined(__APPLE__)
#  define LIBNAME "libsup_xml_compat.dylib"
#else
#  define LIBNAME "libsup_xml_compat.so"
#endif

int main(void) {
    /* RTLD_NOW so we fail fast if any symbol is missing.  rpath was
     * baked into the test binary by the cc invocation in
     * `crates/compat/tests/abi.rs`, so dlopen picks up the right
     * cdylib without needing DYLD_/LD_LIBRARY_PATH at this point. */
    void *h = dlopen(LIBNAME, RTLD_NOW);
    if (!h) {
        fprintf(stderr, "dlopen(%s) failed: %s\n", LIBNAME, dlerror());
        return 1;
    }

    int n = (int) (sizeof(EXPECTED) / sizeof(EXPECTED[0]));
    int missing = 0;
    for (int i = 0; i < n; ++i) {
        void *p = dlsym(h, EXPECTED[i]);
        if (!p) {
            fprintf(stderr, "MISSING: %s\n", EXPECTED[i]);
            ++missing;
        }
    }
    dlclose(h);

    if (missing > 0) {
        fprintf(stderr, "%d/%d symbols missing\n", missing, n);
        return 2;
    }

    printf("T-LINK-01 OK\n");
    return 0;
}
