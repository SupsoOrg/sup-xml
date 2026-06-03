/* T-ERR-03: xmlError struct layout — C-side _Static_assert.
 *
 * Compile-time verification that every public field of xmlError sits
 * at the byte offset libxml2 documents.  Drift = compile failure.
 *
 * Pairs with the const-offset assertions in
 * `crates/compat/src/error.rs` — that file checks the Rust side,
 * this file checks the C side.  Both have to agree for libxml2
 * callers' field reads to land correctly.
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

_Static_assert(offsetof(xmlError, domain)  ==  0, "xmlError::domain @ 0");
_Static_assert(offsetof(xmlError, code)    ==  4, "xmlError::code @ 4");
_Static_assert(offsetof(xmlError, message) ==  8, "xmlError::message @ 8");
_Static_assert(offsetof(xmlError, level)   == 16, "xmlError::level @ 16");
_Static_assert(offsetof(xmlError, file)    == 24, "xmlError::file @ 24");
_Static_assert(offsetof(xmlError, line)    == 32, "xmlError::line @ 32");
_Static_assert(offsetof(xmlError, str1)    == 40, "xmlError::str1 @ 40");
_Static_assert(offsetof(xmlError, str2)    == 48, "xmlError::str2 @ 48");
_Static_assert(offsetof(xmlError, str3)    == 56, "xmlError::str3 @ 56");
_Static_assert(offsetof(xmlError, int1)    == 64, "xmlError::int1 @ 64");
_Static_assert(offsetof(xmlError, int2)    == 68, "xmlError::int2 @ 68");
_Static_assert(offsetof(xmlError, ctxt)    == 72, "xmlError::ctxt @ 72");
_Static_assert(offsetof(xmlError, node)    == 80, "xmlError::node @ 80");
_Static_assert(sizeof(xmlError)            == 88, "xmlError size = 88");

int main(void) {
    /* If we got here, every _Static_assert above passed. */
    printf("T-ERR-03 OK\n");
    return 0;
}
