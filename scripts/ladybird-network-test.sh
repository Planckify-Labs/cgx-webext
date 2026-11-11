#!/usr/bin/env bash
# Checks the network APIs in a real Ladybird build: declarativeNetRequest rules, blocking webRequest listeners and
# webNavigation events, against a local server that records what reached it.
#
#   scripts/ladybird-network-test.sh
#
# Needs the Ladybird build of scripts/run-ladybird.sh and python3. Uses port 8765.
set -euo pipefail

cgx="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ladybird="${LADYBIRD_DIR:-$cgx/../ladybird}"
binary="$ladybird/Build/release/bin/Ladybird"
here="$cgx/Tests/Ladybird/network"
work="$(mktemp -d)"
trap 'rm -rf "$work"; [[ -n "${server:-}" ]] && kill "$server" 2>/dev/null || true' EXIT

[[ -x "$binary" ]] || { echo "error: no Ladybird build at $binary" >&2; exit 1; }

failures=0
# run <name> <start page> <extension dir or none>: loads the page and leaves what the server saw in $work/<name>.log
run() {
    local name="$1" page="$2" extension="$3" args
    rm -f "$work/log.jsonl"
    python3 "$here/server.py" 8765 "$here/site" "$work/log.jsonl" & server=$!
    sleep 0.5
    args=(--headless=screenshot --screenshot-delay 7 --screenshot-path "$work/shot.png" --temporary-profile)
    [[ "$extension" != none ]] && args+=(--load-extension "$here/$extension")
    timeout 90 "$binary" "${args[@]}" "http://127.0.0.1:8765/$page" > "$work/$name.browser.log" 2>&1 || true
    kill "$server"; wait "$server" 2>/dev/null || true; server=
    python3 -c '
import json, sys, urllib.parse
for line in open(sys.argv[1]):
    r = json.loads(line)
    extra = "".join(f" {k}={r[k]}" for k in ("x-test", "x-added") if r.get(k))
    print(urllib.parse.unquote(r["path"]) + extra)' "$work/log.jsonl" > "$work/$name.log"
}
# expect <name> <yes|no> <text>: whether the server log of a run has a line containing the text
expect() {
    local name="$1" want="$2" text="$3" got=no
    grep -qF -- "$text" "$work/$name.log" && got=yes
    if [[ "$got" == "$want" ]]; then
        echo "  ok    $name: $([[ $want == yes ]] && echo has || echo lacks) '$text'"
    else
        echo "  FAIL  $name: expected $([[ $want == yes ]] && echo to find || echo not to find) '$text'"
        failures=$((failures + 1))
    fi
}

echo "without extensions"
run plain index.html none
expect plain yes '/ads/ad.js'
expect plain yes '/api.json x-test=page'
expect plain yes '"redirected":"no"'

echo "declarativeNetRequest (static rules: block, redirect, redirect to an extension file, modifyHeaders)"
run dnr index.html dnr-ext
expect dnr no  '/ads/'
expect dnr yes '/redirected.js'
expect dnr no  '/redirect-me.js'
expect dnr no  '/to-extension.js'
expect dnr yes '/api.json x-added=by-rule'
expect dnr yes '"ad":"undefined","ok":"number","redirected":"yes","surrogate":"yes","cookieHeader":null,"replaced":"yes"'

echo "webRequest (blocking listeners in a background page) and webNavigation"
# The page loads after a moment, so that the background page has added its listeners.
run wr start.html wr-ext
expect wr no  '/ads/'
expect wr yes '/redirected.js'
expect wr yes '/api.json x-added=by-listener'
expect wr yes '"ad":"undefined","ok":"number","redirected":"yes","surrogate":"no","cookieHeader":null,"replaced":"yes"'
expect wr yes 'wr=completed main_frame 200 tab=true frame=0'
expect wr yes 'wr=error net::ERR_BLOCKED_BY_CLIENT'
expect wr yes 'wr=nav onBeforeNavigate frame=0 parent=-1 /index.html'
expect wr yes 'wr=nav onCommitted frame=0 parent=-1 /index.html'
expect wr yes 'wr=nav onDOMContentLoaded frame=0 parent=-1 /index.html'
expect wr yes 'wr=nav onCompleted frame=0 parent=-1 /index.html'
expect wr yes 'parent=0 /frame.html'
expect wr no  '_generated_background_page'

echo "a burst of requests (120 images) with a blocking listener, and with rules"
count_images() { grep -c '^/img/pixel.png?n=' "$work/$1.log" || true; }
for extension in wr-ext dnr-ext; do
    run "many-$extension" start-many.html "$extension"
    expect "many-$extension" yes 'many-loaded'
    images="$(count_images "many-$extension")"
    if [[ "$images" == 120 ]]; then
        echo "  ok    many-$extension: all 120 images were requested"
    else
        echo "  FAIL  many-$extension: $images of 120 images were requested"
        failures=$((failures + 1))
    fi
done

if (( failures > 0 )); then
    echo "$failures check(s) failed; logs are in $work (kept)"
    trap - EXIT
    exit 1
fi
echo "All network checks passed."
