#!/usr/bin/env bash
# Runs the C ABI smoke test (crates/webext-ffi/tests/smoke.c) under AddressSanitizer, UBSan and LeakSanitizer.
# The C<->Rust boundary hands ownership of memory back and forth, so this is the check that catches leaks and double frees.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cd "$root"

cargo build -q -p webext-ffi

mkdir -p "$tmp/pkg/public"
cat > "$tmp/pkg/manifest.json" <<'JSON'
{"manifest_version":3,"name":"C ABI Test","version":"1.0","permissions":["storage","alarms","cookies","tabs"],"optional_permissions":["history"],
 "background":{"service_worker":"bg.js"},
 "content_scripts":[{"matches":["https://example.com/*"],"js":["content.js"]}],
 "web_accessible_resources":[{"resources":["public/*"],"matches":["https://example.com/*"]}]}
JSON
echo "// bg" > "$tmp/pkg/bg.js"
echo "<!doctype html><html><head></head><body>x</body></html>" > "$tmp/pkg/page.html"
echo "// content script" > "$tmp/pkg/content.js"
printf png > "$tmp/pkg/public/logo.png"
(cd "$tmp/pkg" && zip -q -r "$tmp/ext.zip" .)

cc -std=c11 -g -fsanitize=address,undefined -fno-omit-frame-pointer \
    -Icrates/webext-ffi/include crates/webext-ffi/tests/smoke.c target/debug/libwebext_ffi.a \
    -lpthread -ldl -lm -o "$tmp/smoke"

# stdout is lost if LeakSanitizer fires, so judge by the exit status.
ASAN_OPTIONS=detect_leaks=1 "$tmp/smoke" "$tmp/ext.zip" "$tmp/pkg" > "$tmp/out.txt" 2> "$tmp/err.txt" || {
    cat "$tmp/err.txt" >&2
    echo "FAILED: sanitizer or assertion failure in the C ABI smoke test" >&2
    exit 1
}
grep -qx OK "$tmp/out.txt" && echo "OK: C ABI smoke test is clean under ASan/UBSan/LSan"
