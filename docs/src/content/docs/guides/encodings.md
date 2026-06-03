---
title: Character encodings
description: Auto-detection, hand-tuned UTF-8 / Latin-1 / UTF-16 / EBCDIC, plus the full WHATWG set via encoding_rs.
---

The parser auto-detects the input's encoding and transcodes to UTF-8 before
parsing — matching libxml2 and the XML 1.0 spec's requirement (§ 4.3.3)
that processors accept both UTF-8 and UTF-16:

```rust
use sup_xml::{parse_bytes, ParseOptions};
let doc = parse_bytes(latin1_or_utf16_or_gb2312_bytes, &ParseOptions::default())?;
```

Detection follows XML 1.0 Appendix F: BOM, then the four-byte autodetect
signatures for UTF-32 / UTF-16 / EBCDIC, then the `<?xml encoding="..."?>`
declaration. UTF-8 input stays zero-copy; non-UTF-8 input pays one
allocation for the decoded buffer.

## Strict UTF-8 mode

```rust
use sup_xml::{parse_bytes, ParseOptions};
let opts = ParseOptions { auto_transcode: false, ..Default::default() };
let doc = parse_bytes(must_be_utf8_bytes, &opts)?;
```

## Built-in encodings — no dependency

Hand-tuned, byte-for-byte audited:

- **UTF-8, US-ASCII** — zero-copy passthrough
- **ISO-8859-1 (Latin-1), Windows-1252** — SWAR-accelerated
- **UTF-16 LE / BE** — BOM-aware, surrogate-pair validated
- **UTF-32 LE / BE** — BOM-aware (also accepts `UCS-4LE` / `UCS-4BE` aliases)
- **EBCDIC variants**:
  - IBM037 (CCSID 37) — US/Canada Latin
  - IBM500 (CCSID 500) — International EBCDIC
  - IBM1047 (CCSID 1047) — Open Systems / z/OS Unix Services Latin-1
  - IBM1140 (CCSID 1140) — IBM037 with the Euro sign update

## Via encoding_rs — `full-encodings` feature (default on)

The full WHATWG Encoding set: Shift_JIS, EUC-JP, ISO-2022-JP, GB2312, GBK,
GB18030, Big5, EUC-KR, ISO-8859-2…16, Windows-1250…1258, KOI8-R, KOI8-U,
IBM866, macintosh, x-mac-cyrillic, TIS-620 (via `windows-874`), and others.

Disable the feature to drop the dependency and reject these inputs with a
clean error.

## Deliberately unsupported

- **UTF-7** — not supported for security reasons (UTF-7 enables MIME header
  smuggling and XSS in HTML contexts; we won't ship the decoder).
