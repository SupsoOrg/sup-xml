#!/usr/bin/env bash
# Run the nokogiri smoke test twice:
#   1. Control — nokogiri against the system libxml2 (sanity).
#   2. Shim   — nokogiri against our libsup_xml_compat (the real test).
#
# Big difference from the lxml runner: nokogiri's precompiled arm64
# gem statically bundles libxml2 inside `nokogiri.bundle`, so there's
# no `libxml2.2.dylib` to redirect.  We install nokogiri from source
# (`--use-system-libraries`) into a project-local gem path so it
# dynamically links to libxml2, then `install_name_tool`-redirect
# the resulting .bundle the same way lxml/run.sh redirects etree.so.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"

GEM_HOME_DIR="$REPO/target/nokogiri-gem-home"
SWAP_REAL="$REPO/target/nokogiri-swap"
SWAP="/tmp/sxn"
COMPAT_LIB="$REPO/target/debug/libsup_xml_compat.dylib"
SMOKE="$REPO/tests/abi-system/nokogiri/smoke.rb"

# Where Homebrew (or system) libxml2 lives for --use-system-libraries.
LIBXML2_PREFIX="$(brew --prefix libxml2 2>/dev/null || echo /usr)"
LIBXML2_DYLIB="$LIBXML2_PREFIX/lib/libxml2.2.dylib"
if [ ! -f "$LIBXML2_DYLIB" ]; then
    LIBXML2_DYLIB="/usr/lib/libxml2.2.dylib"
fi

# ── 1. Build the shim. ─────────────────────────────────────────────────────
echo ">>> Building libsup_xml_compat (cdylib-exports) …"
cargo build -p sup-xml-compat --features cdylib-exports 2>&1 | tail -2

if [ ! -f "$COMPAT_LIB" ]; then
    echo "ERROR: cdylib not at $COMPAT_LIB" >&2
    exit 1
fi

# ── 2. Install nokogiri from source with system libraries. ─────────────────
# Permissive CFLAGS suppress the clang-17 strict-mode escalations that
# would otherwise make mkmf's `try_cppflags` self-tests fail and skip the
# libgumbo include path.  Without these, gumbo.c can't find its own
# header and the build dies before linking libxml2.
if [ ! -d "$GEM_HOME_DIR" ] || ! GEM_HOME="$GEM_HOME_DIR" GEM_PATH="$GEM_HOME_DIR" gem list nokogiri -i >/dev/null 2>&1; then
    echo ">>> Installing nokogiri from source against $LIBXML2_DYLIB …"
    mkdir -p "$GEM_HOME_DIR"
    CFLAGS="-Wno-error -Wno-unused-command-line-argument -Wno-default-const-init-field-unsafe -Wno-incompatible-function-pointer-types -Wno-implicit-function-declaration" \
    GEM_HOME="$GEM_HOME_DIR" GEM_PATH="$GEM_HOME_DIR" \
        gem install nokogiri --no-document --platform=ruby -- \
            --use-system-libraries \
            --with-xml2-dir="$LIBXML2_PREFIX" \
            --with-xslt-dir="$(brew --prefix libxslt 2>/dev/null || echo /usr)" \
        | tail -5
fi

# Locate the freshly-built nokogiri.bundle.  Nokogiri ships per-ruby-minor
# subdirs (`3.2/`, `3.3/`, `3.4/`); pick the one that matches the live ruby.
RUBY_MINOR="$(ruby -e 'print RUBY_VERSION.split(".")[0..1].join(".")')"
NOKO_GEM_DIR="$(GEM_HOME="$GEM_HOME_DIR" GEM_PATH="$GEM_HOME_DIR" gem which nokogiri | sed 's|/lib/nokogiri.rb||')"
NOKO_BUNDLE="$NOKO_GEM_DIR/lib/nokogiri/$RUBY_MINOR/nokogiri.bundle"
if [ ! -f "$NOKO_BUNDLE" ]; then
    # Fall back to the only .bundle that exists.
    NOKO_BUNDLE="$(find "$NOKO_GEM_DIR/lib" -name 'nokogiri.bundle' | head -1)"
fi
if [ ! -f "$NOKO_BUNDLE" ]; then
    echo "ERROR: could not locate nokogiri.bundle in $NOKO_GEM_DIR" >&2
    exit 1
fi
echo ">>> nokogiri.bundle: $NOKO_BUNDLE"
echo "    linkage:"
otool -L "$NOKO_BUNDLE" | grep -E "libxml2|libxslt|libexslt" | sed 's/^/      /'

