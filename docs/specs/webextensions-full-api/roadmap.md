# Roadmap to 100%: every Chrome and Firefox extension API in Ladybird

State in November 2026 (`docs/COVERAGE.md`): **Chrome 38.6%** of 399 members, **Firefox 37.9%** of 417. Across both
browsers the registry has 503 members: 162 done, 8 delegated, 333 planned.

## What the coverage columns mean

- **core**: members that work now. The cgx-webext core implements them, using the browser where it must (tabs, windows,
  toolbar, menus, scripting all count here: Ladybird reports its tabs and runs the primitives, and that part is done).
- **+host**: also counts *delegated* members: the core checks permissions and passes the call to Ladybird as is, and
  Ladybird must implement it. **Ladybird implements none of the 8 delegated members yet** (`cookies.*` and
  `cookies.onChanged`, `tabs.onReplaced`, `runtime.onMessageExternal`), so today the "core" number is the real one;
  "+host" is what you get once those 8 are done.
- The denominators grow when Requirement 1.6 is met: namespaces that browsers have but the registry lacks (`debugger`,
  `declarativeContent`, `contentSettings`, `readingList`, `ttsEngine`, `accessibilityFeatures`, `printerProvider`,
  `webAuthenticationProxy`, ...) still have to be added. Expect roughly 450 Chrome and 450 Firefox members in the end.

## Kinds of work

- **C**: core only (Rust, tested with the fake browser). Days.
- **H**: core plus a host primitive in Ladybird's UI process (`host.*`, like `host.tabs.create`). Days to a week each.
- **E**: engine work in LibWeb/LibJS/RequestServer; often useful upstream without extensions. Weeks each.

## Phase 1: engine foundations (makes what exists work on real sites)

These are not API members but block extensions that already "have" their APIs.

| Work | Kind | Unblocks |
|---|---|---|
| Isolated worlds for content scripts, including modules they `import()` (Req 2.5) | E | Ambient Light (its module needs `chrome` in the content-script world), security of every content script, pages with Trusted Types or strict CSP without the current exemptions |
| Content-script `fetch`/XHR without page CORS for hosts the extension may access (Req 2.6, 20.4) | E | translators, Dark Reader's cross-origin stylesheet reads |
| Extension pages inside iframes of web pages get their context reliably | E | Surfingkeys' in-page panel |
| Service workers as real worker scopes, idle shutdown (Req 2.3, 13.5) | E | MV3 extensions that test `self`/`clients`, memory with many extensions |
| Dark Reader's dark-theme detector misfires on Ladybird (web-platform difference, not yet identified) | E | Dark Reader |

## Phase 2: network (ad blockers and anything that watches navigation)

| Namespace | Members | Kind | Unblocks |
|---|---:|---|---|
| `webNavigation` | 11 | H+E (navigation events from the canonical navigables; frames already enumerated) | uBlock Origin, SponsorBlock, AnyDoc |
| `webRequest` incl. blocking and Firefox `filterResponseData` | 11 | E (RequestServer interception, a synchronous round trip to the extension) | uBlock Origin (Firefox), privacy and header extensions |
| `declarativeNetRequest` | 14 | C (rule matching in the core) + E (apply in RequestServer) | uBlock Origin Lite and every MV3 Chrome blocker |
| `proxy`, `dns` | 4 | E | proxy switchers |

## Phase 3: browser data

| Namespace | Members | Kind | Notes |
|---|---:|---|---|
| `cookies` | 7 | H | against `LibWebView/CookieJar`; the 6 delegated members. AnyDoc needs it |
| `history`, `topSites` | 10 | H | Ladybird has history for autocomplete |
| `bookmarks` | 18 | H | `LibWebView/BookmarkStore` exists |
| `downloads` | 19 | H | `WebView::FileDownloader` exists |
| `sessions` | 12 | H | `SessionStore` exists (recently closed tabs) |
| `browsingData` | 11 | H | per data type Ladybird stores |
| `contextualIdentities` | 11 | E | needs containers in Ladybird first (Req 25) |

## Phase 4: browser UI surfaces

| Namespace | Members | Kind | Notes |
|---|---:|---|---|
| `notifications` | 10 | H | system notifications through Qt |
| `omnibox` | 6 | H | keyword in the address bar |
| `sidePanel` (Chrome), `sidebarAction` (Firefox) | 16 | H | a docked view hosting an extension page |
| `tabGroups`, `tabs.group/ungroup` | 10 | H+E | Ladybird has no tab groups yet |
| `theme` (Firefox) and theme packages | 4 | H | Qt palette from the theme |
| `devtools.*`, `devtools_page` | 7 | E | needs Ladybird's devtools to host extension panels |
| `find`, `search` | 6 | H | find-in-page and search engines exist |
| rest of `contextMenus` (`onShown`, `onHidden`, `overrideContext`, `getTargetElement`), `commands.openShortcutSettings` | 5 | H | |
| `management` rest (enable/disable, uninstall, events) and an `about:extensions` page | 10 | H | Req 12 |
| `permissions.request` prompt UI | 0 (counted done) | H | today optional permissions are refused |

## Phase 5: the remaining runtime, system and platform APIs

| Namespace | Members | Kind | Notes |
|---|---:|---|---|
| `runtime` rest: native messaging, `onMessageExternal`/`onConnectExternal`, `getBackgroundPage`, update events, user-script messaging | 18 | H/E | native messaging needs host manifests as in Chrome/Firefox |
| `extension` rest: `getBackgroundPage`, `getViews` | 4 | E | needs same-process extension pages (window objects across views) |
| `i18n.detectLanguage`, `getPreferredSystemLanguages` | 2 | C | |
| `storage` rest: `setAccessLevel`, `managed` (policy) | 5 | C+H | Req 23 for policy files |
| `tabs` rest: `print`, `saveAsPDF`, `captureTab`, `hide`/`show`, `warmup`, zoom settings, reader mode, `moveInSuccession` | 12 | H | |
| `userScripts` | 8 | C+E | worlds again |
| `offscreen` | 3 | H | a hidden extension document |
| `idle`, `power`, `system.*` | 9 | H | |
| `tts` | 6 | H | speech through the OS |
| `fontSettings`, `browserSettings`, `privacy` | 41 | H | map to Ladybird settings; many members are small |
| `tabCapture`, `desktopCapture`, `pageCapture` | 6 | E | media capture |
| `identity` | 6 | H | `launchWebAuthFlow` in a browser window; `getAuthToken` per Req 22 |
| `gcm` | 3 | E | push service (Req 22.5) |
| `pkcs11` | 4 | E | TLS client certificates |
| `publicSuffix`, `dom.openOrClosedShadowRoot`, `clipboard.setImageData`, `captivePortal` | 10 | C/E | |

## Phase 6: completeness and proof

- Add the missing namespaces to the registry and a check that fails when Chrome or Firefox add members (Req 1.6).
- Members only ChromeOS or Thunderbird offer stay `undefined`, as on desktop (Req 24).
- A headless Ladybird test per host primitive (Req 16), and the reference corpus of the top 50 + 50 store extensions with
  pass/fail scenarios (Req 1).

## Order recommended for the extensions in use now

1. Isolated worlds (Ambient Light, safety of everything).
2. `webNavigation` (uBlock Origin, SponsorBlock, AnyDoc).
3. `webRequest` blocking and `declarativeNetRequest` (uBlock Origin blocking ads).
4. `cookies`, `i18n.detectLanguage` (AnyDoc).
5. `bookmarks`, `history`, `downloads`, `sessions`, `tts` (Surfingkeys).
