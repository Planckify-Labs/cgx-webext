#!/usr/bin/env bash
# Downloads popular signed extensions from addons.mozilla.org and reports how well they are supported.
# Needs network. The packages are cached in $CACHE (default: Tools/.real-world-cache) and are not part of this repository.
#
#   Tools/real_world.sh [slug...]        e.g. Tools/real_world.sh ublock-origin darkreader
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cache="${CACHE:-$root/Tools/.real-world-cache}"
slugs=("$@")
[[ ${#slugs[@]} -gt 0 ]] || slugs=(ublock-origin darkreader vimium-ff styl-us bitwarden-password-manager)

mkdir -p "$cache"
cargo build -q --release --manifest-path "$root/Cargo.toml" -p webext-cli
files=()
for slug in "${slugs[@]}"; do
    xpi="$cache/$slug.xpi"
    if [[ ! -s "$xpi" ]]; then
        curl -fsSL --max-time 300 -o "$xpi" "https://addons.mozilla.org/firefox/downloads/latest/$slug/latest.xpi"
    fi
    files+=("$xpi")
done
"$root/target/release/webext-coverage" inspect "${files[@]}" --json
