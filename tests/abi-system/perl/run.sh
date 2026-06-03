#!/usr/bin/env bash
# Run the Perl XML::LibXML smoke test twice:
#   1. Control — XML::LibXML against the system libxml2 (sanity).
#   2. Shim   — XML::LibXML against our libsup_xml_compat (the real test).
#
# Easier than the nokogiri or lxml runners: macOS's system Perl ships
# XML::LibXML already dynamically linked to /usr/lib/libxml2.2.dylib —
# no install-from-source needed.  We just stage a redirected copy of
# the bundle that loads our shim instead.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"

SWAP_REAL="$REPO/target/perl-swap"
SWAP="/tmp/sxp"
COMPAT_LIB="$REPO/target/debug/libsup_xml_compat.dylib"
SMOKE="$REPO/tests/abi-system/perl/smoke.pl"

# Locate the live XML::LibXML installation.
LIBXML_BUNDLE="$(perl -MXML::LibXML -e 'use DynaLoader; for (@DynaLoader::dl_shared_objects) { print "$_\n" if /LibXML\.bundle/ }')"
if [ -z "$LIBXML_BUNDLE" ] || [ ! -f "$LIBXML_BUNDLE" ]; then
    echo "ERROR: could not find XML::LibXML's LibXML.bundle" >&2
    exit 1
fi
LIBXML_AUTODIR="$(dirname "$LIBXML_BUNDLE")"
LIBXML_PERL_PARENT="$(perl -MXML::LibXML -e 'print $INC{"XML/LibXML.pm"}' | sed 's|/XML/LibXML.pm||')"

# ── 1. Build the shim. ─────────────────────────────────────────────────────
echo ">>> Building libsup_xml_compat (cdylib-exports) …"
cargo build -p sup-xml-compat --features cdylib-exports 2>&1 | tail -2

if [ ! -f "$COMPAT_LIB" ]; then
    echo "ERROR: cdylib not at $COMPAT_LIB" >&2
    exit 1
fi

echo ">>> XML::LibXML bundle: $LIBXML_BUNDLE"
echo "    linkage:"
otool -L "$LIBXML_BUNDLE" | grep -E "libxml2|libxslt|libexslt" | sed 's/^/      /'

# ── 2. Stage the swap dir + redirect a copy of LibXML.bundle. ──────────────
mkdir -p "$SWAP_REAL"
ln -sf "$COMPAT_LIB" "$SWAP_REAL/libxml2.2.dylib"

# Short symlink so install_name_tool fits in the load-command pad.
rm -f "$SWAP"
ln -s "$SWAP_REAL" "$SWAP"

# Stage a redirected @INC tree.  We need:
#   - All the XML/*.pm files so `use XML::LibXML` keeps finding them
#   - A modified copy of LibXML.bundle that loads our shim
REDIR_INC="$REPO/target/perl-redir-inc"
rm -rf "$REDIR_INC"
mkdir -p "$REDIR_INC/auto/XML/LibXML"
# Copy the Perl-side modules (read-only on SIP filesystems → use cp).
cp -R "$LIBXML_PERL_PARENT/XML" "$REDIR_INC/"
# Copy the bundle into the redirected auto-dir layout.
cp "$LIBXML_BUNDLE" "$REDIR_INC/auto/XML/LibXML/LibXML.bundle"

# Rewrite the libxml2 load command in the copied bundle.
EMBEDDED_XML2="$(otool -L "$REDIR_INC/auto/XML/LibXML/LibXML.bundle" | awk '/libxml2\.2\.dylib/ {print $1; exit}')"
if [ -z "$EMBEDDED_XML2" ]; then
    echo "ERROR: LibXML.bundle has no libxml2.2.dylib load command — is XML::LibXML statically linked?" >&2
    otool -L "$REDIR_INC/auto/XML/LibXML/LibXML.bundle"
    exit 1
fi
install_name_tool -change "$EMBEDDED_XML2" "$SWAP/libxml2.2.dylib" \
    "$REDIR_INC/auto/XML/LibXML/LibXML.bundle"
codesign --force --sign - "$REDIR_INC/auto/XML/LibXML/LibXML.bundle" 2>/dev/null || true

echo ">>> redirected LibXML.bundle linkage:"
otool -L "$REDIR_INC/auto/XML/LibXML/LibXML.bundle" | grep -E "libxml2|libxslt|libexslt" | sed 's/^/      /'

# ── 3. Run the smoke test twice. ───────────────────────────────────────────
echo
echo "============================================================"
echo "  CONTROL RUN  (XML::LibXML against system libxml2)"
echo "============================================================"
perl "$SMOKE" || true

echo
echo "============================================================"
echo "  SHIM RUN  (XML::LibXML against sup-xml-compat)"
echo "============================================================"
PERL5LIB="$REDIR_INC" perl "$SMOKE" || rc=$?
echo
echo "(shim run returned ${rc:-0} — informative, not a fatal error)"
