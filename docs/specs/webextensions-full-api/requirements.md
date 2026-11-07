# Requirements: complete WebExtensions API support in Ladybird

## Introduction

Ladybird with cgx-webext can now install extensions from the Chrome Web Store and addons.mozilla.org, load them, and run
their background pages, extension pages and content scripts. Installed extensions still mostly do not work, because the
browser does not yet provide most of the `chrome.*` / `browser.*` APIs they call.

This feature makes installed extensions work: every API and manifest feature that real extensions use is implemented
for real, backed by Ladybird's actual tabs, windows, network stack, UI and storage. It is an implementation of the
WebExtensions platform, not an imitation. Returning fake answers so that an extension merely does not crash is out of
scope, because the extension would then silently not do its job.

### What was observed (November 2026, Ladybird `e1a46811`)

| Extension | Store | First failure | Other APIs it needs |
|---|---|---|---|
| Dark Reader 4.9.135 | addons.mozilla.org | `tabs.query` → "This browser extension API is not implemented yet"; then `actualTabs` is undefined | `tabs.sendMessage/create/update/onRemoved/executeScript`, `windows.create/update`, `contextMenus.*`, `browserAction.setIcon/setBadgeText`, `commands.*`, `theme.*`, `fontSettings.getFontList`, `dom.openOrClosedShadowRoot` |
| AnyDoc Translator 1.4.31 (MV3) | Chrome Web Store | `import` of `chrome-extension://…/assets/index.ts-….js` fails; `self.crypto.randomUUID` is undefined | `tabs.*` (12 members), `scripting.executeScript`, `action.*` (badge, icon, `openPopup`), `contextMenus.*`, `cookies.get/set`, `commands.*`, `webNavigation.getAllFrames`, `i18n.detectLanguage`, `tabs.captureVisibleTab`, `runtime.setUninstallURL`, isolated-world content scripts |

Coverage today (`docs/COVERAGE.md`): the core implements 14% of Chrome's 399 API members; with the members it routes to
the browser, 34%. The browser implements none of the routed members yet.

### Glossary

- **Core**: the cgx-webext Rust library. It decides from an extension's files and an API call alone (permissions,
  validation, storage, messaging).
- **Host**: the browser side (Ladybird). It does whatever needs the browser: tabs, windows, network, UI.
- **Delegated API**: an API the core validates and routes to the host through `call_browser`.
- **Context**: a place extension code runs: background, extension page, popup, options page, content script.
- **Isolated world**: a separate JavaScript global environment that shares the page's DOM but not its JavaScript
  objects. Chrome and Firefox run content scripts in one by default.
- **Reference corpus**: the extensions used to measure progress (Requirement 1).
- **Fully functioning**: the extension does its documented job in Ladybird the way it does in Chrome or Firefox, as
  checked by that extension's acceptance scenario.

## Requirements

### Requirement 1: Measure "fully functioning" against real extensions

**User story:** As the maintainer, I want a fixed set of real extensions with pass/fail scenarios, so that "fully
functioning" is measured and progress is visible.

#### Acceptance criteria

1. THE project SHALL define a reference corpus of at least the 50 most-used extensions of the Chrome Web Store and the
   50 most-used extensions of addons.mozilla.org, plus the extensions in the table above, plus at least one extension
   for every API namespace that none of those use. Extensions that need a Google or Mozilla account, enterprise policy
   or a smart card are included; their scenarios use test accounts and test policies.
2. THE project SHALL record, for each corpus extension, an acceptance scenario that states what the extension must do.
   Examples: "Dark Reader turns example.com dark", "uBlock Origin blocks the ads on the test page", "the translator
   translates a selected paragraph".
3. THE analyzer (`webext-coverage inspect`) SHALL report, for each corpus extension, which APIs and manifest features it
   uses that are not implemented, and the corpus summary SHALL rank missing APIs by how many corpus extensions need them.
4. Implementation order SHALL follow that ranking, except where Requirement 2 (engine foundations) blocks an API.
5. WHEN a release is made THE project SHALL publish the pass rate of the acceptance scenarios, per store.
6. THE registry (`Data/api-support.json`) SHALL list every namespace, function, event, property and type that Chrome
   (from Chromium's API schemas in `extensions/common/api` and `chrome/common/extensions/api`) or Firefox (from its
   WebExtension schemas and MDN browser-compat-data) exposes to extensions, and a check SHALL fail when either browser
   adds a member the registry lacks. Namespaces missing from the registry today include `debugger`,
   `declarativeContent`, `contentSettings`, `readingList`, `ttsEngine`, `accessibilityFeatures`, `printerProvider`,
   `webAuthenticationProxy`, `menus` and `browserAction`.
7. EVERY registry member SHALL end up implemented. The only exception is a member that desktop Chrome or desktop
   Firefox itself does not offer on the same operating system (Requirement 24); it SHALL then behave exactly as it does
   in that browser. No member SHALL remain "planned" when the feature is complete.
8. THE feature SHALL be considered complete WHEN criteria 6 and 7 hold and every corpus extension passes its acceptance
   scenario in Ladybird, or fails only for a reason outside the browser that the corpus report names (Requirement 22.4).
   Intermediate releases SHALL report the pass rate (criterion 5).

