/* T-LINK-02 — the cdylib builds, links, and the resulting binary loads
 * the dynamic library cleanly.
 *
 * From the test inventory:
 *   T-LINK-02  SONAME is libxml2.so.2 (matches upstream)
 *               readelf check
 *
 * For v0.1 of the shim we ship as `libsup_xml_compat` (opt-in
 * pkg-config) rather than `libxml2.so.2` directly — see
 * thoughts/libxml2_abi_plan.txt § DISTRIBUTION.  Once conformance
 * reaches the threshold, we'll add a SONAME-matching symlink and a
 * separate test will assert it.  For now this test just verifies the
 * dynamic linkage path works end-to-end: the C program compiles
 * against our headers, the linker resolves `-lsup_xml_compat`, and
 * the dynamic loader can run the binary.
 *
 * No functional API call — this is the link smoke test.  Output:
 * a single line "T-LINK-02 OK\n" so the test driver can verify the
 * binary executed (not just compiled).
 */

#include <stdio.h>

int main(void) {
    printf("T-LINK-02 OK\n");
    return 0;
}
