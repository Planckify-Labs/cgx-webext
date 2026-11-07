#!/usr/bin/env bash
# Downloads an extension package from the Chrome Web Store or addons.mozilla.org, to load it with
#   Ladybird --load-extension <file>
#
#   Tools/fetch_extension.sh <store URL | Chrome extension id | AMO slug> [output directory]
#
# Examples:
#   Tools/fetch_extension.sh https://chromewebstore.google.com/detail/dark-reader/eimadpbcbfnmbkopoojfekhnkhdbieeh
#   Tools/fetch_extension.sh eimadpbcbfnmbkopoojfekhnkhdbieeh
#   Tools/fetch_extension.sh https://addons.mozilla.org/en-US/firefox/addon/ublock-origin/
#   Tools/fetch_extension.sh ublock-origin
#
# The stores' own "Add to Chrome" / "Add to Firefox" buttons only work in those browsers. Check the store's terms before
# redistributing anything you download this way.
set -euo pipefail

input="${1:?usage: $0 <store URL | Chrome extension id | AMO slug> [output directory]}"
out_dir="${2:-.}"
mkdir -p "$out_dir"

# A recent Chrome version; the update service only serves packages compatible with the version you name.
chrome_version="${CHROME_VERSION:-138.0.0.0}"

chrome_id=""
amo_slug=""
if [[ "$input" =~ ([a-p]{32}) ]]; then
    chrome_id="${BASH_REMATCH[1]}"
elif [[ "$input" =~ addons\.mozilla\.org/.*/addon/([^/?#]+) ]]; then
    amo_slug="${BASH_REMATCH[1]}"
elif [[ "$input" =~ ^[A-Za-z0-9._@-]+$ ]]; then
    amo_slug="$input"
else
    echo "error: '$input' is neither a Chrome Web Store URL/id nor an addons.mozilla.org URL/slug" >&2
    exit 2
fi

if [[ -n "$chrome_id" ]]; then
    file="$out_dir/$chrome_id.crx"
    url="https://clients2.google.com/service/update2/crx?response=redirect&prodversion=${chrome_version}&acceptformat=crx2,crx3&x=id%3D${chrome_id}%26uc"
    echo "Chrome Web Store: $chrome_id"
else
    file="$out_dir/$amo_slug.xpi"
    url="https://addons.mozilla.org/firefox/downloads/latest/${amo_slug}/latest.xpi"
    echo "addons.mozilla.org: $amo_slug"
fi

curl -fsSL --retry 3 --max-time 600 -o "$file.part" "$url"
mv "$file.part" "$file"
if [[ "$(head -c 4 "$file")" != "Cr24" && "$(head -c 2 "$file")" != "PK" ]]; then
    echo "error: the download is not an extension package (got $(file -b "$file" 2>/dev/null || echo 'unknown data'))" >&2
    exit 1
fi
echo "Saved $file ($(du -h "$file" | cut -f1))"