### Requirement 2: Engine foundations that extensions depend on

**User story:** As an extension, I want extension pages and workers to behave as they do in Chrome and Firefox, so that
my code loads and runs before any API is called.

#### Acceptance criteria

1. Documents and workers with a `chrome-extension://` or `moz-extension://` origin SHALL be secure contexts, so that
   `crypto.randomUUID`, `crypto.subtle`, `navigator.clipboard` and other secure-context APIs exist there.
2. Extension pages and background contexts SHALL load ES modules from their own origin: `<script type="module">`, static
   `import`, dynamic `import()`, and `"background": { "type": "module" }`.
3. WHEN a manifest declares `background.service_worker` THE host SHALL run it in a worker global scope (`self`,
   `importScripts` for classic workers, no `window` or DOM), with the extension API available. Running it as a hidden
   page is not enough, because extensions detect the difference.
4. THE host SHALL apply the extension's `content_security_policy` to extension pages, and the MV3 minimum policy
   whatever the manifest says.
5. Content scripts SHALL run in an isolated world by default (`world: "ISOLATED"`), with one world per extension per
   frame, sharing the page's DOM. `world: "MAIN"` SHALL run in the page's world.
6. Content scripts SHALL be able to `fetch` and XHR to origins the extension has host permissions for, without the page's
   CORS restrictions, as Chrome MV2 and Firefox allow.
7. WHEN extension code calls an API that is not implemented THE error message SHALL name the API, for example
   "tabs.captureVisibleTab is not implemented yet in Ladybird".

### Requirement 3: Tabs

**User story:** As an extension, I want to see and control the browser's real tabs, so that I can act on the page the
user is looking at.

#### Acceptance criteria

1. THE host SHALL implement `tabs.query`, `get`, `getCurrent`, `create`, `update`, `remove`, `reload`, `duplicate`,
   `move`, `highlight`, `discard`, `goBack`, `goForward`, `getZoom`, `setZoom`, `detectLanguage` and
   `captureVisibleTab`, against Ladybird's actual tabs.
2. `tabs.Tab` objects SHALL carry `id`, `index`, `windowId`, `active`, `highlighted`, `pinned`, `audible`, `mutedInfo`,
   `status`, `incognito`, `width`, `height`, `openerTabId`, and, only with the `tabs` permission or a host permission for
   the tab's URL, `url`, `pendingUrl`, `title` and `favIconUrl`.
3. THE host SHALL fire `tabs.onCreated`, `onUpdated` (with `changeInfo`), `onActivated`, `onHighlighted`, `onMoved`,
   `onDetached`, `onAttached`, `onRemoved`, `onReplaced` and `onZoomChange` when the matching change happens.
4. `tabs.sendMessage` and `tabs.connect` SHALL reach the content scripts of the given tab, and of the given `frameId`
   when one is given.
5. Firefox-only members (`tabs.executeScript`, `insertCSS`, `removeCSS` in MV2, `tabs.hide`/`show`, `warmup`) SHALL work
   for Firefox-style extensions.
6. Tab ids SHALL be stable for a tab's lifetime and never reused within a browser session.

### Requirement 4: Windows

**User story:** As an extension, I want to list and open browser windows, so that I can open my UI or move tabs.

#### Acceptance criteria

1. THE host SHALL implement `windows.get`, `getCurrent`, `getLastFocused`, `getAll`, `create` (including
   `type: "popup"`), `update` (focus, position, size, state) and `remove`.
2. THE host SHALL fire `windows.onCreated`, `onRemoved`, `onFocusChanged` and `onBoundsChanged`.
3. `windows.WINDOW_ID_CURRENT` and `WINDOW_ID_NONE` SHALL behave as in Chrome.

### Requirement 5: Running code in pages

**User story:** As an extension, I want to inject scripts and styles into pages on demand, so that I can act when the
user asks.

#### Acceptance criteria

1. THE host SHALL implement `scripting.executeScript` (files or `func` with `args`, `world`, `allFrames`, `frameIds`,
   `injectImmediately`), `insertCSS`, `removeCSS`, `registerContentScripts`, `getRegisteredContentScripts`,
   `updateContentScripts` and `unregisterContentScripts`.
2. `executeScript` SHALL resolve with one `InjectionResult` per frame, holding the structured-cloned return value.
3. WHEN the extension lacks host permission for the target and has no `activeTab` grant THE call SHALL fail with
   Chrome's error message.
4. Dynamically registered content scripts SHALL persist across restarts when `persistAcrossSessions` is true.
5. Firefox's `contentScripts.register` and `userScripts` SHALL be supported for Firefox-style extensions.

### Requirement 6: Toolbar button, popup and badge

**User story:** As a user, I want each extension's toolbar button with its icon, badge and popup, so that I can use the
extension as in Chrome or Firefox.

#### Acceptance criteria

1. THE Qt UI SHALL show a toolbar button for every extension with an `action`, `browser_action` or `page_action`,
   using the manifest icon at the right size for the screen.
2. THE host SHALL implement `setIcon` (paths and `ImageData`), `setTitle`, `getTitle`, `setBadgeText`, `getBadgeText`,
   `setBadgeBackgroundColor`, `setBadgeTextColor`, `setPopup`, `getPopup`, `enable`, `disable`, `isEnabledForTab`,
   `getUserSettings` and `openPopup`, both globally and per tab.
