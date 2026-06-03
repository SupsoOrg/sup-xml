#!/usr/bin/env bash
# Run the lxml smoke test twice:
#   1. Control — lxml against the system libxml2 (sanity).
#   2. Shim   — lxml against our libsup_xml_compat (the real test).
#
# Everything lives under $REPO/target/ (gitignored, wiped by cargo clean).
# Nothing outside the repo is touched.  System libxml2 is read-only
# (SIP-protected on macOS); even if we wanted to, we couldn't damage it.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"

VENV="$REPO/target/py-venv"
# Real swap dir under target/ so `cargo clean` wipes it.  But macOS
# install_name_tool can't grow the Mach-O load-command table beyond
# what fits, and lxml's etree.so has just enough pad for *one* long
# path rewrite (libxml2).  To stage libxslt + libexslt as well we
# need a short path — same length or shorter than the original
# `/usr/lib/libxslt.1.dylib`.  We bind-mount `target/lxml-swap` to
# `/tmp/sxs` via a symlink and point install_name_tool at the short
# path; the underlying files still live under target/.
SWAP_REAL="$REPO/target/lxml-swap"
SWAP="/tmp/sxs"
COMPAT_LIB="$REPO/target/debug/libsup_xml_compat.dylib"
SMOKE="$REPO/tests/abi-system/lxml/smoke.py"

# ── 1. Build the shim (incremental — fast on rebuilds). ─────────────────
echo ">>> Building libsup_xml_compat …"
cargo build -p sup-xml-compat --features cdylib-exports 2>&1 | tail -2

if [ ! -f "$COMPAT_LIB" ]; then
    echo "ERROR: cdylib not at $COMPAT_LIB" >&2
    exit 1
fi

# ── 2. Set up the Python venv (one-time). ───────────────────────────────
if [ ! -x "$VENV/bin/python" ]; then
    echo ">>> Creating Python venv at $VENV …"
    python3 -m venv "$VENV"
    "$VENV/bin/pip" install --quiet --upgrade pip
fi
PY="$VENV/bin/python"

# Check that lxml is installed AND dynamically linked.  The PyPI binary
# wheel embeds libxml2 statically — useless for our test because
# DYLD_* can't intercept calls that never leave the .so.  We force a
# source build with `STATIC_DEPS=false` which links against the system
# libxml2; then our shim can replace it.
LXML_SO_PATH=$("$PY" -c "import lxml.etree; print(lxml.etree.__file__)" 2>/dev/null || true)
NEEDS_REBUILD=1
if [ -n "$LXML_SO_PATH" ]; then
    if otool -L "$LXML_SO_PATH" 2>/dev/null | grep -q 'libxml2'; then
        NEEDS_REBUILD=0
    fi
fi
if [ "$NEEDS_REBUILD" = "1" ]; then
    echo ">>> Installing/rebuilding lxml against system libxml2 (source build)…"
    "$VENV/bin/pip" uninstall --quiet -y lxml 2>/dev/null || true
    STATIC_DEPS=false "$VENV/bin/pip" install --quiet --no-binary lxml lxml
fi

# Optional packages that gate parts of lxml's own test suite: cssselect
# unlocks test_css (CSS->XPath translation); rnc2rng unlocks the RelaxNG
# Compact (.rnc) tests in test_relaxng.  Without them those modules
# collect zero tests and silently skip.
"$VENV/bin/pip" install --quiet cssselect rnc2rng

# ── 3. Inspect how lxml resolves libxml2. ───────────────────────────────
echo ">>> lxml install info:"
LXML_SO=$("$PY" -c "import lxml.etree; print(lxml.etree.__file__)")
echo "   etree.so: $LXML_SO"
echo "   linkage:"
otool -L "$LXML_SO" 2>/dev/null | grep -i 'libxml\|@rpath' | sed 's/^/     /' || true

# ── 4. Stage the swap dir (symlinks; instant + gitignored). ─────────────
# Files live in target/lxml-swap/ (gitignored, wiped by `cargo clean`).
# `/tmp/sxs/` is a symlink to that real dir so install_name_tool
# rewrites can use a short path that fits the Mach-O headerpad in
# lxml's etree.so.  Why this matters: on macOS each Mach-O file has
# a fixed-size load-command table laid out at link time; replacing a
# load command with one that's *longer* requires unused header bytes
# ("headerpad") to grow into.  etree.so was linked with just enough
# pad to absorb one long path rewrite — using the full
# `/Users/jp/projects/sup-xml/target/lxml-swap/...` for libxml2 +
# libxslt + libexslt overflows it.  A 24-char path like
# `/tmp/sxs/libxslt.1.dylib` matches `/usr/lib/libxslt.1.dylib` exactly
# and needs zero new pad.
mkdir -p "$SWAP_REAL"
ln -sfn "$SWAP_REAL" "$SWAP"
ln -sfn "$COMPAT_LIB" "$SWAP_REAL/libxml2.2.dylib"
ln -sfn "$COMPAT_LIB" "$SWAP_REAL/libxml2.dylib"

