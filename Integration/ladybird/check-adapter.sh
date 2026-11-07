#!/usr/bin/env bash
# Type-checks the C++ adapter against a Ladybird checkout's real AK/LibWebView headers, without building Ladybird.
# Run it in CI against upstream master to find out early when a Ladybird API change breaks the adapter.
#
#   check-adapter.sh <ladybird-checkout>
#
# The repository's adapter files are searched first, so this also works on a checkout that already has an older copy.
# Ladybird generates a few headers at configure time (AK/Debug.h and the per-library Export.h); this script stubs them.
# It does NOT link or run anything: that needs a full Ladybird build (see Integration/README.md).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
lb="${1:?usage: $0 <ladybird-checkout>}"
lb="$(cd "$lb" && pwd)"
cxx="${CXX:-clang++}"
gen="$(mktemp -d)"
trap 'rm -rf "$gen"' EXIT

mkdir -p "$gen/AK"
sed -E 's/^#[[:space:]]*cmakedefine01 ([A-Za-z0-9_]+)/#define \1 0/; s/^#[[:space:]]*cmakedefine ([A-Za-z0-9_]+).*/\/\/ \1 unset/' "$lb/AK/Debug.h.in" > "$gen/AK/Debug.h"
for lib in LibWebCommon:WEBCOMMON LibWebView:WEBVIEW LibWeb:WEB LibGfx:GFX LibURL:URL LibCore:CORE LibUnicode:UNICODE LibIPC:IPC LibMedia:MEDIA; do
    mkdir -p "$gen/${lib%%:*}"
    printf '#pragma once\n#define %s_API\n' "${lib##*:}" > "$gen/${lib%%:*}/Export.h"
done

out="$("$cxx" -std=c++23 -fsyntax-only -Wno-unqualified-std-cast-call -DENABLE_WEBEXT \
    -I "$here/files/Libraries" -I "$gen" -I "$lb" -I "$lb/Libraries" \
    -I "$here/../../crates/webext-ffi/include" \
    "$here/files/Libraries/LibWebView/Extensions/ExtensionManager.cpp" 2>&1 || true)"

# Any compiler error fails the check. (An earlier version only counted errors located in our own files and so missed ones
# reported inside AK headers, such as a missing AK_ALLOC_WITH_KMALLOC.) The stubbed generated headers are trivial, so they
# should not cause errors; if one does, fix the stub instead of filtering the output.
if grep -qE "(^|[^a-z])(fatal )?error:" <<<"$out"; then
    grep -E "error:" -A4 <<<"$out" | head -60
    echo "FAILED: the adapter does not type-check against $lb" >&2
    exit 1
fi
echo "OK: ExtensionManager type-checks against Ladybird at $(git -C "$lb" rev-parse --short HEAD 2>/dev/null || echo '?')"