# Detect the libxml2 install_name that nokogiri.bundle was linked against.
# Homebrew's libxml2 used to be `libxml2.2.dylib` (v2 ABI); from libxml2 3.x
# the major-version bump made it `libxml2.16.dylib`.  We don't pick one
# statically — we read it off the bundle.
LIBXML2_LINKED_NAME="$(otool -L "$NOKO_BUNDLE" | awk '/libxml2\.[0-9]+\.dylib/ {n=$1; sub(".*/","",n); print n; exit}')"
LIBXSLT_LINKED_NAME="$(otool -L "$NOKO_BUNDLE" | awk '/libxslt\.[0-9]+\.dylib/ {n=$1; sub(".*/","",n); print n; exit}')"
LIBEXSLT_LINKED_NAME="$(otool -L "$NOKO_BUNDLE" | awk '/libexslt\.[0-9]+\.dylib/ {n=$1; sub(".*/","",n); print n; exit}')"
echo ">>> nokogiri.bundle links: $LIBXML2_LINKED_NAME, $LIBXSLT_LINKED_NAME, $LIBEXSLT_LINKED_NAME"

# ── 3. Stage swap dir. ─────────────────────────────────────────────────────
mkdir -p "$SWAP_REAL"
ln -sf "$COMPAT_LIB" "$SWAP_REAL/$LIBXML2_LINKED_NAME"
# Also expose libxslt & libexslt — copy from Homebrew, then rewrite their
# embedded libxml2 reference to point at OUR shim, so libxslt → libxml2
# transitively goes through us.
for libname in "$LIBXSLT_LINKED_NAME" "$LIBEXSLT_LINKED_NAME"; do
    [ -z "$libname" ] && continue
    src="$LIBXML2_PREFIX/../libxslt/lib/$libname"
    if [ -f "$src" ]; then
        cp "$src" "$SWAP_REAL/$libname"
        EMBEDDED_XML2="$(otool -L "$SWAP_REAL/$libname" | awk -v ln="$LIBXML2_LINKED_NAME" '$0 ~ ln {print $1; exit}')"
        if [ -n "$EMBEDDED_XML2" ]; then
            install_name_tool -change "$EMBEDDED_XML2" "$SWAP/$LIBXML2_LINKED_NAME" "$SWAP_REAL/$libname"
            codesign --force --sign - "$SWAP_REAL/$libname" 2>/dev/null || true
        fi
    fi
done

# Bind /tmp/sxn -> target/nokogiri-swap via symlink so install_name_tool
# can point at a short path (Mach-O load-command pads are stingy).
rm -f "$SWAP"
ln -s "$SWAP_REAL" "$SWAP"

# ── 4. Redirect the .bundle. ───────────────────────────────────────────────
REDIR_DIR="$REPO/target/nokogiri-redir"
rm -rf "$REDIR_DIR"
mkdir -p "$REDIR_DIR"
cp "$NOKO_BUNDLE" "$REDIR_DIR/nokogiri.bundle"

# Rewrite every libxml2 / libxslt / libexslt load command.
for libname in "$LIBXML2_LINKED_NAME" "$LIBXSLT_LINKED_NAME" "$LIBEXSLT_LINKED_NAME"; do
    [ -z "$libname" ] && continue
    EMBEDDED="$(otool -L "$REDIR_DIR/nokogiri.bundle" | awk -v ln="$libname" '$0 ~ ln {print $1; exit}')"
    if [ -n "$EMBEDDED" ]; then
        install_name_tool -change "$EMBEDDED" "$SWAP/$libname" "$REDIR_DIR/nokogiri.bundle"
    fi
done
codesign --force --sign - "$REDIR_DIR/nokogiri.bundle" 2>/dev/null || true

echo ">>> redirected nokogiri.bundle linkage:"
otool -L "$REDIR_DIR/nokogiri.bundle" | grep -E "libxml2|libxslt|libexslt" | sed 's/^/      /'

# ── 5. Run the smoke test twice. ───────────────────────────────────────────
echo
echo "============================================================"
echo "  CONTROL RUN  (nokogiri against system/Homebrew libxml2)"
echo "============================================================"
GEM_HOME="$GEM_HOME_DIR" GEM_PATH="$GEM_HOME_DIR" ruby "$SMOKE" || true

echo
echo "============================================================"
echo "  SHIM RUN  (nokogiri against sup-xml-compat)"
echo "============================================================"
# Point the loader at our redirected .bundle by symlinking it over the
# real one's path in a copied gem dir.  Simpler: copy the whole gem
# into a redir dir, swap the .bundle, run ruby with that GEM_HOME.
REDIR_GEM="$REPO/target/nokogiri-redir-gem"
rm -rf "$REDIR_GEM"
mkdir -p "$(dirname "$REDIR_GEM")"
cp -R "$NOKO_GEM_DIR/../.." "$REDIR_GEM"
# Find and overwrite the bundle inside the copied tree.
REDIR_BUNDLE_PATH="$(find "$REDIR_GEM" -name 'nokogiri.bundle' | head -1)"
cp "$REDIR_DIR/nokogiri.bundle" "$REDIR_BUNDLE_PATH"
GEM_HOME="$REDIR_GEM" GEM_PATH="$REDIR_GEM" ruby "$SMOKE" || rc=$?
echo
echo "(shim run returned ${rc:-0} — informative, not a fatal error)"