3. WHEN the user clicks the button and the extension has a popup THE UI SHALL open the popup page in a panel anchored to
   the button, sized to its content within Chrome's limits (800×600), and close it when focus leaves.
4. WHEN the extension has no popup THE host SHALL fire `action.onClicked` with the active tab and grant `activeTab` for it.
5. A clicked toolbar button, context menu item or command SHALL grant `activeTab` for the current tab until the tab
   navigates to another origin.
6. Pinned extensions SHALL show their button on the toolbar; unpinned ones SHALL be reachable from the extensions menu
   (Requirement 7).

### Requirement 7: Extensions menu and extension popups

**User story:** As a user, I want one toolbar button that opens a menu of all my installed extensions, and from it the
popup of any extension, so that I can see and use my extensions without remembering their URLs.

#### Acceptance criteria

1. THE Qt toolbar SHALL have an extensions button (a puzzle-piece icon, as in Chrome) whenever extension support is
   built in, and it SHALL show how many extensions are installed when the menu is closed.
2. WHEN the user clicks the extensions button THE UI SHALL open a menu that lists every installed extension, enabled and
   disabled, with its icon, name, badge text (if any), and whether it can read the current site ("can read and change
   this site", "on click", "cannot access").
3. EACH entry SHALL offer: open the extension (its popup, or `action.onClicked` when it has none), pin or unpin it to the
   toolbar, open its options page (when it has one), site access for the current site, "Manage extension" (its section
   of `about:extensions`), and "Remove", which asks for confirmation.
4. THE menu SHALL end with "Manage extensions" (opens `about:extensions`) and "Find more extensions" (opens the Chrome
   Web Store and addons.mozilla.org).
5. WHEN the user opens an extension from the menu or its pinned button THE UI SHALL show the extension's popup page
   (`action.default_popup`, `browser_action.default_popup`, or the per-tab popup set with `setPopup`) in a panel anchored
   to the button that opened it, as a real extension page with the full API, and the popup SHALL be interactive (clicks,
   typing, scrolling, links that open tabs).
6. THE popup panel SHALL size itself to the page's content between 25×25 and 800×600 pixels, follow resizes of the page,
   close when it loses focus, when the user presses Escape, or when the page calls `window.close()`, and only one popup
   SHALL be open per window.
7. WHEN no extension is installed THE menu SHALL say so and offer "Find more extensions".
8. THE menu and popups SHALL update without reopening when an extension is installed, removed, enabled, disabled, or
   changes its icon, title or badge.
9. THE menu and popup panel SHALL be usable with the keyboard alone and expose names to screen readers.

### Requirement 8: Context menus

**User story:** As an extension, I want to add items to the page's context menu, so that users can act on a selection,
link or image.

#### Acceptance criteria

1. THE host SHALL implement `contextMenus.create`, `update`, `remove` and `removeAll` (and Firefox's `menus.*` alias,
   including `menus.refresh` and `onShown`/`onHidden`), with all `contexts`, `documentUrlPatterns`,
   `targetUrlPatterns`, types (normal, checkbox, radio, separator) and parent items.
2. THE Qt context menu SHALL show the items that apply, grouped under the extension's name when there is more than one.
3. WHEN the user picks an item THE host SHALL fire `contextMenus.onClicked` with Chrome's `OnClickData` (selection text,
   link, src, frame, page URL, checked state) and the tab.
4. Menu items SHALL be kept across restarts for event pages and service workers, as Chrome does.

### Requirement 9: Keyboard commands

**User story:** As a user, I want extension shortcuts to work, so that I can toggle an extension from the keyboard.

#### Acceptance criteria

1. THE host SHALL register the manifest's `commands`, including `_execute_action` / `_execute_browser_action` /
   `_execute_page_action`, and fire `commands.onCommand` with the tab when the shortcut is pressed.
2. THE host SHALL implement `commands.getAll`, and Firefox's `commands.update` and `commands.reset`.
3. THE UI SHALL let the user change or clear shortcuts, and SHALL refuse a shortcut another command or the browser uses.

### Requirement 10: Network: request blocking and observing

**User story:** As a user, I want ad blockers, privacy and header-changing extensions to work, so that they can do their
job.

#### Acceptance criteria

1. THE host SHALL implement `declarativeNetRequest`: static rulesets, dynamic and session rules, all actions (`block`,
   `allow`, `allowAllRequests`, `upgradeScheme`, `redirect`, `modifyHeaders`) and conditions, with rule priorities and
   Chrome's limits, applied in the network process to every request, including subresources and workers.
2. THE host SHALL implement `webRequest` events (`onBeforeRequest`, `onBeforeSendHeaders`, `onSendHeaders`,
   `onHeadersReceived`, `onAuthRequired`, `onResponseStarted`, `onBeforeRedirect`, `onCompleted`, `onErrorOccurred`)
   with filters and `extraInfoSpec`.
3. Blocking `webRequest` (`"blocking"`) SHALL work for Firefox extensions in MV2 and MV3, and for Chrome MV2 extensions,
   including `filterResponseData` for Firefox.
4. THE host SHALL implement `webNavigation` events and `getFrame` / `getAllFrames`, with stable `frameId`s (0 for the top
   frame).
