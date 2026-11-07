#!/usr/bin/env bash
# Applies the cgx-webext integration to a Ladybird (or Ladybird-derived) checkout.
#
#   apply.sh <ladybird-checkout> [--check]
#
# --check only verifies that everything would apply cleanly. Nothing is committed; review and commit the result yourself.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
target="${1:-}"
mode="${2:-apply}"

if [[ -z "$target" || ! -f "$target/CMakeLists.txt" || ! -d "$target/Libraries/LibWebView" ]]; then
    echo "usage: $0 <ladybird-checkout> [--check]" >&2
    echo "error: '$target' does not look like a Ladybird checkout" >&2
    exit 2
fi
target="$(cd "$target" && pwd)"

if grep -q "ENABLE_WEBEXT" "$target/Meta/CMake/cmake_options.cmake" 2>/dev/null; then
    echo "already applied: ENABLE_WEBEXT is defined in $target" >&2
    exit 0
fi

supported="$(tr -d '[:space:]' < "$here/SUPPORTED_COMMIT")"
head="$(git -C "$target" rev-parse HEAD 2>/dev/null || echo unknown)"
if [[ "$head" != "$supported" ]]; then
    echo "note: this integration was written against Ladybird $supported" >&2
    echo "      your checkout is at                      $head" >&2
    echo "      patches may need adjusting; see Integration/README.md" >&2
fi

for patch in "$here"/patches/*.patch; do
    git -C "$target" apply --check "$patch"
done
if [[ "$mode" == "--check" ]]; then
    echo "OK: patches apply cleanly to $target"
    exit 0
fi

for patch in "$here"/patches/*.patch; do
    git -C "$target" apply "$patch"
    echo "applied $(basename "$patch")"
done
cp -R "$here/files/." "$target/"
echo "copied new files:"
(cd "$here/files" && find . -type f | sed 's|^\./|  |')

cat <<NEXT

Done. Build with:
  cmake -S "$target" -B "$target/Build/webext" -DENABLE_WEBEXT=ON -DWEBEXT_SOURCE_DIR="$(cd "$here/../.." && pwd)" ...
Without -DENABLE_WEBEXT=ON the build is identical to stock Ladybird.
NEXT
