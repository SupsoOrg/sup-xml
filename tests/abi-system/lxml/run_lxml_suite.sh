#!/usr/bin/env bash
# Drive lxml's own test suite against sup-xml-compat.
#
# Each test_*.py file runs in its own subprocess so a crash in one
# file doesn't take down the harness.  Within a file, tests run
# sequentially; if a test segfaults, the surviving tests in the
# same file are lost — that's the trade-off vs. per-test
# subprocess spawn cost (would be 3000+ processes).
#
# Outputs a per-file PASS/FAIL/CRASH summary, then a grand-total
# tally.  Re-uses the lxml-redir-pkg dir that run.sh sets up.

set -uo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
PY="$REPO/target/py-venv/bin/python"
PKG_DIR="$REPO/target/lxml-redir-pkg"
TESTS_DIR="$PKG_DIR/lxml/tests"
TIMEOUT_S=120

if [ ! -d "$TESTS_DIR" ]; then
    echo "lxml tests not staged — run tests/abi-system/lxml/run.sh first to" >&2
    echo "build the shim package, then extract lxml's source tests into:" >&2
    echo "  $TESTS_DIR" >&2
    exit 1
fi

# Each module's test_suite() adds doctests over lxml's documentation
# (.txt files), resolved by common_imports' DOC_DIR to $REPO/target/doc.
# Stage them from the sdist so test_suite() construction doesn't raise
# FileNotFoundError; best-effort (the sdist is fetched by the operator
# the same way run.sh's test sources are).
DOC_DST="$REPO/target/doc"
if [ ! -d "$DOC_DST" ] || [ -z "$(ls -A "$DOC_DST"/*.txt 2>/dev/null)" ]; then
    for cand in "$REPO/target/lxml-sdist"/lxml-*/doc /tmp/lxml-sdist/lxml-*/doc; do
        if [ -d "$cand" ]; then
            mkdir -p "$DOC_DST"
            cp "$cand"/*.txt "$DOC_DST"/ 2>/dev/null || true
            echo ">>> staged lxml doc/*.txt from $cand"
            break
        fi
    done
fi

total_pass=0
total_fail=0
total_err=0
total_crash=0

printf "%-40s  %6s %6s %6s %6s\n" "test file" "pass" "fail" "err" "crash"
printf "%-40s  %6s %6s %6s %6s\n" "----------------------------------------" "----" "----" "----" "-----"

# DYLD_LIBRARY_PATH points test code's `ctypes.CDLL(find_library('xml2'))`
# at our shim instead of system libxml2.  test_external_document loads
# libxml2 via ctypes directly to construct an external xmlDoc; without
# this, ctypes hits the system libxml2 alongside our shim and the
# resulting two-libxml2-in-one-process state segfaults.  /tmp/sxs is
# the same short path the install_name_tool rewrite of etree.so uses,
# so dyld sees both consumers loading from the *same path* and dedups
# to a single shim instance.
export DYLD_LIBRARY_PATH="/tmp/sxs${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"

# sup-xml's parse gate locates a license certificate relative to $HOME or
# the current directory by default.  Each test file runs from $PKG_DIR (so
# `import lxml` resolves the redirected package), which has no `.supso/`,
# so point SUPSO_LICENSE at the repo's certificate explicitly — otherwise
# every parse in every test fails the gate and reports as an error
# unrelated to the ABI.
export SUPSO_LICENSE="${SUPSO_LICENSE:-$REPO/.supso/license_certificates}"

# Optional positional args restrict the run to specific modules (by base
# name, with or without the `test_` prefix), e.g. `run_lxml_suite.sh
# test_etree`.  Without args, every `test_*.py` runs.  This is the
# canonical way to spot-check one module — it uses the module's own
# `test_suite()` and runs from `$PKG_DIR`, so it counts exactly what
# lxml's own runner does (no abstract-base mixins, no cwd-relative
# fixture clobbering).  Prefer it over `python -m unittest <module>`.
declare -a only=()
for arg in "$@"; do
    case "$arg" in
        test_*) only+=("$arg") ;;
        *)      only+=("test_$arg") ;;
    esac
done

module_selected() {
    [ "${#only[@]}" -eq 0 ] && return 0
    for m in "${only[@]}"; do [ "$m" = "$1" ] && return 0; done
    return 1
}

for f in "$TESTS_DIR"/test_*.py; do
    name="$(basename "$f" .py)"
    module_selected "$name" || continue
    output=$(cd "$PKG_DIR" && PYTHONPATH="." timeout "$TIMEOUT_S" "$PY" -c "
import unittest, sys
from lxml.tests import $name
# Prefer the module's own test_suite(): it selects the concrete,
# lxml-targeting cases and omits the abstract bases (etree=None) and the
# stdlib-ElementTree comparison cases that loadTestsFromModule would run.
if hasattr($name, 'test_suite'):
    suite = $name.test_suite()
else:
    suite = unittest.TestLoader().loadTestsFromModule($name)
runner = unittest.TextTestRunner(verbosity=0, stream=open('/dev/null', 'w'), buffer=True)
result = runner.run(suite)
print('SUMMARY %d %d %d' % (result.testsRun, len(result.failures), len(result.errors)))
sys.stdout.flush()
" 2>/dev/null)
    rc=$?
    summary=$(echo "$output" | grep "^SUMMARY")
    if [ "$rc" -eq 0 ] && [ -n "$summary" ]; then
        ran=$(echo "$summary" | awk '{print $2}')
        fail=$(echo "$summary" | awk '{print $3}')
        err=$(echo "$summary" | awk '{print $4}')
        pass=$((ran - fail - err))
        total_pass=$((total_pass + pass))
        total_fail=$((total_fail + fail))
        total_err=$((total_err + err))
        printf "%-40s  %6d %6d %6d %6d\n" "$name" "$pass" "$fail" "$err" 0
    else
        total_crash=$((total_crash + 1))
        printf "%-40s  %6s %6s %6s %6s  (rc=%d)\n" "$name" "?" "?" "?" "CRASH" "$rc"
    fi
done

printf "%-40s  %6s %6s %6s %6s\n" "----------------------------------------" "----" "----" "----" "-----"
printf "%-40s  %6d %6d %6d %6d\n" "TOTAL" "$total_pass" "$total_fail" "$total_err" "$total_crash"