5. Blocking SHALL not slow page loads noticeably: with a 100,000-rule list, matching SHALL add under 1 ms per request at
   the 95th percentile.
6. WHEN uBlock Origin (Firefox) or uBlock Origin Lite (Chrome) is installed THE ads on the reference test page SHALL be
   blocked.

### Requirement 11: Cookies and browsing data

**User story:** As an extension, I want to read and change cookies and clear browsing data, so that session, privacy
and developer extensions work.

#### Acceptance criteria

1. THE host SHALL implement `cookies.get`, `getAll`, `set`, `remove`, `getAllCookieStores` and `onChanged` against
   Ladybird's cookie jar, honouring host permissions, partitioned cookies (`partitionKey`) and private browsing stores.
2. THE host SHALL implement `browsingData.remove*` for the data types Ladybird stores.

### Requirement 12: Extension pages and extension management UI

**User story:** As a user, I want a page that lists my extensions and lets me open their options, see their permissions
and errors, and turn them off or remove them, so that I am in control.

#### Acceptance criteria

1. THE browser SHALL provide `about:extensions` (also reached from the extensions menu, Requirement 7) listing every
   installed extension with its icon, name, version, source
   (Chrome Web Store, addons.mozilla.org, file), permissions, and errors it logged.
2. THE page SHALL let the user enable, disable, remove, reload and open the options page of each extension, set site
   access ("on click", "on specific sites", "on all sites"), allow it in private windows, and load an unpacked extension.
3. WHEN an extension is disabled THE host SHALL stop its contexts and remove its content scripts, toolbar button, menu
   items, commands and network rules, and the stores SHALL report it as disabled.
4. THE host SHALL implement `runtime.openOptionsPage` and `options_ui` (embedded or in a tab), and
   `chrome_url_overrides` (new tab, history, bookmarks).
5. THE host SHALL implement `management.getSelf`, `getAll`, `get`, `setEnabled`, `uninstall` and `uninstallSelf`, with
   their events, for extensions that have the `management` permission.

### Requirement 13: Extension lifecycle and updates

**User story:** As a user, I want extensions to stay up to date and behave the same across restarts, so that they keep
working.

#### Acceptance criteria

