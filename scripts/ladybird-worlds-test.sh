#!/usr/bin/env bash
# Checks the isolated worlds of content scripts in a real Ladybird build: what the page and the content scripts can see of
# each other, MAIN-world scripts, modules a content script imports, Firefox's cloneInto/exportFunction/wrappedJSObject,
# strict-CSP pages, and navigations that tear worlds down.
#
#   scripts/ladybird-worlds-test.sh
#
# Needs the Ladybird build of scripts/run-ladybird.sh and python3. Uses port 8765. The pages and the content scripts report
# each check to the server (/report?c=<name>&ok=<0|1>); every name in Tests/Ladybird/worlds/expected.txt must be reported
# as ok, so a check that never ran fails too.
set -euo pipefail

cgx="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ladybird="${LADYBIRD_DIR:-$cgx/../ladybird}"
binary="$ladybird/Build/release/bin/Ladybird"
here="$cgx/Tests/Ladybird/worlds"
work="$(mktemp -d)"
trap 'rm -rf "$work"; [[ -n "${server:-}" ]] && kill "$server" 2>/dev/null || true' EXIT

[[ -x "$binary" ]] || { echo "error: no Ladybird build at $binary" >&2; exit 1; }

rm -f "$work/log.jsonl"
python3 "$here/server.py" 8765 "$here/site" "$work/log.jsonl" & server=$!
sleep 0.5
# page.html -> page2.html -> page3.html (an iframe that goes away) -> csp.html -> done.html, each by the page itself.
timeout 120 "$binary" --headless=screenshot --screenshot-delay "${WORLDS_TEST_SECONDS:-30}" --screenshot-path "$work/shot.png" --temporary-profile \
    --load-extension "$here/ext-a" --load-extension "$here/ext-b" --load-extension "$here/ext-ff" \
    "http://127.0.0.1:8765/page.html" > "$work/browser.log" 2>&1 || browser_status=$?
kill "$server"; wait "$server" 2>/dev/null || true; server=

status=0
python3 - "$work/log.jsonl" "$here/expected.txt" "$work/browser.log" <<'PY' || status=$?
import json, sys, urllib.parse
log, expected, browser_log = sys.argv[1:4]
results = {}
for line in open(log):
    path = json.loads(line)["path"]
    if not path.startswith("/report?"):
        continue
    q = urllib.parse.parse_qs(urllib.parse.urlsplit(path).query)
    name = q.get("c", [""])[0]
    results.setdefault(name, []).append((q.get("ok", ["0"])[0] == "1", q.get("d", [""])[0]))
failures = 0
for name in [l.strip() for l in open(expected) if l.strip() and not l.startswith("#")]:
    got = results.get(name)
    if not got:
        print(f"  FAIL  {name}: never reported")
        failures += 1
    elif not all(ok for ok, _ in got):
        print(f"  FAIL  {name}: {[d for ok, d in got if not ok][0]!r}")
        failures += 1
    else:
        print(f"  PASS  {name}")
# The browser must still be alive and must not have hit a failed assertion.
text = open(browser_log, errors="replace").read()
for marker in ("VERIFICATION FAILED", "ASSERTION FAILED", "Segmentation fault", "Aborted", "terminated"):
    if marker in text:
        print(f"  FAIL  browser log contains {marker!r}")
        failures += 1
sys.exit(1 if failures else 0)
PY
if (( status != 0 )); then
    echo "Some world checks failed; the browser log is $work/browser.log (kept)"
    trap - EXIT
    exit 1
fi
echo "All world checks passed."
