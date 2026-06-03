/* t-libxml2-headers: a "real" libxml2 client compiled against the
 * actual libxml2 headers — but linked against `libsup_xml_compat`
 * instead of `libxml2`.  If our struct layouts and function ABIs are
 * byte-exact, this works without any code modification.  If they're
 * off, the test fails loudly: `doc->children` reads the wrong offset,
 * `xmlGetProp` returns garbage, etc.
 *
 * This is the most authentic possible test — it's literally a libxml2
 * user that doesn't know they're running on our shim.
 *
 * The test harness in `tests/abi.rs` injects `-I<sdk>/usr/include`
 * (or `pkg-config --cflags libxml-2.0` on Linux) so that <libxml/...>
 * headers are visible.  Linkage is `-lsup_xml_compat` only — the
 * shim's exports satisfy every symbol declared by those headers
 * that we use here.
 *
 * If you add a libxml2 function to this test that we don't yet
 * implement, you'll get a clean link error — which is the contract
 * T-LINK-03 protects.
 */

#include <stdio.h>
#include <string.h>
#include <libxml/parser.h>
#include <libxml/tree.h>
#include <libxml/xmlerror.h>

static int check_parse_walk(void) {
    const char *src =
        "<?xml version=\"1.0\"?>\n"
        "<catalog xmlns=\"urn:demo\">\n"
        "  <!-- a comment -->\n"
        "  <book id=\"42\">\n"
        "    <title>Hello</title>\n"
        "    <author>World</author>\n"
        "  </book>\n"
        "</catalog>";

    xmlDocPtr doc = xmlReadMemory(src, (int) strlen(src), NULL, NULL, 0);
    if (!doc) {
        fprintf(stderr, "xmlReadMemory failed\n");
        return 1;
    }

    xmlNodePtr root = xmlDocGetRootElement(doc);
    if (!root || strcmp((const char *) root->name, "catalog") != 0) {
        fprintf(stderr, "root->name expected \"catalog\", got %s\n",
                root ? (const char *) root->name : "(null)");
        xmlFreeDoc(doc);
        return 2;
    }

    /* Default namespace must be in scope on the root. */
    if (!root->ns || !root->ns->href ||
        strcmp((const char *) root->ns->href, "urn:demo") != 0) {
        fprintf(stderr, "root->ns->href mismatch\n");
        xmlFreeDoc(doc);
        return 3;
    }

    /* Find <book> via element-aware iteration. */
    xmlNodePtr book = xmlFirstElementChild(root);
    if (!book || strcmp((const char *) book->name, "book") != 0) {
        fprintf(stderr, "first element child expected \"book\"\n");
        xmlFreeDoc(doc);
        return 4;
    }

    /* Read attribute. */
    xmlChar *id = xmlGetProp(book, (const xmlChar *) "id");
    if (!id || strcmp((const char *) id, "42") != 0) {
        fprintf(stderr, "book@id expected \"42\", got %s\n",
                id ? (const char *) id : "(null)");
        xmlFree(id);
        xmlFreeDoc(doc);
        return 5;
    }
    xmlFree(id);

    /* Concatenated text content. */
    xmlChar *text = xmlNodeGetContent(book);
    if (!text || strstr((const char *) text, "Hello") == NULL ||
                 strstr((const char *) text, "World") == NULL) {
        fprintf(stderr, "book content missing expected substrings: %s\n",
                text ? (const char *) text : "(null)");
        xmlFree(text);
        xmlFreeDoc(doc);
        return 6;
    }
    xmlFree(text);

    /* Element count. */
    unsigned long count = xmlChildElementCount(book);
    if (count != 2) {
        fprintf(stderr, "book has %lu element children, expected 2\n", count);
        xmlFreeDoc(doc);
        return 7;
    }

    /* Round-trip dump. */
    xmlChar *out = NULL;
    int out_size = 0;
    xmlDocDumpMemory(doc, &out, &out_size);
    if (!out || out_size <= 0) {
        fprintf(stderr, "xmlDocDumpMemory returned no output\n");
        xmlFreeDoc(doc);
        return 8;
    }
    xmlFree(out);

    xmlFreeDoc(doc);
    return 0;
}

static int g_callback_count = 0;
/* libxml2's xmlStructuredErrorFunc takes non-const xmlError*. */
static void capture(void *user_data, xmlError *err) {
    (void) user_data;
    (void) err;
    ++g_callback_count;
}

static int check_error_path(void) {
    xmlResetLastError();
    xmlSetStructuredErrorFunc(NULL, capture);

    const char *bad = "<oops";
    xmlDocPtr doc = xmlReadMemory(bad, (int) strlen(bad), NULL, NULL, 0);
    if (doc != NULL) {
        fprintf(stderr, "malformed input should have failed\n");
        xmlFreeDoc(doc);
        return 10;
    }
    if (g_callback_count == 0) {
        fprintf(stderr, "structured error callback never fired\n");
        return 11;
    }
    const xmlError *e = xmlGetLastError();
    if (!e || e->code == 0) {
        fprintf(stderr, "xmlGetLastError() returned no/zero code\n");
        return 12;
    }
    xmlSetStructuredErrorFunc(NULL, NULL);
    xmlResetLastError();
    return 0;
}

int main(void) {
    xmlInitParser();

    int rc;
    if ((rc = check_parse_walk())   != 0) return rc;
    if ((rc = check_error_path())   != 0) return rc;

    xmlCleanupParser();
    printf("t-libxml2-headers OK\n");
    return 0;
}
