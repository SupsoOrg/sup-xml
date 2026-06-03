// Thin C wrapper around expat (libexpat) for benchmarking.
//
// expat is a SAX/push parser — it doesn't build a DOM.  To make the
// comparison fair against sup-xml's DOM-building parsers, the shim
// runs expat with minimal callbacks that just COUNT events.  This
// represents the "parser-only, no tree allocation" performance ceiling.
//
//   expat_bench_parse_count(buf, len) -> element count, or SIZE_MAX on error.
//
// The bench uses the count as black-box to prevent the optimiser from
// eliding the parse — same role as `walk_count` for pugixml.

#include <expat.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

struct counter {
    size_t element_count;
    size_t err;
};

static void XMLCALL on_start_element(void* userData, const XML_Char* name, const XML_Char** attrs) {
    (void)name;
    (void)attrs;
    struct counter* c = (struct counter*)userData;
    c->element_count += 1;
}

static void XMLCALL on_end_element(void* userData, const XML_Char* name) {
    (void)userData;
    (void)name;
}

static void XMLCALL on_character_data(void* userData, const XML_Char* s, int len) {
    (void)userData;
    (void)s;
    (void)len;
}

size_t expat_bench_parse_count(const char* buf, size_t len) {
    XML_Parser p = XML_ParserCreate(NULL);
    if (!p) return SIZE_MAX;
    struct counter c = { 0, 0 };
    XML_SetUserData(p, &c);
    XML_SetElementHandler(p, on_start_element, on_end_element);
    XML_SetCharacterDataHandler(p, on_character_data);
    // Feed the whole buffer in one call (is_final=1).
    if (XML_Parse(p, buf, (int)len, 1) == XML_STATUS_ERROR) {
        XML_ParserFree(p);
        return SIZE_MAX;
    }
    XML_ParserFree(p);
    return c.element_count;
}

// Memory probe: expat exposes XML_MemMalloc/Realloc/Free; we can't easily
// hook them without rebuilding expat, but we can ask expat for its
// peak-memory if available.  In 2.x expat doesn't expose that directly,
// so the bench reports the parser's working-set indirectly via the
// caller (getrusage).