1. THE host SHALL check installed store extensions for updates at least once a day (Chrome's update protocol for
   `update_url`, AMO's update API for Firefox add-ons) and install newer versions after the same checks as a store
   install.
2. WHEN an update asks for new permissions THE host SHALL disable the extension until the user approves them.
3. THE host SHALL fire `runtime.onInstalled` (`install`, `update` with `previousVersion`, `browser_update`),
   `onStartup`, `onSuspend`, `onUpdateAvailable` and `onRestartRequired` at the right moments.
4. THE host SHALL implement `runtime.reload`, `setUninstallURL` (open the URL after removal), `requestUpdateCheck`,
   `getPlatformInfo`, `getBrowserInfo` (Firefox), `getContexts`, `openOptionsPage` and `restart`.
5. MV3 service workers SHALL be stopped when idle and restarted for events, as Chrome does, keeping registered listeners;
   long-lived ports and pending API calls SHALL keep them alive.

### Requirement 14: All other APIs

**User story:** As an extension, I want every remaining API of my browser, so that I work without changes.

#### Acceptance criteria

1. THE host SHALL implement, in the order Requirement 1 ranks them: `notifications`, `downloads`, `storage.sync`
   (Requirement 22), `storage.managed` (Requirement 23), `i18n.detectLanguage`, `history`, `bookmarks`,
   `sessions`, `topSites`, `idle`, `alarms` (done), `offscreen`, `sidePanel` (Chrome) and `sidebarAction` (Firefox),
   `theme` (Firefox), `search`, `tabGroups`, `fontSettings`, `dom.openOrClosedShadowRoot`, `tts`, `tabCapture`,
   `desktopCapture`, `pageCapture`, `privacy`, `proxy`, `permissions.request` with a prompt, `identity` (Requirement 22),
   `runtime.connectNative` / `sendNativeMessage` (native messaging hosts installed as in Chrome and Firefox),
   `debugger` (Chrome DevTools Protocol commands against a tab, needed by automation and accessibility extensions),
   `declarativeContent`, `contentSettings`, `readingList`, `ttsEngine`, `accessibilityFeatures`, `system.cpu` /
   `memory` / `storage` / `display`, `power`, `browsingData`, `userScripts` (Chrome), Firefox's `browserSettings`,
   `captivePortal`, `dns`, `find`, `clipboard.setImageData`, `menus.getTargetElement`, `pkcs11` (Requirement 23), and `devtools.*` (`devtools_page`, panels,
   `inspectedWindow`, `network`, `recorder`, `performance`), together with the Ladybird developer tools they extend.
2. `runtime.sendMessage` / `connect` from web pages (`externally_connectable`) and between extensions
   (`onMessageExternal`, `onConnectExternal`) SHALL work.
3. THE `browser.*` namespace SHALL return Promises for every function and the `chrome.*` namespace SHALL accept callbacks
   and set `runtime.lastError` in both browsers' styles, for every implemented API.

### Requirement 15: Permissions and security

**User story:** As a user, I want extensions limited to what I allowed, so that a bad extension cannot do more than it
says.

#### Acceptance criteria

1. EVERY delegated API SHALL be checked against the extension's granted API and host permissions in the core before the
   host runs it, and the host SHALL not trust anything in a call that came from a renderer process.
2. THE host SHALL verify CRX3 signatures (RSA and ECDSA) and Mozilla add-on signatures before installing store packages,
   and refuse unsigned packages from the stores.
3. `permissions.request` SHALL show a browser prompt and need a user gesture.
4. Content scripts in isolated worlds SHALL not expose extension APIs or the bridge to the page, and the page SHALL not be
   able to tamper with built-ins the content script uses.
5. Extension pages SHALL be unable to read other extensions' storage or files, except `web_accessible_resources` that
   allow it.

### Requirement 16: Testing and compatibility tracking

**User story:** As the maintainer, I want each API tested in isolation and in the browser, so that regressions are caught
and coverage numbers are honest.

#### Acceptance criteria

1. EVERY API the host implements SHALL have a Ladybird test (headless) that exercises it from an extension, following
   Ladybird's test conventions.
2. THE registry (`Data/api-support.json`) SHALL mark an API `implemented` only once that test exists, and
   `docs/COVERAGE.md` SHALL show host coverage separately from core coverage.
3. THE reference corpus scenarios (Requirement 1) SHALL run automatically, headless, against packages pinned by version,
   and report per-extension pass/fail.
4. WHEN Ladybird's upstream changes break the patch or a test THE weekly upstream workflow SHALL report it.

### Requirement 17: Keep the integration separable

**User story:** As the maintainer, I want this to stay an opt-in layer, so that it can be offered to Ladybird or used by
other browsers.

#### Acceptance criteria

1. Browser-independent logic (validation, permission checks, rule matching for `declarativeNetRequest`, menu and command
   bookkeeping, `Tab` object filtering by permission) SHALL live in the core and be tested there.
2. Ladybird changes SHALL stay behind `ENABLE_WEBEXT`; a build without it SHALL be identical to stock Ladybird.
3. Engine changes that are useful without extensions (isolated worlds, secure-context rules, module loading fixes) SHALL be
   written so that they can be proposed upstream separately.

### Requirement 18: Chrome extensions behave like Chrome, Firefox extensions like Firefox

**User story:** As an extension written for one browser, I want the API to behave the way my browser's does, so that I
run unchanged, including code that checks which browser it is in.

#### Acceptance criteria

1. Firefox-style extensions (gecko id, or Firefox-only manifest keys) SHALL get both `browser.*` and `chrome.*`, as in
   Firefox. Chrome-style extensions SHALL get `chrome.*` only, because extensions test `typeof browser` to decide
   whether they run in Firefox.
2. Each style SHALL see its own browser's members: Firefox-only APIs (`menus`, `theme`, `sidebarAction`,
   `tabs.executeScript` in MV3, `runtime.getBrowserInfo`, `browserSettings`, `find`, `dns`, ...) only for Firefox-style
   extensions, and Chrome-only APIs (`sidePanel`, `offscreen`, `tabGroups`, `readingList`, `declarativeContent`, ...) only
   for Chrome-style ones.
3. Messaging SHALL follow each browser: Chrome's `sendResponse` with `return true` and MV3 Promise replies, Firefox's
   Promise-returning `onMessage` listeners, and each browser's error messages ("Could not establish connection.
   Receiving end does not exist." and Firefox's equivalent).
4. Background contexts SHALL follow each browser: Chrome MV3 service workers; Chrome MV2 persistent and event pages;
   Firefox MV2 persistent pages and MV3 event pages (`background.scripts`, non-persistent), including
   `runtime.getBackgroundPage`, `extension.getBackgroundPage` and `extension.getViews`.
5. Firefox MV3 host permissions SHALL be off until the user grants them, as in Firefox, with a prompt at install that
   offers them; Chrome host permissions SHALL be granted at install.
6. `runtime.getURL`, `@@extension_id` and `__MSG_*__` substitution in CSS, and `web_accessible_resources` (MV2 list,
   MV3 objects with `matches`, `extension_ids` and `use_dynamic_url`) SHALL behave as in each browser.
7. `runtime.getBrowserInfo` (Firefox), `navigator.userAgent` and `navigator.userAgentData` in extension contexts SHALL
   report Ladybird by default. WHEN an extension refuses to run outside Chrome or Firefox THE user SHALL be able to turn
   on, for that extension only, a compatibility setting under which its own contexts and content scripts see the values
   of the browser it was written for. The corpus report SHALL list the extensions that need it.

### Requirement 19: Manifest features of both ecosystems

**User story:** As an extension, I want every manifest key Chrome and Firefox honour to work, so that what I declare
happens.

#### Acceptance criteria

