#!/usr/bin/env bash
# Starts the Ladybird build with extension support.
#
#   scripts/run-ladybird.sh [options] [extension...] [url...] [other Ladybird flags...]
#
# Options:
#   --demo     also load the demo extension (toolbar button, popup, badge, context menu, Alt+Shift+M shortcut)
#   --trace    print every extension API call and its answer (WEBEXT_TRACE=1)
#   --fresh    use a throwaway profile (nothing is kept, store installs included); it starts with Google as the search
#              engine and without the first-run page (change the engine with --search=<name>)
#   --search=<name>  search engine for --fresh, as Ladybird's settings name it (default: Google)
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
if [[ -x "$ladybird/Build/debug/bin/Ladybird" ]]; then
    build_dir="$ladybird/Build/debug"
else
    build_dir="$ladybird/Build/release"
fi
binary="$build_dir/bin/Ladybird"

args=()
build=no
fresh=no
search_engine=Google
for arg in "$@"; do
    case "$arg" in
        --demo) args+=(--load-extension "$cgx/Integration/ladybird/demo/toolbar-extension") ;;
        --trace) export WEBEXT_TRACE=1 ;;
        --fresh) fresh=yes ;;
        --search=*) search_engine="${arg#--search=}" ;;
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
    ninja -C "$build_dir" ladybird
fi
if [[ ! -x "$binary" ]]; then
    echo "error: no Ladybird build at $binary" >&2
    echo "       build it with: ninja -C $build_dir ladybird   (or pass --build)" >&2
    exit 1
fi

if [[ "$fresh" == no ]]; then
    exec "$binary" "${args[@]}"
fi

# A throwaway profile, seeded so that typing words in the address bar searches, and removed when Ladybird exits.
profile="$(mktemp -d -t ladybird-fresh-XXXXXXXX)"
trap 'rm -rf "$profile"' EXIT
mkdir -p "$profile/config"
printf '{"firstRunComplete":true,"searchEngine":{"engine":"%s"}}\n' "$search_engine" > "$profile/config/Settings.json"
"$binary" --profile-path "$profile" "${args[@]}"
