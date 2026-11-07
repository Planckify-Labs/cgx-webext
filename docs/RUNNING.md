# Running Ladybird with extensions

## The build on this machine

Already built, with extension support, at:

```
/home/cstralpt/cgx-translation-research/ladybird/Build/release/bin/Ladybird
```

(branch `cgx-webext` of the Ladybird clone). Below, `LB` is that path and `CGX` is
`/home/cstralpt/cgx-translation-research/cgx-webext`.

```sh
LB=/home/cstralpt/cgx-translation-research/ladybird/Build/release/bin/Ladybird
CGX=/home/cstralpt/cgx-translation-research/cgx-webext
```

## 1. Try the demo extension first

```sh
$LB --load-extension $CGX/Integration/ladybird/demo/extension file://$CGX/Integration/ladybird/demo/pages/test.html
```

You should see a green banner at the bottom of the page that says
"content script + background page OK. Pages seen: N". N counts up across reloads and is kept in
`chrome.storage.local`. The page itself reports `window.chrome: undefined`, so the page cannot see
the extension's API.

The terminal prints the extension's address, for example
`Loaded extension: cgx-webext demo 1.0 at chrome-extension://daamioghnkdecdegkndfgnncbkimmffe/`. Open
`<that address>popup.html` in a tab to see an extension page using `chrome.runtime` and `chrome.storage`.

Add `--temporary-profile` to start fresh each time.

## 2. Install from the Chrome Web Store or addons.mozilla.org

Open an extension's page in the store and press its button:

```sh
$LB https://chromewebstore.google.com/detail/dark-reader/eimadpbcbfnmbkopoojfekhnkhdbieeh
$LB https://addons.mozilla.org/en-US/firefox/addon/darkreader/
```

1. Press **Add to Chrome** or **Add to Firefox**.
2. Ladybird asks "Add "Dark Reader"?" and lists what the extension can do. Press **Add extension** (Chrome Web Store)
   or **Add** (addons.mozilla.org).
3. Ladybird downloads the package from the store, checks that it is what the store offered (the extension id for the
   Chrome Web Store, the SHA-256 hash for addons.mozilla.org), installs it and starts it. The button turns into
   **Remove from Chrome** or **Remove**. The terminal prints `Installed extension from ...`.
4. The extension loads again every time you start Ladybird with the same profile (`Loaded extension: ... (from ...)`).
   The package is kept in `<profile data>/ExtensionPackages/`.
5. **Remove from Chrome** asks before removing; **Remove** on addons.mozilla.org removes at once, as in Firefox.
   Removing deletes the package and the extension's stored data.

What you will notice:

- The Chrome Web Store still shows "Switch to Chrome?" and a "Switch to Chrome to install extensions" banner. Google
  shows these to every browser that is not Google Chrome (Brave, Edge and Opera users see them too); they do not stop
  the install. Press **No thanks**.
- addons.mozilla.org is sent a Firefox User-Agent (rule `WebCompat/addons.mozilla.org.json`), because it only offers
  "Add to Firefox" to Firefox.
- Do not use `--temporary-profile` if you want installs to stay.

For scripted tests without a window, `--headless --auto-accept-extension-prompts` answers yes to the prompts.
Without that flag a headless browser declines every install.

### Loading a file instead

`--load-extension` still works, for unpacked directories and `.crx`/`.xpi`/`.zip` files. The download helper fetches
a store's package without the browser:

```sh
$CGX/Tools/fetch_extension.sh https://chromewebstore.google.com/detail/dark-reader/eimadpbcbfnmbkopoojfekhnkhdbieeh ~/extensions
$LB --load-extension ~/extensions/eimadpbcbfnmbkopoojfekhnkhdbieeh.crx https://example.com
```

Check what a package needs before you load it:

```sh
cd $CGX && cargo run -q -p webext-cli -- inspect ~/extensions/eimadpbcbfnmbkopoojfekhnkhdbieeh.crx
```

## What to expect

These work, and were tested in this build with the demo extension in Chrome and Firefox form:

- extension pages (`chrome-extension://…`, `moz-extension://…`) with their own origin
- background pages, scripts and service workers (run as hidden pages)
- content scripts at `document_start`, `document_end` and `document_idle`, with CSS
- `chrome.*` and `browser.*` (callbacks, promises, `lastError`)
- `storage`, `i18n`, `alarms`, `permissions`, `runtime` messaging and ports between pages and content scripts

These do not work yet:

- **Most real extensions only partly work.** Dark Reader (Chrome and Firefox builds) loads, its background starts and
  its content scripts run, but it does not darken pages yet. It needs `tabs.query` and similar browser APIs, which
  still answer "This browser extension API is not implemented yet". The terminal shows these errors.
- No toolbar button, popup window or extensions page in the UI. Open extension pages by URL.
- `tabs`, `windows`, `action`, `contextMenus`, `cookies`, `scripting`, `webRequest`, `declarativeNetRequest` and
  `webNavigation` are not implemented, so ad blockers such as uBlock Origin will not block anything.
- Content scripts share the page's JavaScript world (Chrome's "MAIN" world). A hostile page can tamper with them,
  so only try extensions on sites you trust.
- Signatures of `.crx`/`.xpi` files are not verified. Store installs are downloaded over HTTPS from the store and
  checked against the id or hash the store gave, which is what ties them to the store.
- Extensions cannot be disabled, only removed.

When an extension does not work, the terminal messages (`Unhandled JavaScript exception`, `not implemented yet`)
usually name the missing piece.

## Rebuilding after changes

```sh
cd /home/cstralpt/cgx-translation-research/ladybird
ninja -C Build/release -j8 ladybird
```

Let builds finish. Interrupting `ninja` can damage its log, and the next build then recompiles almost everything.

To put the integration on another Ladybird checkout, see [Integration/README.md](../Integration/README.md).
