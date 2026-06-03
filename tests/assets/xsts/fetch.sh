#!/bin/sh
# Fetch the W3C XML Schema Test Suite (XSTS 2007-06-20).
#
# Run from anywhere; downloads into the same directory as this script.
# The resulting `xmlschema2006-11-06/` tree is gitignored — fetch
# once, re-run any time to refresh.

set -e
dir="$(cd "$(dirname "$0")" && pwd)"
cd "$dir"

if [ -d xmlschema2006-11-06 ]; then
    echo "XSTS already present at $dir/xmlschema2006-11-06"
    echo "Delete it to re-fetch."
    exit 0
fi

url="https://www.w3.org/XML/2004/xml-schema-test-suite/xmlschema2006-11-06/xsts-2007-06-20.tar.gz"
echo "Downloading $url..."
curl -sLo xsts.tar.gz "$url"
tar xzf xsts.tar.gz
rm xsts.tar.gz
echo "Extracted to $dir/xmlschema2006-11-06"
echo "Test count: $(find xmlschema2006-11-06 -name '*.testSet' | wc -l) .testSet manifests"