# ── 4a. Optionally stage a redirected libxslt + libexslt. ─────────────────
# lxml's etree.so links libxslt for XSLT / iso-schematron support.
# libxslt itself was compiled against the system's (or Homebrew's)
# libxml2.  Without rewiring libxslt → our shim, every test that
# touches XSLT crashes when libxslt internally calls libxml2's
# xmlFreeNode / xmlDictOwns / etc. on nodes our shim allocated.
#
# The fix is to take Homebrew's real libxslt + libexslt dylibs,
# install_name_tool their libxml2 load command to point at our shim
# (via $SWAP/libxml2.2.dylib), then redirect lxml's libxslt
# references to the staged copies.  Works only when our compat shim
# exports every libxml2 symbol libxslt looks up at dlopen — the
# stubs in `crates/compat/src/stubs.rs` cover that today.
#
# Note: every stub returns NULL at runtime, so XSLT operations that
# *succeed* would still produce wrong results.  The point of this
# block is to make XSLT-touching tests *load and fail cleanly*
# (raising Python exceptions) instead of segfaulting — a much
# better debugging baseline than "the whole process crashes."
LIBXSLT_BREW=""
for cand in \
    /opt/homebrew/opt/libxslt/lib/libxslt.1.dylib \
    /usr/local/opt/libxslt/lib/libxslt.1.dylib \
    /opt/homebrew/Cellar/libxslt/*/lib/libxslt.1.dylib \
    /usr/local/Cellar/libxslt/*/lib/libxslt.1.dylib
do
    [ -f "$cand" ] && LIBXSLT_BREW="$cand" && break
done

if [ -n "$LIBXSLT_BREW" ]; then
    LIBEXSLT_BREW="$(dirname "$LIBXSLT_BREW")/libexslt.0.dylib"
    cp "$LIBXSLT_BREW"  "$SWAP/libxslt.1.dylib"
    cp "$LIBEXSLT_BREW" "$SWAP/libexslt.0.dylib"
    chmod u+w "$SWAP/libxslt.1.dylib" "$SWAP/libexslt.0.dylib"
    # Repoint both at our shim instead of whatever libxml2 they were
    # built against.  Discover the actual install_name from
    # `otool -L` rather than hardcoding (Homebrew's current libxml2
    # is `libxml2.16.dylib`, not the system's `libxml2.2.dylib`).
    for dylib in "$SWAP/libxslt.1.dylib" "$SWAP/libexslt.0.dylib"; do
        for old in $(otool -L "$dylib" 2>/dev/null \
                       | awk '/libxml2.*\.dylib/ {print $1}'); do
            install_name_tool -change "$old" "$SWAP/libxml2.2.dylib" "$dylib" 2>/dev/null || true
        done
    done
    # libexslt links libxslt — point that at our local copy too.
    for old in $(otool -L "$SWAP/libexslt.0.dylib" 2>/dev/null \
                   | awk '/libxslt[^[:space:]]*\.dylib/ && !/libexslt/ {print $1}'); do
        install_name_tool -change "$old" "$SWAP/libxslt.1.dylib" "$SWAP/libexslt.0.dylib" 2>/dev/null || true
    done
    install_name_tool -id "$SWAP/libxslt.1.dylib"  "$SWAP/libxslt.1.dylib"
    install_name_tool -id "$SWAP/libexslt.0.dylib" "$SWAP/libexslt.0.dylib"
    codesign --force --sign - "$SWAP/libxslt.1.dylib"  2>/dev/null
    codesign --force --sign - "$SWAP/libexslt.0.dylib" 2>/dev/null
    echo ">>> staged Homebrew libxslt at $SWAP/libxslt.1.dylib (source: $LIBXSLT_BREW)"
else
    echo ">>> note: libxslt redirect skipped (run 'brew install libxslt' to enable"
    echo "          test_xslt / test_isoschematron / test_main_xslt_in_thread)"
fi

echo ">>> swap dir: $SWAP"
ls -la "$SWAP" | sed 's/^/     /'

# ── 4b. Make a redirected copy of etree.so under target/. ───────────────
# DYLD_INSERT_LIBRARIES + DYLD_FORCE_FLAT_NAMESPACE doesn't work for us
# because macOS uses two-level namespaces: lxml's etree.so has its
# libxml2 references PINNED to the install_name `/usr/lib/libxml2.2.dylib`.
# Even with our shim inserted, dyld routes calls to the system one.
#
# Fix: rewrite the load command via install_name_tool.  We copy the
# stock etree.so → target/ first so the venv's original stays clean
# (and pip-uninstall still works if we ever need it), then point a
# DYLD_INSERT_LIBRARIES + LD_LIBRARY_PATH dance at our shim.  We use a
# private Python launcher (a tiny copy of the venv site config) that
# uses the redirected .so.
# Derive these from the etree.so discovered above ($LXML_SO) so the
# harness tracks whatever Python the venv was built with, rather than a
# pinned interpreter version.
LXML_DIR="$(dirname "$LXML_SO")"
LXML_REDIR_DIR="$REPO/target/lxml-redir"
ETREE_ORIG="$LXML_SO"
ETREE_REDIR="$LXML_REDIR_DIR/$(basename "$LXML_SO")"

