#!/usr/bin/env bash
# Fetch real-world HTML fixtures for the html_parse bench.  See
# SOURCES.md for the URL list.  Skips fixtures already present so
# re-runs are cheap; use `rm tests/assets/html/*.html` to force
# a re-fetch.

set -euo pipefail

cd "$(dirname "$0")"

UA='Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36'
TIMEOUT=20

fetch() {
    local out="$1"
    local url="$2"
    if [[ -s "$out" ]]; then
        printf '  %-32s already present (%s)\n' "$out" "$(wc -c < "$out") bytes"
        return
    fi
    if curl -sLo "$out.tmp" -A "$UA" --max-time "$TIMEOUT" "$url"; then
        # Refuse files smaller than 5 KB — likely a captcha / block page.
        local size
        size=$(wc -c < "$out.tmp")
        if (( size < 5000 )); then
            printf '  %-32s SKIPPED (got %s bytes — likely blocked)\n' "$out" "$size"
            rm -f "$out.tmp"
        else
            mv "$out.tmp" "$out"
            printf '  %-32s fetched (%s bytes)\n' "$out" "$size"
        fi
    else
        printf '  %-32s FAILED (curl exited non-zero)\n' "$out"
        rm -f "$out.tmp"
    fi
}

echo "Fetching HTML fixtures into $(pwd)..."
fetch hn.html                  'https://news.ycombinator.com/'
fetch mdn_table.html           'https://developer.mozilla.org/en-US/docs/Web/HTML/Element/table'
fetch stackoverflow_rust.html  'https://stackoverflow.com/questions/tagged/rust'
fetch bbc_news.html            'https://www.bbc.com/news'
fetch github_rust.html         'https://github.com/rust-lang/rust'
fetch wikipedia_rust.html      'https://en.wikipedia.org/wiki/Rust_(programming_language)'
fetch wikipedia_languages.html 'https://en.wikipedia.org/wiki/List_of_Wikipedias'
fetch wikipedia_ww2.html       'https://en.wikipedia.org/wiki/World_War_II'
fetch guardian.html            'https://www.theguardian.com/international'

echo
echo "Done.  Run benches with:"
echo "  cargo bench -p sup-xml-bench --bench html_parse"
