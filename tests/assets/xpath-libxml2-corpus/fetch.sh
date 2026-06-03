#!/bin/sh
# Fetch libxml2's XPath test corpus.
#
# libxml2 is MIT-licensed; the test corpus is included in their main
# repo under test/XPath/.  We mirror the test inputs (expression files
# and source documents) and skip the libxml2-formatted `result/` files
# — the cross-impl bench at crates/bench/benches/xpath_libxml2_corpus.rs
# uses the inputs to drive both sup-xml and libxml2 simultaneously and
# compares their live outputs, so the canned reference outputs aren't
# needed.
#
# Run from anywhere; downloads into the same directory as this script.
# The resulting subtree is gitignored — fetch once, re-run to refresh.

set -e
dir="$(cd "$(dirname "$0")" && pwd)"
cd "$dir"

base="https://gitlab.gnome.org/GNOME/libxml2/-/raw/master/test/XPath"

# Expression-only tests: arithmetic / comparison / function tests
# that don't reference a document.
EXPR_FILES="
  base
  compare
  equality
  floats
  functions
  strings
"

# Stateful tests: reference a doc by filename prefix
# (chaptersbase → chapters, idsimple → id, …).
TEST_FILES="
  chaptersbase
  chaptersprefol
  idsimple
  langsimple
  mixedpat
  nodespat
  nssimple
  simpleabbr
  simplebase
  strbase
  unicodesimple
  usr1check
  vidbase
"

# Source XML documents, referenced by the stateful tests above.
DOCS="
  chapters
  id
  issue289
  lang
  mixed
  nodes
  ns
  simple
  str
  unicode
  usr1
  vid
"

mkdir -p expr tests docs

fetch() {
    out="$1"; url="$2"
    if [ -s "$out" ]; then return; fi
    echo "  $out"
    curl -fsSL "$url" -o "$out"
}

echo "Fetching libxml2 XPath test corpus into $dir ..."
for f in $EXPR_FILES;  do fetch "expr/$f"  "$base/expr/$f";  done
for f in $TEST_FILES;  do fetch "tests/$f" "$base/tests/$f"; done
for d in $DOCS;        do fetch "docs/$d"  "$base/docs/$d";  done

echo "Done.  $(find expr tests docs -type f | wc -l | tr -d ' ') files."
