# HTML benchmark fixtures

Real-world HTML pages used by `cargo bench -p sup-xml-bench --bench html_parse`.
Not checked into git — fetch a fresh copy with:

    tests/assets/html/fetch.sh

The bench skips any fixture that's missing, so you can fetch a subset
if you only want a few.

## Fixtures

| filename | source | size | what it exercises |
|---|---|---|---|
| `hn.html` | https://news.ycombinator.com/ | ~35 KB | minimal, table-heavy, no scripts |
| `mdn_table.html` | https://developer.mozilla.org/en-US/docs/Web/HTML/Element/table | ~240 KB | tech-doc with code blocks, deep nesting |
| `stackoverflow_rust.html` | https://stackoverflow.com/questions/tagged/rust | ~250 KB | Q&A list, dynamic-looking server-rendered |
| `bbc_news.html` | https://www.bbc.com/news | ~320 KB | modern news landing page |
| `github_rust.html` | https://github.com/rust-lang/rust | ~370 KB | heavy modern UI, lots of components |
| `wikipedia_rust.html` | https://en.wikipedia.org/wiki/Rust_(programming_language) | ~590 KB | medium Wikipedia article |
| `wikipedia_languages.html` | https://en.wikipedia.org/wiki/List_of_Wikipedias | ~900 KB | very table-heavy Wikipedia article |
| `wikipedia_ww2.html` | https://en.wikipedia.org/wiki/World_War_II | ~1.2 MB | the big-document case |
| `guardian.html` | https://www.theguardian.com/international | ~1.3 MB | large news landing, many articles |

## Licenses

- Wikipedia and MDN content is CC-BY-SA — redistributing is OK in
  principle, but we still keep them out of git for simplicity.
- BBC, Guardian, GitHub, Stack Overflow, Hacker News content is
  copyrighted by their respective owners.  Use of these fetched
  copies for local benchmarking falls under fair use for engineering
  purposes (cached, transient, no public redistribution).
- Sites may rate-limit or block automated fetches; the script uses
  a desktop browser User-Agent and a 20s timeout.  If a fetch fails
  or returns a captcha page, that fixture is just absent for the
  bench run.
