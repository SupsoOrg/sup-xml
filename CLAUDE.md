# Working in this repo

Notes for AI assistants and contributors working on sup-xml.

## Running tests

**Before reporting any non-trivial change as done, run:**

```
cargo test-all
```

That's a cargo alias (defined in `.cargo/config.toml`) for
`cargo test --workspace --all-features`.  It exercises every test
in every crate under every optional feature (`xsd`, `xslt`, `html`,
`serde`, `tokio`, `network-resolver`, `full-encodings`, `c-abi`)
plus doctests, integration tests, and example builds.

The expected baseline is **all green** — 0 failures across all test
suites.  If a change introduces failures or surfaces previously-
masked failures, fix them in the same change rather than leaving
the suite broken for someone else.

`cargo check-all` (also an alias) is a faster "still compiles?"
pass that skips test execution — useful mid-edit, but
`cargo test-all` is the gate before declaring work done.

## Bias for the long-term best solution

Bias your decisions towards what is best for the long-term.
Avoid hacky fixes if there is a better long-term, correct way.

## Code style

**No in-progress notes in comments.** When you finish a task, the
checked-in code should read like a professional library, not a
work-in-progress journal. Also try to do work in a way where the
final state is like a completed pull request, don't just do half
and then leave half unfinished, which would leave the project
in a weird unfinished state.

Avoid these patterns in committed code:

- `// Pass 1 —`, `// Pass 2 —`, `// Step 1`, `// Step 2` (numbered
  phases of the implementation you happened to write).
- `// task #4`, `// see follow-up task`, `// PR3+ adds`, `// v1
  limitation` (references to your workflow / planning).
- `// NOTE: this test uses xsi:nil because extension merging hasn't
  landed yet` (commentary about what was true mid-session).
- `// blocked on X`, `// deferred until Y`.

If a comment is needed at all, it should explain *what the code does
or why it's structured this way for a future reader who doesn't know
the change history*. Reference the spec section or the runtime
invariant, not the PR / task / session that produced the code.

Bad:
```rust
// Pass 1 — rewrite top-level NAMED complex types.
// (Pass 2 handles inline anonymous types attached to elements.)
```

Good:
```rust
// Top-level NAMED complex types live in `types`; anonymous inline
// types attached to element decls are patched below.
```

Bad:
```rust
// task #4: parse_simple_restriction drops the base's Variety
```

Good:
```rust
// Inherit the base's Variety so length facets count items, not chars.
```

**No comments that restate the code.** If a function is named
`merge_extension_chains` and the body iterates types and composes
them, you don't need `// iterate types and compose them` above the
loop.

**Don't reference deprecated/removed things.** No
`// removed in v0.2`, no `// formerly _foo`, no
`// see also old impl in commit abc123`. Git history covers this.

**Prefer combinators over nested `if let` / `match` on `Option`/`Result`.**
When you find yourself writing:

```rust
if let Some(x) = thing {
    if let Some(y) = x.lookup(...) {
        return Some(y);
    }
}
None
```

rewrite as a chain:

```rust
thing.and_then(|x| x.lookup(...))
```

`map`, `and_then`, `or_else`, `unwrap_or`, `?` exist precisely to
replace this pattern.  Nested `if let` is fine when the body needs
multiple statements or shadows a binding — but when it's a single
operation you're trying to thread, the chain reads better and
doesn't dress destructuring up to look like a C-style assignment.

This isn't about avoiding `if let` in general — it's idiomatic
Rust pattern matching, not an assignment-in-if footgun.  It IS
about reaching for combinators first when the operation is a
linear "if Some, transform; else propagate None."

## Documentation

Doc comments (`///`) explain *what callers need to know to use the
item correctly* — invariants, spec references, error conditions,
performance characteristics. Not the implementation history.

Module-level docs (`//!`) can describe architecture (the parser
runs in N passes; the validator drives an event stream) because
that's load-bearing for a reader trying to navigate the file.

## When in doubt

Ask "would a senior engineer reviewing this code for inclusion in a
library they're going to depend on find this comment helpful, or
would they ask me to remove it?"
