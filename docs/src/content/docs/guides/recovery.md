---
title: Recovery mode
description: Parse malformed XML without losing data — opt-in recovery mode for third-party feeds, legacy migrations, diagnostic UIs.
---

For trusted-but-buggy input (third-party RSS / Atom feeds, legacy data
migration, diagnostic UIs), opt into recovery mode:

```rust
use sup_xml::{parse_str_with_recovered, ParseOptions};

let xml = "<r>tom & jerry<unclosed>";   // bare & + missing end tag

let opts = ParseOptions { recovery_mode: true, ..Default::default() };
let (doc, recovered) = parse_str_with_recovered(xml, &opts);
let doc = doc.unwrap();

for err in &recovered {
    eprintln!("recovered: {}", err.message);
}
```

The default — `recovery_mode: false` — fails fast on the first
non-trivial error, which is the right behaviour for trusted internal
data and untrusted user input.

## What gets preserved that other parsers drop

libxml2's `XML_PARSE_RECOVER` silently corrupts text in three common
malformed-input scenarios; SupXML's recovery mode preserves the
text in all three:

| input | libxml2 recovered text | SupXML recovered text |
|---|---|---|
| `<p>tom & jerry</p>` | `"tom  jerry"` (space-collapsed) | `"tom & jerry"` (literal) |
| `<p>x ]]> y</p>` | `"]>y"` (prefix dropped) | `"x ]]> y"` (preserved) |
| `<p>` followed by EOF | trailing text lost | trailing text preserved, `</p>` synthesised |

This is the design choice that motivates having our own recovery
mode at all — `XML_PARSE_RECOVER` is "be permissive and lossy";
ours is "be permissive and *don't lose user data*." Inspect libxml2's
behaviour against the same inputs with:

```bash
cargo bench -p sup-xml-bench --bench libxml2_recovery_inspector
```

## What's test-backed today

`crates/api/tests/recovery.rs` covers:

| Scenario | Behaviour | Test |
|---|---|---|
| Bare `&` in PCDATA | preserved as literal `&` in text node | `recover_logs_bare_ampersand_and_keeps_it_literal` |
| Bare `&` round-trip | serializes back as `&amp;` | `recover_bare_ampersand_serializes_back_as_amp_entity` |
| Unclosed element at EOF | synthetic close inserted, one error per level | `recover_logs_one_error_per_unclosed_level` |
| Unclosed → serialize | output is well-formed | `recover_unclosed_serializes_to_closed_xml` |
| Combined bare-`&` + unclosed | both recoveries applied in source order | `recover_combined_bare_amp_and_unclosed` |
| Bytes input variant | same behaviour from `parse_bytes_with_recovered` | `parse_bytes_with_recovered_handles_bare_amp` |
| Invalid UTF-8 | fatal — recovery cannot help | `parse_bytes_with_recovered_rejects_invalid_utf8` |
| Line/column tracking | preserved in recovered errors | `recovered_error_carries_line_info` |

## From the shell

```bash
# Recovery is a top-level CLI flag; works with every subcommand
sup-xml --recover lint broken.xml          # exit 0 if anything recovered
sup-xml --recover repair broken.xml -o clean.xml   # write the cleaned tree
sup-xml --recover format --pretty broken.xml       # pretty-print recovered
```

The `sup-xml repair` subcommand is the recovery-mode pipeline in
binary form — parse with `recovery_mode: true`, write the cleaned
document back. Useful for one-shot fixes to corrupted feed files.

## When NOT to use recovery mode

Recovery mode is for **inputs you've already decided you'd rather
salvage than reject.** Don't reach for it as a default because:

- **It quietly accepts ambiguous input.** A typo in your config file
  that recovery would round into a different tree shouldn't be
  silently corrected.
- **It hides upstream bugs.** If the upstream is generating malformed
  XML, the right fix is to fix the upstream, not paper over it.
- **Recovery isn't deterministic across implementations.** SupXML
  recovers more conservatively than libxml2, but a recovered tree
  isn't guaranteed to round-trip to identical output across parsers.

For untrusted input (web request bodies, file uploads), prefer
strict parsing + a fallback path — show the user a real error and
let them re-submit, rather than silently transforming their input.