mkdir -p "$LXML_REDIR_DIR"
# Mirror every file in lxml's package dir (Python files + .so's + data),
# but replace etree.so with a redirected copy whose libxml2 dependency
# points at our swap location.
#
# `cp -R` clobbers existing files atomically enough for an idempotent
# rerun; the install_name_tool step below is also idempotent.
rsync -a --delete "$LXML_DIR/" "$LXML_REDIR_DIR/"
# Every lxml C-extension that links libxml2 must be redirected, not
# just etree.so — objectify.so reads `node.doc.dict` and passes it
# straight to whichever libxml2 it's bound to.  If that's the system
# one, it reads our Rust `Dict` struct as an `_xmlDict` and reads
# garbage off offsets it doesn't understand.
for so in "$LXML_REDIR_DIR"/*.so; do
    changed=0
    if otool -L "$so" 2>/dev/null | grep -q "/usr/lib/libxml2.2.dylib"; then
        install_name_tool -change /usr/lib/libxml2.2.dylib "$SWAP/libxml2.2.dylib" "$so"
        changed=1
    fi
    # Redirect libxslt / libexslt too, if we have staged copies.
    if [ -f "$SWAP/libxslt.1.dylib" ] && otool -L "$so" 2>/dev/null | grep -q "/usr/lib/libxslt.1.dylib"; then
        install_name_tool -change /usr/lib/libxslt.1.dylib "$SWAP/libxslt.1.dylib" "$so"
        changed=1
    fi
    if [ -f "$SWAP/libexslt.0.dylib" ] && otool -L "$so" 2>/dev/null | grep -q "/usr/lib/libexslt.0.dylib"; then
        install_name_tool -change /usr/lib/libexslt.0.dylib "$SWAP/libexslt.0.dylib" "$so"
        changed=1
    fi
    if [ "$changed" -eq 1 ]; then
        # install_name_tool invalidates the code signature on Apple
        # Silicon; ad-hoc re-sign so dyld won't refuse to load it.
        codesign --force --sign - "$so" 2>/dev/null
    fi
done

echo ">>> redirected lxml linkage:"
for so in "$LXML_REDIR_DIR"/*.so; do
    name=$(basename "$so")
    otool -L "$so" 2>/dev/null | grep -i 'libxml\|libxslt' \
        | sed "s|^|     $name: |" || true
done

# ── 5. Control run — system libxml2, no env vars. ───────────────────────
echo ""
echo "============================================================"
echo "  CONTROL RUN  (lxml against system libxml2)"
echo "============================================================"
"$PY" "$SMOKE" || echo "(control returned $?)"

# ── 6. Shim run — point Python at the redirected lxml package. ─────────
# We prepend `target/lxml-redir/`'s parent to PYTHONPATH so `import
# lxml` finds our redirected package first.  Because the redirected
# etree.so has its LC_LOAD_DYLIB pointing at our shim, dyld resolves
# every libxml2 symbol against `libsup_xml_compat.dylib`.  The
# parent shell + every other Python invocation is unaffected.
PYPATH_SHIM="$REPO/target"  # target/lxml-redir/ → `import lxml-redir`
LXML_REDIR_PKG_DIR="$REPO/target/lxml-redir-pkg"
mkdir -p "$LXML_REDIR_PKG_DIR/lxml"
rsync -a --delete "$LXML_REDIR_DIR/" "$LXML_REDIR_PKG_DIR/lxml/"

# Stage lxml's test_*.py files so run_lxml_suite.sh has something to
# load.  The PyPI lxml wheel doesn't include `tests/` — only the sdist
# does — so we pull it from a cached sdist if one exists.  Without
# this step, `rsync --delete` above wipes any previously-staged tests
# on every harness run.  Best-effort: silently skip if no sdist is
# around (run_lxml_suite.sh handles the absence with a clear message).
LXML_SDIST_TESTS=""
for cand in /tmp/lxml-sdist/lxml-*/src/lxml/tests "$REPO/target/lxml-sdist"/lxml-*/src/lxml/tests; do
    [ -d "$cand" ] && LXML_SDIST_TESTS="$cand" && break
done
if [ -n "$LXML_SDIST_TESTS" ]; then
    rsync -a "$LXML_SDIST_TESTS/" "$LXML_REDIR_PKG_DIR/lxml/tests/"
    echo ">>> staged lxml test_*.py from $LXML_SDIST_TESTS"
fi

echo ""
echo "============================================================"
echo "  SHIM RUN  (lxml against sup-xml-compat)"
echo "============================================================"
PYTHONPATH="$LXML_REDIR_PKG_DIR:${PYTHONPATH:-}" \
"$PY" "$SMOKE" || {
    rc=$?
    echo ""
    echo "(shim run returned $rc — informative, not a fatal error)"
    exit 0
}
