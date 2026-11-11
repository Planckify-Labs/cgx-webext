# Tasks: complete WebExtensions API support in Ladybird

Progress against [requirements.md](requirements.md). Updated November 2026.

## Design decision: the core owns the semantics

Requirement 17.1 asks for browser-independent logic in the core. This milestone takes that further than the original plan:
the browser no longer implements `tabs.*`, `windows.*`, `action.*`, `contextMenus.*`, `commands.*` or `scripting.*`
member by member. It reports a snapshot of its windows and tabs and runs a dozen primitives (`host.*`); the core answers
every API call from the snapshot, trims `Tab` objects by permission, keeps the toolbar, menu and shortcut state, and
synthesizes the `tabs.*`/`windows.*` events by diffing snapshots. See `docs/INTEGRATION.md` section 6 and
`crates/webext/src/browser_api.rs`.

Why: about 120 API members become Rust code tested without a browser (a fake browser in the tests implements the same
primitives Ladybird does), each browser engine needs ~1,000 lines instead of several thousand, and Chrome-vs-Firefox
differences (argument shapes, `Tab` fields, error messages) live in one place.

## Done in this milestone

- [x] Core browser model: `browser.rs` (snapshot, `Tab`/`Window` objects, `tabs.query`, event diffing with Chrome's order,
      move detection, `activeTab` ending when the tab leaves the origin) and `ui.rs` (action state global and per tab,
      colours, icons from paths or `ImageData`, menu items with contexts/patterns/checkbox/radio, shortcut validation).
