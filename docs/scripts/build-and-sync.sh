#!/usr/bin/env bash
# Build the docs and copy the static output into the supso_website Rails
# app at first_party_projects/sup_xml/dist/.  Run from anywhere — the
# script resolves its own paths relative to itself.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
docs_dir="$(cd "$script_dir/.." && pwd)"
dest="${SUPSO_WEBSITE_ROOT:-$docs_dir/../../supported_source/supso_website}/first_party_projects/sup_xml/dist"

if [[ ! -d "$(dirname "$dest")" ]]; then
  echo "destination parent missing: $(dirname "$dest")" >&2
  echo "set SUPSO_WEBSITE_ROOT to point at the Rails app root" >&2
  exit 1
fi

cd "$docs_dir"
npm run build

mkdir -p "$dest"
rsync -a --delete "$docs_dir/dist/" "$dest/"

echo "synced $docs_dir/dist/ -> $dest/"