1. THE host SHALL honour: `background` (all forms), `content_scripts` (all fields, including `match_about_blank`,
   `match_origin_as_fallback`, `all_frames`, `world`, `run_at`), `action`/`browser_action`/`page_action`,
   `options_page`/`options_ui`, `chrome_url_overrides`, `chrome_settings_overrides` (homepage, search provider,
   startup pages; with the user's confirmation), `omnibox` (keyword in the address bar, `omnibox.*` events and
   suggestions), `commands`, `devtools_page`, `side_panel` and `sidebar_action`, `declarative_net_request`,
   `externally_connectable`, `web_accessible_resources`, `content_security_policy`, `incognito` (`spanning`, `split`,
   `not_allowed`), `storage.managed_schema`, `optional_permissions`, `optional_host_permissions`, `host_permissions`,
   `minimum_chrome_version` and `browser_specific_settings.gecko.strict_min_version` / `strict_max_version`,
   `protocol_handlers` (Firefox), `default_locale` and `_locales`, `icons`, `key`, `update_url`, `version_name`, and
   `cross_origin_embedder_policy` / `cross_origin_opener_policy`.
2. WHEN a manifest key is not honoured THE install prompt or `about:extensions` SHALL say which one and what will not
   work.
3. Themes from both stores SHALL install through the store buttons and change the browser's look: Chrome Web Store
   themes (CRX with a `theme` key: colors, tints, images) and Firefox static themes (`theme` key), plus Firefox's dynamic
   `theme.update` / `theme.reset` / `theme.getCurrent` and `theme.onUpdated`. Removing or disabling a theme SHALL restore
   the previous look.
4. Private windows SHALL run only extensions the user allowed there, with `extension.inIncognitoContext`,
   `extension.isAllowedIncognitoAccess` and incognito cookie stores, following the manifest's `incognito` mode.
5. Access to `file://` pages SHALL be off until the user allows it per extension
   (`extension.isAllowedFileSchemeAccess`).

### Requirement 20: Web platform features extensions rely on

**User story:** As an extension, I want the ordinary web platform to be there in my pages and scripts, so that I do
not fail on something that is not an extension API.

#### Acceptance criteria

1. THE corpus run SHALL record every failure caused by a web platform feature Ladybird lacks (for example
   `navigator.clipboard`, `IndexedDB` in extension origins, dedicated and shared workers from extension URLs,
   `OffscreenCanvas`, `WebAssembly` streaming compilation, `Intl.Segmenter`, `structuredClone`, `BroadcastChannel`,
   `customElements` in content scripts), separately from extension API failures.
2. EACH recorded web platform gap SHALL be fixed in Ladybird (and proposed upstream) or have a written reason, before
   the feature is complete.
3. Extension origins SHALL get the same storage as web origins (IndexedDB, Cache API, localStorage), kept per extension
   and removed with it, and exempt from the eviction web origins are subject to when the extension has
   `unlimitedStorage`.
4. Background contexts and extension pages SHALL be allowed to `fetch` and XHR any origin the extension has host
   permissions for, without CORS, and with the extension's cookies for that site.

### Requirement 21: Many extensions at once

**User story:** As a user, I want to run all my extensions together without the browser becoming slow or unstable.

#### Acceptance criteria

1. THE browser SHALL run at least 30 extensions together (the 30 most-used of the corpus) with all their background
   contexts, and start up with them in under 1.5 times the time it takes without them.
2. A crashing or hanging extension context SHALL not take down tabs or other extensions; THE host SHALL restart it on
   its next event and show the error in `about:extensions`.
3. Content scripts, network rules and event delivery of several extensions in the same page or request SHALL not
   interfere with each other, and SHALL be applied in the order Chrome and Firefox use.


### Requirement 22: Account sign-in, push messaging and sync

**User story:** As an extension that signs users in, receives push messages or syncs settings, I want the account APIs
of my browser, so that sign-in, push and sync work.

Background: `identity.getAuthToken` is not an ordinary web flow. Google Chrome mints the token with its own signed-in
Google account and Google's private token endpoint, using OAuth credentials that only Google Chrome may use since
March 15, 2021 ("Limiting Private API availability in Chromium"). Chromium-based browsers such as Microsoft Edge, Brave
and Vivaldi therefore do not support `getAuthToken`, and extensions fall back to `launchWebAuthFlow`. `gcm` and
`instanceID` likewise use Chrome's private Google Cloud Messaging connection. Ladybird must not use Google Chrome's
private credentials.

#### Acceptance criteria

1. THE host SHALL implement `identity.launchWebAuthFlow` (interactive and non-interactive, `abortOnLoadForNonInteractive`,
   `timeoutMsForNonInteractive`) and `getRedirectURL`, intercepting Chrome's `https://<id>.chromiumapp.org/` and
   Firefox's `https://<hash>.extensions.allizom.org/` redirect URLs in a browser-owned sign-in window.
2. THE host SHALL implement `identity.getAuthToken`, `removeCachedAuthToken`, `clearAllCachedAuthTokens`, `getAccounts`
   and `getProfileUserInfo` on top of a Ladybird account manager: the user adds Google accounts once through Google's
   public OAuth 2.0 authorization flow in a browser window, and Ladybird then obtains tokens for an extension's
   manifest `oauth2.client_id` and `scopes` through that public flow, showing the consent screen when Google asks for
   it, and caches them until they expire.
3. `getAuthToken` SHALL behave like Chrome's for the extension: the same options (`interactive`, `account`, `scopes`,
   `enableGranularPermissions`), result shape (`token`, `grantedScopes`), caching and error messages.