- [x] Req 3 Tabs: `query get getCurrent create update remove reload duplicate goBack goForward move highlight
      captureVisibleTab detectLanguage discard getZoom setZoom executeScript insertCSS removeCSS` and all events except
      `onReplaced`. `discard` answers "not implemented yet in Ladybird"; `highlight` activates the first tab (no
      multi-selection in Ladybird's tab strip); `detectLanguage` reports the document's `lang`.
- [x] Req 4 Windows: all functions and events.
- [x] Req 5.1–5.4 `scripting.*` including `registerContentScripts` with `persistAcrossSessions`; host-permission and
      `activeTab` checks with Chrome's messages; results per frame, promises awaited.
- [x] Req 6 Toolbar button: all `action`/`browserAction`/`pageAction` members, per tab, `onClicked` with `activeTab`.
- [x] Req 7 Extensions menu (Qt): puzzle button with installed count; a panel listing every extension with icon, name,
      site access, pin button and a menu (Options, Remove); pinned buttons with icon and badge; popups anchored to their
      button, sized to content within 25×25..800×600, closed by Escape, outside clicks or `window.close()`, one per window;
      live updates. Keyboard: rows and buttons are focusable with accessible names.
- [x] Req 8 Context menus: core bookkeeping, `menus` alias, `onclick`, persistence for event pages and service workers,
      items in Ladybird's page, link, image and media menus (all UIs) and in the toolbar button's menu (Qt).
- [x] Req 9.1–9.2 Commands: manifest shortcuts registered per window, `_execute_action`, `getAll`, Firefox
      `update`/`reset`, conflicts refused.
- [x] Req 13.4 (part) `runtime.reload`, `setUninstallURL`, `getPlatformInfo`, `getBrowserInfo`, `getContexts`,
      `openOptionsPage`; `management.getSelf/getAll/get`.
- [x] Req 18.1 Chrome-style extensions get `chrome` only; Firefox-style get `browser` too (pages and content scripts).
- [x] Req 2.7 Unimplemented APIs name themselves: "`cookies.get` is not implemented yet in Ladybird".

## Done: network APIs (Requirement 10)

- [x] Req 10.1 `declarativeNetRequest`: every function and `onRuleMatchedDebug`. The rule engine is in the core
      (`crates/webext/src/dnr.rs`): static rulesets from the manifest, dynamic and session rules, all actions (`block`,
      `allow`, `allowAllRequests`, `upgradeScheme`, `redirect` with `url`/`extensionPath`/`transform`/`regexSubstitution`,
      `modifyHeaders`) and conditions, priorities within and across extensions, Chrome's limits, `getMatchedRules` and the
      badge count. Dynamic rules, enabled rulesets and disabled static rules survive a restart.
- [x] Req 10.2–10.3 `webRequest`: the events with URL/type/tab filters and `extraInfoSpec`; blocking listeners (cancel,
      redirect, changed request and response headers, promises and `asyncBlocking`) for Firefox extensions and Chrome MV2.
      The listener's filter lives in the core, so a context is only woken for requests it asked for.
- [x] Req 10.4 `webNavigation`: `onBeforeNavigate`, `onCommitted`, `onDOMContentLoaded`, `onCompleted`,
      `onHistoryStateUpdated`, `onReferenceFragmentUpdated` with URL filters, `getFrame`, `getAllFrames`; frame ids are the
      ones `tabs` and `scripting` use.
- [x] Req 10.5 Matching 100,000 rules takes well under 1 ms per request (a test in `dnr.rs` holds it there).
- [x] Ladybird: fetching asks the browser process at `beforeRequest`, `beforeSendHeaders` and `headersReceived`, only for
      the stages an extension cares about (`LibWeb/Loader/ExtensionNetwork`, `http_network_fetch_with_extensions` in
      `Fetching.cpp`). An extension's redirect is an internal 307; a redirect to an extension file or a `data:` URL is
      answered in place. `scripts/ladybird-network-test.sh` checks all of it in a real build.

Not done in this area: `webRequest.filterResponseData`; `onAuthRequired`, `webNavigation.onErrorOccurred` and
`onCreatedNavigationTarget` exist but Ladybird never fires them; requests made by workers and WebSocket connections are not
reported; `onCompleted` fires when the headers arrive; `getSecurityInfo` has no certificate details (Ladybird reports none).
Req 10.6: uBlock Origin 1.76 (Firefox) and uBlock Origin Lite (Chrome) were run headless against a page that loads Google's
ad, tag-manager and publisher-tag scripts: both cancel or replace all three and leave an unrelated script alone. uBlock
Origin found two bugs, fixed: `webRequest` filters naming the extension's own files (`moz-extension://…/*`) were refused,
and reporting a navigation crashed the renderer for documents without an id (YouTube, `about:welcome`). Open: uBlock
Origin holds all requests until its lists are ready, which takes about 25 seconds on a first run in Ladybird (script
speed, not the hook: requests are answered in under 60 ms once it is ready).

## Fixed after running real extensions

- Unimplemented members and empty namespaces are `undefined`, and each extension sees only its browser's members, so
  feature detection takes the fallback (uBlock's popup was blank because `browser.publicSuffix` existed but failed).
- `contentScripts.register` (Firefox), `webRequest.handlerBehaviorChanged`, `storage.<area>.onChanged`,
  `extension.inIncognitoContext`; `'wasm-unsafe-eval'` in the default extension CSP; `abp:*`-style `targetUrlPatterns`.
- Engine: extension origins are potentially trustworthy (secure contexts, no mixed-content blocking); extension files are
  exempt from the page's CSP, may be fetched cross-origin, and script URLs into an extension pass Trusted Types (until
  isolated worlds exist). Pages where Chrome extensions run get an empty `window.chrome`, as in Chrome.
- Generated background page at the root (`_generated_background_page.html`), so relative URLs resolve; `setIcon`,
  `setPopup` and relative `tabs.create` URLs resolve against the calling page; `sender.tab.url` uses the sender's
  document when the tab's snapshot lags.
- Popups are shown once loaded (no empty flash). `WEBEXT_TRACE=1` logs extension traffic; `scripts/run-ladybird.sh`
  starts the build.

## Reference extensions

See [../../COMPATIBILITY.md](../../COMPATIBILITY.md) for the six extensions run in Ladybird.


- **Dark Reader 4.9.135 (Firefox)**: popup works and shows the site's state; `tabs.query`, `tabs.create`, messaging and
  ports work both ways; the background sends `bg-cs-add-dynamic-theme` to the page. The page is not darkened: the content
  script's dark-theme detector (`runCheck` → `hasBuiltInDarkTheme` in `inject/index.js`) reports the page as already dark
  and Dark Reader removes its theme. Checked so far: `getComputedStyle` values of example.com are right, and
  `CSSStyleSheet.disabled` is honoured. Next: log the detector's inputs while Dark Reader's own sheets are present
  (`document.styleSheets` membership of its `<style class="darkreader">` nodes, `adoptedStyleSheets`).

## Next

The full plan to 100%, by kind of work, is in [roadmap.md](roadmap.md). In the order the reference extensions need them:

1. Rest of Req 10: `filterResponseData`; requests of workers and WebSockets; `onAuthRequired`; failed navigations; uBlock
   Origin's cosmetic filtering and popup counts, checked in a window.
2. Req 2.5 isolated worlds for content scripts (security, and pages that shadow built-ins).
3. Req 2.3 service workers as real worker scopes; Req 13.5 idle shutdown.
4. Req 11 `cookies` against Ladybird's cookie jar; `browsingData`.
5. Req 12 `about:extensions` (enable/disable, permissions, errors, load unpacked); Req 15.3 permission prompt.
6. Req 14: `notifications`, `downloads`, `history`, `bookmarks`, `sessions`, `idle`, `i18n.detectLanguage`, and the rest.
7. Req 9.3 shortcut editor UI; Req 6.2 `openPopup` without a pinned button anchors to the puzzle button (done) but
   does not yet focus a minimized window.
8. Req 1 reference corpus and acceptance scenarios; Req 16 headless Ladybird tests for each host primitive (the network
   test is the first).

## Known limits of what is done

- Content scripts and injected code still share the page's JavaScript world (Req 2.5).
- `tabs.captureVisibleTab` grabs the view's widget; with GPU compositing on some platforms the capture can be blank.
- Restored tabs that have not loaded yet are not reported to extensions until they load.
- `windows.create({type: "popup"})` opens Ladybird's popup-window style; `panel` is treated as `popup`.
