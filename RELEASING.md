# Releasing sup-xml

How to cut a new release of the sup-xml workspace. Releases are
currently a manual sequence — there is no release-automation
workflow. Follow these steps in order.

## Versioning

The version is single-sourced from the workspace root: every crate
inherits it via `version.workspace = true`. The publishable crates
share one version number and are released together.

- `crates/api` (`sup-xml`) — the public Rust API
- `crates/core` (`sup-xml-core`)
- `crates/tree` (`sup-xml-tree`)
- `crates/xslt` (`sup-xml-xslt`)
- `crates/cli` (`sup-xml-cli`)

`sup-xml-compat` and `sup-xml-bench` are `publish = false` and are
never sent to the registry.

Follow semver: a backward-compatible feature release is a minor bump
(`1.0.0` → `1.1.0`); a breaking change is a major bump.

### First publish

The crates are not yet on crates.io — `v1.0.0` exists only as a git
tag. The first `cargo publish` claims each crate name. Two things to
settle before that first publish, because they are irreversible:

- **The source goes fully public.** Anything in the published crates
  is readable by anyone.
- **The license gate ships with it.** `ensure_licensed` runs on the
  first parse, so every downstream consumer who pulls these crates
  from crates.io hits that gate. Confirm that is the intended
  distribution model before claiming the names.

## Steps

### 1. Pre-flight

Start from a clean tree on `main`, fully up to date, with any pending
work already landed and CI green.

```sh
git switch main && git pull
cargo test-all          # the all-green baseline — must pass
```

### 2. Bump the version

Edit the workspace version and the inter-crate dependency pins so they
all move in lockstep. The pins are caret requirements (`version =
"1.0.0"` means `^1.0.0`), so an out-of-date pin would still resolve
against a newer release — but lockstep keeps the published metadata
honest and stops a consumer pulling mismatched crate versions.

- `Cargo.toml` → `[workspace.package] version`
- The 10 `version = "…"` pins on internal `path` dependencies in
  `crates/core`, `crates/tree`, `crates/xslt`, `crates/api`,
  `crates/cli`, and `crates/compat`.

Then regenerate the lockfile:

```sh
cargo check-all
```

### 3. Verify packaging

Only the leaf crate can be dry-run in isolation. A dependent crate
cannot be packaged until its dependencies already exist on crates.io
at the new version — `cargo publish --dry-run -p sup-xml-core` fails
with "no matching package named `sup-xml-tree`" because the package
step strips the local `path` and resolves the dependency from the
registry. So the real verification of the dependents happens
crate-by-crate during the ordered publish in step 5 (each one's
verification build finds its deps on the registry by the time its
turn comes).

Dry-run the leaf, and use a workspace build as the pre-flight for the
rest:

```sh
cargo publish --dry-run -p sup-xml-tree   # leaf: full package + verify
cargo build --workspace --all-features    # everything still compiles
```

### 4. Commit, tag, push

```sh
git commit -am "Release <version>"
git tag v<version>
git push && git push --tags
```

### 5. Publish to crates.io

Publish in dependency order so each crate's dependencies already exist
on the registry. Publish one at a time; the index needs a moment to
register each crate before a dependent will resolve.

```sh
cargo publish -p sup-xml-tree
cargo publish -p sup-xml-core
cargo publish -p sup-xml-xslt
cargo publish -p sup-xml
cargo publish -p sup-xml-cli
```

### 6. GitHub release

```sh
gh release create v<version> --generate-notes
```

## Notes

- This process is manual. If releases become frequent, a tool such as
  `cargo-release` or `release-plz` automates the version bump, lockstep
  pin update, tag, and ordered publish — including the unpublished-
  dependency ordering that makes a single upfront dry-run impossible.
