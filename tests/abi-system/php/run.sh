#!/usr/bin/env bash
# Run the PHP libxml2 smoke test twice:
#   1. Control — PHP against the system/Homebrew libxml2 (sanity).
#   2. Shim   — PHP against our libsup_xml_compat (the real test).
#
# PHP's XML extensions (DOM, SimpleXML, XMLReader, XMLWriter, XSL,
# libxml core) are PHP's entire XML stack and all built on libxml2.
# On macOS with Homebrew PHP, those extensions dynamically link to
# Homebrew's libxml2 — we hijack the load command the same way as
# the nokogiri / lxml / Perl runners.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"

SWAP_REAL="$REPO/target/php-swap"
SWAP="/tmp/sxph"
COMPAT_LIB="$REPO/target/debug/libsup_xml_compat.dylib"
SMOKE="$REPO/tests/abi-system/php/smoke.php"

if ! command -v php >/dev/null 2>&1; then
    echo "ERROR: php not found.  Install with:" >&2
    echo "  brew install php" >&2
    exit 1
fi

# ── 1. Build the shim. ─────────────────────────────────────────────────────
echo ">>> Building libsup_xml_compat (cdylib-exports) …"
cargo build -p sup-xml-compat --features cdylib-exports 2>&1 | tail -2

if [ ! -f "$COMPAT_LIB" ]; then
    echo "ERROR: cdylib not at $COMPAT_LIB" >&2
    exit 1
fi

# ── 2. Locate PHP's XML-containing binary and its libxml2 linkage. ────────
# PHP on macOS via Homebrew links libxml2 into the main `php` binary
# (built-in extensions, not loadable .so).  Some installs use a
# shared-extensions layout where dom.so / simplexml.so live separately
# and each link libxml2 individually.  Detect both shapes.
PHP_BIN="$(command -v php)"
echo ">>> php binary: $PHP_BIN"
echo "    linkage:"
otool -L "$PHP_BIN" | grep -E "libxml2|libxslt|libexslt" | sed 's/^/      /' || true

# Probe for separately-built extension .so files (informational only).
# Most Homebrew PHP installs build the XML extensions INTO the main
# `php` binary, so the directory may be empty/missing — that's fine.
PHP_EXT_DIR="$(php -i 2>/dev/null | awk -F' => ' '/^extension_dir/ {print $2; exit}' || true)"
PHP_EXT_DIR="${PHP_EXT_DIR%[[:space:]]}"
if [ -n "$PHP_EXT_DIR" ] && [ -d "$PHP_EXT_DIR" ]; then
    echo ">>> extensions dir: $PHP_EXT_DIR"
    for ext in dom.so simplexml.so xmlreader.so xmlwriter.so xsl.so libxml.so; do
        [ -f "$PHP_EXT_DIR/$ext" ] || continue
        echo "    $ext linkage:"
        otool -L "$PHP_EXT_DIR/$ext" | grep -E "libxml2|libxslt|libexslt" | sed 's/^/      /' || true
    done
fi

# Detect the libxml2 dylib name the PHP binary references.
LIBXML2_LINKED_NAME="$(otool -L "$PHP_BIN" | awk '/libxml2\.[0-9]+\.dylib/ {n=$1; sub(".*/","",n); print n; exit}')"
LIBXML2_FULL_PATH="$(otool -L "$PHP_BIN" | awk '/libxml2\.[0-9]+\.dylib/ {print $1; exit}')"

if [ -z "$LIBXML2_LINKED_NAME" ]; then
    echo "ERROR: PHP binary doesn't dynamically link libxml2.  Either the build is" >&2
    echo "static (rebuild with --with-libxml=shared) or the binary uses a different" >&2
    echo "XML library entirely.  Cannot redirect." >&2
    exit 1
fi
echo ">>> PHP links libxml2 as: $LIBXML2_LINKED_NAME (full: $LIBXML2_FULL_PATH)"

# ── 3. Stage swap dir. ─────────────────────────────────────────────────────
mkdir -p "$SWAP_REAL"
ln -sf "$COMPAT_LIB" "$SWAP_REAL/$LIBXML2_LINKED_NAME"

# Bind /tmp/sxph -> target/php-swap for short-path install_name.
rm -f "$SWAP"
ln -s "$SWAP_REAL" "$SWAP"

# ── 4. Redirect a copy of the PHP binary. ─────────────────────────────────
REDIR_BIN="$REPO/target/php-redir/php"
rm -rf "$(dirname "$REDIR_BIN")"
mkdir -p "$(dirname "$REDIR_BIN")"
cp "$PHP_BIN" "$REDIR_BIN"
chmod +x "$REDIR_BIN"

# Rewrite the libxml2 load command in the copied binary to point at our shim.
install_name_tool -change "$LIBXML2_FULL_PATH" "$SWAP/$LIBXML2_LINKED_NAME" "$REDIR_BIN"
codesign --force --sign - "$REDIR_BIN" 2>/dev/null || true

echo ">>> redirected php linkage:"
otool -L "$REDIR_BIN" | grep -E "libxml2|libxslt|libexslt" | sed 's/^/      /' || true

# ── 5. Run the smoke test twice. ───────────────────────────────────────────
echo
echo "============================================================"
echo "  CONTROL RUN  (PHP against Homebrew libxml2)"
echo "============================================================"
"$PHP_BIN" "$SMOKE" || true

echo
echo "============================================================"
echo "  SHIM RUN  (PHP against sup-xml-compat)"
echo "============================================================"
"$REDIR_BIN" "$SMOKE" || rc=$?
echo
echo "(shim run returned ${rc:-0} — informative, not a fatal error)"
