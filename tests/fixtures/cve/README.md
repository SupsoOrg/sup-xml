# tests/fixtures/cve/

Attack inputs the parser must reject (or bound the cost of)
without hanging, OOM-ing, or crashing.  Each file targets a
specific known XML attack class.

## Files

| File | Attack | Defense in our parser |
|---|---|---|
| `billion_laughs.xml` | Exponential entity expansion (XEE) — N nested entity references each referring to a string of N copies of the previous entity.  Naive expansion is O(2^depth) bytes from a few-KB source. | Parser caps entity-expansion depth and total expanded size; rejects with a clear error well before the bomb detonates. |

## How they're used

The CVE fixtures are loaded by `crates/api/tests/parse_fixtures.rs`
(see line 180-ish).  Each test asserts:

1. The parser **returns an error** (does not silently succeed and
   produce a giant DOM).
2. Wall time stays bounded (the test would time out if defense
   regressed).
3. RSS stays bounded (no allocation explosion before the parser
   notices).

## How to run

```sh
cargo test -p sup-xml --test parse_fixtures cve
```

## Adding a new attack

1. Find the attack class (e.g., XXE, quadratic blowup, denial of
   service via deep recursion).  Construct a minimal reproducer.
2. Drop it here with a descriptive filename (`billion_laughs.xml`,
   `quadratic_blowup.xml`, etc.).
3. Add a test in `crates/api/tests/parse_fixtures.rs` that asserts
   the parser rejects it and the rejection happens fast.
4. Reference the CVE / advisory ID in the test comment so future
   readers know what the input is defending against.
