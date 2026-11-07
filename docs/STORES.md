# Installing from the extension stores

The Chrome Web Store (`https://chromewebstore.google.com/`) and addons.mozilla.org (`https://addons.mozilla.org/`)
install extensions through private browser APIs that their pages call. cgx-webext provides those APIs to the store
pages, so the stores' own buttons work.

| Store | Button | APIs the page uses |
|---|---|---|
| Chrome Web Store | Add to Chrome / Remove from Chrome | `chrome.webstorePrivate`, `chrome.management`, `navigator.userAgentData` |
| addons.mozilla.org | Add to Firefox / Remove | `navigator.mozAddonManager` (Firefox's `AddonManager.webidl`) |

## Pieces

- **Core** (`crates/webext/src/store.rs`): recognises store pages (`store_for_url`, exact HTTPS origins only), adds the
  store program to `content_script_programs` for them under the bridge key `__store__`, writes the permission warnings
  for the install prompt, checks downloaded packages (`check_store_package`), and describes installed extensions as
  `chrome.management.ExtensionInfo` and AMO `Addon` objects. C ABI: the `webext_store_*` functions in `webext.h`.
- **Page programs** (`Shim/store-chrome.js`, `Shim/store-firefox.js`): define the APIs in the store page, before the
  page's own scripts, and forward the calls that need the browser through the bridge. Tested by
  `Shim/test/store.test.js`.
- **Browser** (Ladybird: `Libraries/LibWebView/Extensions/StoreInstaller.*`): answers those calls, downloads, asks the
  user, installs, keeps the package, starts the extension, and tells the store pages.

## Trust

- The browser honours the `__store__` bridge only in documents whose URL and origin, as the browser process knows them,
  are a store's (`WebContentPage::extension_context_init`). Calls are re-checked against the document on every call.
- Nothing installs without the user's answer to a prompt the browser shows (not the page).
- Chrome Web Store: the browser builds the download URL itself from the extension id
  (`https://clients2.google.com/service/update2/crx?...`), requires the package to carry that id, and refuses a package
  that asks for permissions the approved manifest did not.
- addons.mozilla.org: the download must be HTTPS and match the `sha256:` hash the page gave. An `.xpi` whose manifest has
  no gecko id gets the id AMO signed it for (the common name in `META-INF/mozilla.rsa`); the signature itself is not
  verified.
- Downloads follow at most 10 redirects, HTTPS only, time out after 120 s and are capped at 256 MiB.

## Protocol between the page program and the browser

Calls are `bridge.call(name, argsJson)`; the reply is JSON text (empty = `undefined`), or an error message.

Chrome Web Store (`cws.*`):

| Call | Arguments | Reply |
|---|---|---|
| `cws.getExtensionStatus` | id, manifest | `"enabled"` if installed, else `"installable"` |
| `cws.beginInstall` | `beginInstallWithManifest3` details | Result code: `""`, `"user_cancelled"`, `"invalid_id"`, `"manifest_error"`, `"install_in_progress"` |
| `cws.completeInstall` | id | nothing, or an error (download or check failed) |
| `cws.getAll` / `cws.get` | – / id | `ExtensionInfo` array / object |
| `cws.uninstall` | id, showConfirmDialog | nothing, or `"User cancelled uninstall"` |
| `cws.setEnabled` | id, enabled | nothing (extensions are always enabled) |
| `cws.permissionWarningsByManifest` / `ById` | manifest / id | array of strings |

Events to the page: `{ "kind": "event", "name": "management.onInstalled", "args": [info] }` and
`management.onUninstalled` with `[id]`.

Everything else `webstorePrivate` has (`getFullChromeVersion`, `isInIncognitoMode`, `getMV2DeprecationStatus`, ...) is
answered in the page program with what a signed-out, unmanaged browser says.

addons.mozilla.org (`amo.*`):

| Call | Arguments | Reply |
|---|---|---|
| `amo.getAddonByID` | id | `Addon` fields or `null` |
| `amo.install` | install id, url, hash | nothing; progress arrives as messages |
| `amo.uninstall` | id | `true` / `false` |
| `amo.cancel`, `amo.setEnabled` | | nothing |

Progress of an install: `{ "kind": "install", "installId", "event", "state", "error", "progress", "maxProgress" }` with
Firefox's event names (`onDownloadStarted`, `onDownloadEnded`, `onDownloadFailed`, `onInstallStarted`,
`onInstallEnded`, `onInstallCancelled`, `onInstallFailed`), states (`STATE_DOWNLOADING`, ..., `STATE_INSTALLED`) and
errors (`ERROR_NETWORK_FAILURE`, `ERROR_INCORRECT_HASH`, `ERROR_CORRUPT_FILE`, `ERROR_INVALID_DOMAIN`).
Manager events: `{ "kind": "event", "name": "onInstalled" | "onUninstalled" | ..., "id" }`.

## Known differences from Chrome and Firefox

- The Chrome Web Store keeps showing "Switch to Chrome?". Google decides that on its server for every browser that is
  not Google Chrome; cgx-webext does not try to pass as Google Chrome. `navigator.userAgentData` names the brands
  `Chromium` and `Ladybird`.
- addons.mozilla.org gets a Firefox User-Agent string (`as_Firefox` in Ladybird's `WebCompat/addons.mozilla.org.json`).
- No updates yet: an installed extension stays at its version until it is installed again from the store.
- Extensions cannot be disabled, only removed. Incognito/private windows are not distinguished.
