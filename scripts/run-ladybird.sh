#!/usr/bin/env bash
# Starts the Ladybird build with extension support.
#
#   scripts/run-ladybird.sh [options] [extension...] [url...] [other Ladybird flags...]
#
# Options:
#   --demo     also load the demo extension (toolbar button, popup, badge, context menu, Alt+Shift+M shortcut)
#   --trace    print every extension API call and its answer (WEBEXT_TRACE=1)
#   --fresh    use a throwaway profile (nothing is kept, store installs included)
#   --build    rebuild Ladybird first
#
# Any argument that is an unpacked extension directory or a .crx/.xpi/.zip file is loaded as an extension; everything else
# (URLs, Ladybird's own flags) is passed through. Extensions installed from the Chrome Web Store or addons.mozilla.org
# stay installed in the default profile, so they load again on the next start without arguments.
#
# Examples:
#   scripts/run-ladybird.sh --demo https://example.com/
#   scripts/run-ladybird.sh ~/extensions/darkreader.xpi https://en.wikipedia.org/
#   scripts/run-ladybird.sh https://chromewebstore.google.com/detail/dark-reader/eimadpbcbfnmbkopoojfekhnkhdbieeh
set -euo pipefail

cgx="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ladybird="${LADYBIRD_DIR:-$cgx/../ladybird}"
binary="$ladybird/Build/release/bin/Ladybird"

args=()
build=no
for arg in "$@"; do
    case "$arg" in
        --demo) args+=(--load-extension "$cgx/Integration/ladybird/demo/toolbar-extension") ;;
        --trace) export WEBEXT_TRACE=1 ;;
        --fresh) args+=(--temporary-profile) ;;
        --build) build=yes ;;
        -h|--help) sed -n '2,21p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)
            if [[ -d "$arg" && -f "$arg/manifest.json" ]] || [[ -f "$arg" && "$arg" =~ \.(crx|xpi|zip)$ ]]; then
                args+=(--load-extension "$(cd "$(dirname "$arg")" && pwd)/$(basename "$arg")")
            else
                args+=("$arg")
            fi
            ;;
    esac
done

if [[ "$build" == yes ]]; then
    ninja -C "$ladybird/Build/release" ladybird
fi
if [[ ! -x "$binary" ]]; then
    echo "error: no Ladybird build at $binary" >&2
    echo "       build it with: ninja -C $ladybird/Build/release ladybird   (or pass --build)" >&2
    exit 1
fi

exec "$binary" "${args[@]}"
