/* T-RECOVER-01: XML_PARSE_RECOVER returns a partial tree on
 * deliberately-malformed input.
 *
 * The compat translator (`map_libxml2_options` in compat/parse.rs)
 * grew NEW wiring this session for RECOVER, NOENT, DTDVALID, and
 * NOBLANKS.  Prior behaviour: the bit was silently dropped and
 * every C caller passing XML_PARSE_RECOVER got NULL on malformed
 * input — same as if they'd passed options=0.  This test would
 * have failed against that older code; passing now is the
 * regression gate.
 *
 * Functions exercised:
 *   xmlReadMemory (with options=XML_PARSE_RECOVER)
 *   xmlDocGetRootElement
 *   xmlFreeDoc
 *
 * Asserts:
 *   - options=0  on malformed `<r><a></r>` → returns NULL (strict reject)
 *   - options=XML_PARSE_RECOVER on the same input → returns non-NULL
 *     doc with a discoverable root element
 *
 * Why a C test (vs the existing `options_audit` bench): the bench
 * reaches `parse_bytes` through the Rust translator copy.  Only a
 * binary that goes through `xmlReadMemory` via the cdylib's
 * `#[no_mangle]` export confirms the entry point honours the bit
 * end-to-end — including the parsectx integration with
 * xmlCtxtUseOptions that re-uses the same translator.
 */

#include <stdio.h>
#include <stddef.h>
#include <string.h>

/* libxml2 XML_PARSE_* bit (parser.h). */
#define XML_PARSE_RECOVER (1 << 0)

typedef struct _xmlDoc  xmlDoc;
typedef struct _xmlNode xmlNode;

extern xmlDoc  *xmlReadMemory(const char *buffer, int size,
                              const char *url, const char *encoding,
                              int options);
extern void     xmlFreeDoc(xmlDoc *doc);
extern xmlNode *xmlDocGetRootElement(const xmlDoc *doc);

int main(void) {
    /* Mismatched end tag — well-formedness violation that libxml2's
     * strict path rejects but its recovery path partially recovers. */
    const char *bad = "<r><a></r>";

    /* Sub-test 1: strict mode rejects. */
    xmlDoc *strict = xmlReadMemory(bad, (int) strlen(bad), NULL, NULL, 0);
    if (strict != NULL) {
        fprintf(stderr, "strict xmlReadMemory accepted malformed input\n");
        xmlFreeDoc(strict);
        return 1;
    }

    /* Sub-test 2: recovery mode accepts.  Pre-fix this would still
     * return NULL because the bit was being silently dropped. */
    xmlDoc *recovered = xmlReadMemory(bad, (int) strlen(bad), NULL, NULL,
                                      XML_PARSE_RECOVER);
    if (recovered == NULL) {
        fprintf(stderr,
                "xmlReadMemory(XML_PARSE_RECOVER) returned NULL — "
                "compat translator may be dropping the bit\n");
        return 2;
    }

    /* Recovered docs must still have a reachable root element. */
    xmlNode *root = xmlDocGetRootElement(recovered);
    if (root == NULL) {
        fprintf(stderr, "recovered doc has no root element\n");
        xmlFreeDoc(recovered);
        return 3;
    }

    xmlFreeDoc(recovered);

    printf("T-RECOVER-01 OK\n");
    return 0;
}
