#!/bin/sh
# Fetch the XSD 1.1-specific testSets from the W3C xsdtests repository.
#
# `https://github.com/w3c/xsdtests` is a superset of the XSD 1.0 XSTS
# we already vendor at `tests/assets/xsts/xmlschema2006-11-06/` — it
# adds tests contributed by Saxonica (2010), IBM (2011), Oracle (2011),
# and the Working Group itself.  Those are the only directories this
# script pulls; the 1.0 sun/boeing/nist/ms tests stay in the original
# 2006-11-06 vendor.
#
# License: W3C Document License (the same one the 2006 XSTS uses).
#
# Run from anywhere; downloads into the same directory as this script.
# The resulting subtree is gitignored — fetch once, re-run to refresh.

set -e
dir="$(cd "$(dirname "$0")" && pwd)"
cd "$dir"

base="https://raw.githubusercontent.com/w3c/xsdtests/master"

# 1.1-only contributor directories: each contains both meta (.testSet
# manifests) and data (.xsd / .xml fixture files).  The `wg` set is
# the Working Group's own minimal end-to-end coverage.
META_DIRS="saxonMeta ibmMeta oracleMeta wgMeta common"
DATA_DIRS="saxonData ibmData oracleData wgData"

# Top-level manifests that point into the directories above.
EXTRA_FILES="extra-suite.xml XSD1_1TestCategories.xml XSD1_1TestCategories.xhtml 00COPYRIGHT"

echo "Cloning W3C xsdtests at master (sparse-checkout to 1.1 dirs only)..."
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Sparse checkout: clone with no blobs, then materialise just the dirs
# we want.  Avoids pulling the ~hundred-MB 1.0 corpus we already have.
git -C "$tmp" init -q
git -C "$tmp" remote add origin https://github.com/w3c/xsdtests.git
git -C "$tmp" config core.sparseCheckout true
{
    for d in $META_DIRS $DATA_DIRS; do echo "$d/*"; done
    for f in $EXTRA_FILES; do echo "$f"; done
} > "$tmp/.git/info/sparse-checkout"
git -C "$tmp" fetch --depth 1 -q origin master
git -C "$tmp" checkout -q FETCH_HEAD

# Copy the sparse tree into place.
for d in $META_DIRS $DATA_DIRS; do
    if [ -d "$tmp/$d" ]; then
        rm -rf "$d"
        cp -R "$tmp/$d" "$d"
    fi
done
for f in $EXTRA_FILES; do
    if [ -f "$tmp/$f" ]; then
        cp "$tmp/$f" "$f"
    fi
done

n_files="$(find . -type f \( -name '*.xsd' -o -name '*.xml' -o -name '*.testSet' \) | wc -l | tr -d ' ')"
echo "Done.  $n_files .xsd/.xml/.testSet files in $dir."
