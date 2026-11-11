# Running Ladybird with extensions

## Quickest way

```sh
cd /home/cstralpt/cgx-translation-research/cgx-webext
scripts/run-ladybird.sh --demo https://example.com/
```

`scripts/run-ladybird.sh --help` lists the options (`--demo`, `--trace`, `--fresh`, `--build`). Any extension directory or
`.crx`/`.xpi`/`.zip` you pass is loaded; URLs and Ladybird's own flags pass through.

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

## 3. The extension UI

Every window's toolbar has, right of the address field:

- a **button for each pinned extension**, with its icon and badge text. Click it to open the extension's **popup**: a
  small panel under the button, sized to the popup page (up to 800×600), closed by Escape, a click outside, or the page
  calling `window.close()`. An extension without a popup gets `action.onClicked` instead. Right-click the button for the
  extension's own "action" menu items, Options, Unpin and Remove.
- the **extensions button** (puzzle piece). It opens a panel listing every installed extension with its icon, name and
  what it can access on the current site; click a row to open that extension, the pin to put it on the toolbar, ⋮ for
  Options and Remove. "Find more extensions" opens the Chrome Web Store or Firefox Add-ons.

Extensions' context-menu items appear at the end of the page, link, image and media menus, and their keyboard shortcuts
(`commands` in the manifest, e.g. Alt+Shift+E) work in every window.

### A test extension that exercises all of it

```sh
$LB --load-extension $CGX/Integration/ladybird/demo/toolbar-extension https://example.com/
```

- the toolbar button's badge shows the number of open tabs (`tabs.query`, `tabs.onCreated/onRemoved`, `setBadgeText`);
- its popup lists the window's tabs and has buttons for `scripting.executeScript`, `tabs.create`,
  `runtime.openOptionsPage`, messaging the service worker, and setting the badge;
- right-click a page for "Outline this page", select text for "Log … in the popup history";
- Alt+Shift+M outlines the page from the service worker (`commands.onCommand` + `scripting.executeScript`).

### Seeing what an extension does

```sh
WEBEXT_TRACE=1 $LB --load-extension ...
```

prints every extension API call with its arguments and answer, and every message and event delivered to an extension
context. When an extension misbehaves, the first `FAILED` or the last call before it stops usually names the cause.

## What to expect

These work, tested in this build:

- installing from the Chrome Web Store and addons.mozilla.org, and `--load-extension`
- extension pages, background pages and scripts, service workers (run as hidden pages)
- content scripts at all three stages; `chrome.*` only for Chrome extensions, `browser.*` too for Firefox ones
- `storage`, `i18n`, `alarms`, `permissions`, messaging and ports
- `tabs`, `windows`, `action`/`browserAction`/`pageAction`, `contextMenus`/`menus`, `commands`, `scripting`,
  Firefox's `tabs.executeScript`, `runtime.openOptionsPage`, `management.getSelf`
- popups open once their page has loaded (as in Chrome), sized to their content
- the popups of uBlock Origin, Dark Reader, SponsorBlock, Ambient light for YouTube, AnyDoc Translator and Surfingkeys;
  see [COMPATIBILITY.md](COMPATIBILITY.md) for what each of them does and does not do yet

These do not work yet:

- `webRequest`, `declarativeNetRequest`, `webNavigation`, `cookies`: ad blockers such as uBlock Origin open their popup but
  block nothing ("0 blocked").
- Content scripts share the page's JavaScript world (no isolated worlds). Extensions whose content scripts `import()`
  modules that use `chrome.*`, such as Ambient light for YouTube, do not work, and a hostile page can tamper with content
  scripts. Try extensions on sites you trust.
- Dark Reader shows its popup but does not darken pages yet.
- No extensions manager page; remove extensions from the extensions panel. They cannot be disabled, only removed.
- Signatures of `.crx`/`.xpi` files are not verified; store installs are checked against the id or hash the store gave.

The plan for everything that is missing: [specs/webextensions-full-api/roadmap.md](specs/webextensions-full-api/roadmap.md).

Unimplemented APIs fail with "<name> is not implemented yet in Ladybird" in the terminal.

## Rebuilding after changes

```sh
cd /home/cstralpt/cgx-translation-research/ladybird
ninja -C Build/release -j8 ladybird
```

Let builds finish. Interrupting `ninja` can damage its dependency log (`ninja: warning: premature end of file;
recovering`), after which every build recompiles almost everything. Repair it with
`ninja -C Build/release -t recompact` and let one build complete; after that a build with no changes says "no work to
do" in under a second.

To put the integration on another Ladybird checkout, see [Integration/README.md](../Integration/README.md).