4. WHEN Google refuses to issue a token for an extension's client to a browser other than Google Chrome THE call SHALL
   fail with Chrome's error message, and the corpus report SHALL record that the extension's OAuth client blocks
   Ladybird, so that the extension's developer can register a web client, as they must for Edge and Brave.
5. THE host SHALL implement `gcm` and `instanceID` with a push service that delivers messages from extension servers to
   the extension, including while no extension page is open. WHEN Google does not grant Ladybird its own Google Cloud
   Messaging credentials THE host SHALL deliver through the Web Push protocol (RFC 8030 and RFC 8291), and `gcm.register`
   SHALL return Chrome's error for servers that only speak Google's protocol.
6. `storage.sync` SHALL keep Chrome's and Firefox's quotas and `onChanged` events and be stored locally, and THE browser
   SHALL offer optional syncing of extension data between the user's own Ladybird installs through a sync service the
   user chooses (self-hosted or Ladybird's), end-to-end encrypted.
7. Firefox's `identity.getRedirectURL` and Mozilla-account features SHALL behave as in Firefox for extensions that use
   Mozilla accounts, through the same account manager.

### Requirement 23: Policy, enterprise and certificate APIs

**User story:** As an administrator or a user with a smart card, I want policy-managed extensions and certificate APIs
to work, so that managed and enterprise extensions run.

#### Acceptance criteria

1. THE browser SHALL read extension policy from the files Chrome and Firefox use on each platform (Chrome's JSON
   policies, Firefox's `policies.json`): force-installed, blocked and allowed extensions, install sources, runtime host
   restrictions, and per-extension `3rdparty` policy, which SHALL be returned by `storage.managed` and validated against
   the extension's `storage.managed_schema`.
2. Force-installed extensions SHALL install and update from their `update_url` without a prompt and SHALL not be
   removable by the user; the stores SHALL report them as `force_installed`.
3. THE host SHALL implement `enterprise.platformKeys` and `platformKeys` (generate keys, sign, list and select client
   certificates, import certificates) against the operating system's certificate and key store, and Firefox's `pkcs11`
   (register PKCS#11 modules) together with client-certificate support in Ladybird's TLS.
4. THE host SHALL implement `enterprise.deviceAttributes`, `enterprise.hardwarePlatform` and
   `enterprise.networkingAttributes` exactly as desktop Chrome does on the same operating system, where they are only
   available to force-installed extensions.

### Requirement 24: Members the desktop browsers do not offer

**User story:** As an extension, I want APIs that my browser does not offer on this platform to be absent in the same
way, so that my feature detection takes the same branch it takes in Chrome or Firefox.

#### Acceptance criteria

1. Members that desktop Chrome offers only on ChromeOS (`fileSystemProvider`, `input.ime`, `wallpaper`, `vpnProvider`,
   `documentScan`, `printing`, `printingMetrics`, `fileBrowserHandler`, `loginState`, `certificateProvider`, ...) SHALL be
   `undefined` with the same permission warnings and install-time messages as desktop Chrome on that operating system.
2. Chrome Apps, hosted apps and NaCl manifests SHALL be refused with the message current desktop Chrome shows, and
   Thunderbird-only APIs (`messageDisplayScripts`, `compose`, ...) SHALL be absent as in desktop Firefox.
3. THE registry SHALL record, per member, the platforms on which each browser offers it, and the conformance tests
   SHALL check that Ladybird matches on Linux, macOS and Windows.

### Requirement 25: Containers (Firefox contextual identities)

**User story:** As a user of container extensions, I want tabs in separate containers, so that Multi-Account Containers
and similar extensions work.

#### Acceptance criteria

1. Ladybird SHALL support containers: tabs with a container have their own cookies, storage, cache and HTTP state, a
   color and icon in the tab strip, and the address bar shows the container.
2. THE host SHALL implement `contextualIdentities.create`, `get`, `query`, `update`, `remove`, `move` and their events,
   `tabs.create({ cookieStoreId })`, `cookieStoreId` on `tabs.Tab`, and container cookie stores in `cookies.*`.

### Requirement 26: Dictionaries and language packs from addons.mozilla.org

**User story:** As a user, I want dictionaries and language packs from addons.mozilla.org to work, so that I can spell
check and use the browser in my language.

#### Acceptance criteria

1. Ladybird SHALL have spell checking in editable fields with a context-menu correction list, and dictionary add-ons
   (`dictionaries` manifest key, Hunspell `.dic`/`.aff`) installed from addons.mozilla.org SHALL add their language to it.
2. Language pack add-ons (`langpack_id`) SHALL install, add their locale to the UI languages the user can pick and to
   `i18n.getAcceptLanguages`, and translate the Ladybird UI strings for which Ladybird has a catalog for that locale.

### Requirement 27: Shared modules

**User story:** As an extension that depends on a shared module, I want the module installed with me, so that I can
load its files.

#### Acceptance criteria

1. WHEN an extension's manifest has `import` entries THE host SHALL install the shared modules (manifest `export`) from
   the same store, check `minimum_version`, and serve their files under `_modules/<id>/`.
2. Shared modules SHALL be removed when no installed extension imports them any more, and their `export.allowlist`
   SHALL be enforced.

### Requirement 28: Extensions on Ladybird for Android

**User story:** As a user of Ladybird on Android, I want the extensions Firefox for Android supports, so that my
extensions also run on my phone.

#### Acceptance criteria

1. Ladybird for Android SHALL install extensions from addons.mozilla.org and the Chrome Web Store with the same checks
   and prompts as the desktop.
2. THE Android UI SHALL provide the extensions menu, toolbar actions and popups (Requirements 6 and 7) in the form
   Firefox for Android uses (a menu entry per extension, popups as bottom sheets), and the APIs SHALL behave as in
   Firefox for Android where it differs from the desktop.


### Requirement 29: No "Switch to Chrome?" notice on the Chrome Web Store

**User story:** As a user, I want the Chrome Web Store to look the way it does in Chrome, without the "Switch to
Chrome?" dialog and the "Switch to Chrome to install extensions and themes" banner, because Ladybird installs Chrome
extensions.

Background (measured November 2026 on the Dark Reader page of the store):

- The store's script shows the notice unless its browser check passes. When `webstorePrivate.getFullChromeVersion`
  reports 142.0.7430.0 or newer and the store's feature flag is on, the check returns a value the server writes into
  the page (`IJ_values[24]`). For older versions it checks the User-Agent: its product tokens must be exactly
  `Mozilla AppleWebKit Chrome Safari`.
- A correct Chrome User-Agent alone does not remove the notice. With Ladybird's "Chrome Linux Desktop" preset,
  `navigator.userAgentData` and `Sec-CH-UA` client hints, the server wrote the same value as for any other browser, and
  the notice still showed.
- Google Chrome also sends `X-Browser-Channel`, `X-Browser-Year`, `X-Browser-Validation` and `X-Client-Data` headers to
  Google sites. `X-Browser-Validation` is computed with a key built into Google Chrome so that Google can tell genuine
  Chrome apart; it is the likely input to the server's value.

#### Acceptance criteria

1. Ladybird SHALL implement User-Agent Client Hints for all sites (`navigator.userAgentData`, `Sec-CH-UA`,
   `Sec-CH-UA-Mobile`, `Sec-CH-UA-Platform`, and the high-entropy hints on request through `Accept-CH`), with brands
   in Chromium's format.
2. THE browser SHALL send `chromewebstore.google.com` a User-Agent string in Google Chrome's exact format for the
   platform (product tokens `Mozilla AppleWebKit Chrome Safari`, Chrome's version), through a `WebCompat` rule like the
   one for addons.mozilla.org.
3. WHEN the store still decides to show the notice THE store program SHALL hide the "Switch to Chrome?" dialog and the
   "Switch to Chrome to install extensions and themes" banner, the way a content blocker hides page elements, and SHALL
   leave every other store notice visible (for example "To add this item to Chrome, please update your browser" or
   "Item currently unavailable").
4. Hiding SHALL not break the page: the install, remove and enable buttons, and the store's layout, SHALL work as
   without it.
5. THE browser SHALL NOT forge `X-Browser-Validation` or other values derived from Google Chrome's built-in keys. No
   extension needs them, and computing them would mean passing Google Chrome's secret off as Ladybird's.
6. A headless test SHALL open a Chrome Web Store item page and check that neither notice is visible and that "Add to
   Chrome" installs. The corpus run SHALL fail it when the store changes how it shows the notice, so the hiding can be
   updated.

## Appendix: where each namespace is covered

| Namespace | Requirement |
|---|---|
| `runtime`, `extension`, `i18n`, `storage`, `alarms`, `permissions` | 2, 13, 14, 15, 18 (core already implements most) |
| `tabs` | 3 |
| `windows` | 4 |
| `scripting`, `contentScripts`, `userScripts` | 5, 14 |
| `action`, `browserAction`, `pageAction`, `declarativeContent` | 6, 7, 14 |
| `contextMenus`, `menus` | 8 |
| `commands` | 9 |
| `declarativeNetRequest`, `webRequest`, `webNavigation`, `proxy`, `dns` | 10, 14 |
| `cookies`, `browsingData` | 11, 14 |
| `management` | 12 |
| `omnibox`, `sidePanel`, `sidebarAction`, `devtools`, `theme`, `search` | 14, 19 |
| `notifications`, `downloads`, `history`, `bookmarks`, `sessions`, `topSites`, `idle`, `offscreen`, `tabGroups`, `readingList` | 14 |
| `fontSettings`, `dom`, `tts`, `ttsEngine`, `tabCapture`, `desktopCapture`, `pageCapture`, `privacy`, `contentSettings`, `accessibilityFeatures` | 14 |
| `identity`, `gcm`, `instanceID`, `storage.sync` syncing | 22 |
| `enterprise.*`, `platformKeys`, `pkcs11`, `storage.managed`, policy installs | 23 |
| native messaging, `debugger`, `system.*`, `power`, `publicSuffix` | 14 |
| `contextualIdentities` | 25 |
| Dictionaries, language packs | 26 |
| Shared modules (`import` / `export`) | 27 |
| Android | 28 |
| Chrome Web Store "Switch to Chrome?" notice, User-Agent Client Hints | 29 |
| `browserSettings`, `captivePortal`, `find`, `clipboard` (Firefox) | 14, 18 |
| ChromeOS-only APIs, Chrome Apps, NaCl, Thunderbird APIs | 24 (absent exactly as in the desktop browsers) |
