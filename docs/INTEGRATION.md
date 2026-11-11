# Integration guide: what a browser must build

This is the contract between the core and an embedding browser, and an honest list of the engine work that remains. File
references are to Ladybird at the commit in `Integration/ladybird/SUPPORTED_COMMIT`; they show where each piece would
attach, not that anything there has been changed.

## What the core already does for you

Call these; do not reimplement them.

| You need to... | Call | When |
|---|---|---|
| Install a package | `webext_install_package` | User installs a `.crx`/`.xpi`/`.zip` |
| Know how to host it | `webext_extension_info` | After install and at startup |
| Run its background code | (info → `background`) | See item 2 |
| Pick content scripts for a page | `webext_content_scripts` | On every navigation / frame creation |
| Serve its files | `webext_read_file` | A request for `<scheme>://<id>/path` |
| Gate web-page access to those files | `webext_is_web_accessible` | When the requester is not the extension |
| Build `chrome.*`/`browser.*` | `webext_init_data` + `Shim/webext-shim.js` | When creating a context |
| Run an API call | `webext_dispatch` | Each call from a context |
| Fire alarms | `webext_tick` | When the timer from `schedule_alarm_check` elapses |
| Tell it about lifecycle | `webext_notify_installed`, `webext_notify_startup` | Install/update, browser start |

And you provide the callbacks in `WebExtHostCallbacks`: persistence (`read_storage`/`write_storage`), time, locale, event
delivery (`dispatch_event`), the delegated browser calls (`call_browser` → later `webext_complete`), permission prompts
(`request_permissions` → later `webext_complete_permission`), and a timer hint (`schedule_alarm_check`).

## What the browser has to build

Ordered roughly by when you need it. "Core support" says what already exists for it.

### 1. Extension origin and resource serving
Register a scheme (`chrome-extension://<id>/`, or `moz-extension://<random-uuid>/` as Firefox does so that pages cannot
probe for installed extensions by ID). Serve files with `webext_read_file`; for requests whose initiator is a web page, require
`webext_is_web_accessible`. Apply `csp.extensionPages` from `webext_extension_info` as the Content-Security-Policy of every
extension page. Treat each extension origin as a separate origin and site for process isolation.
*Ladybird:* the loading path is `LibWebView/NavigationLoader` and the `RequestServer` process (`Documentation/ProcessArchitecture.md`).

### 2. Background context
`info.background.kind` is `page`, `scripts`, `service_worker` or `none`. For `scripts`, generate a page that loads the scripts
in order; for `page`, load it. For `service_worker`, run the script as an event page: a hidden page at the extension origin that
loads the script (as a module if `module` is true). Genuine service-worker lifetimes (idle shutdown, wake on event) are *not*
modelled; the simple approach is to keep the context alive. Chrome's `importScripts` needs a shim in that context.
Firefox allows `scripts` and `service_worker` together; prefer `scripts`.
*Ladybird:* `HeadlessWebView` (`LibWebView/HeadlessWebView.h`) is the likely hidden-page host.

### 3. Building `chrome.*` / `browser.*` in each context
Load `Shim/webext-shim.js`, then call `createApi(init, bridge)` with the JSON from `webext_init_data` and a bridge whose
`call(name, args)` returns a Promise carrying the result of `webext_dispatch` and whose `callSync` is a synchronous call for the
functions in `init.syncFunctions` (`i18n.getMessage`, ...). Expose `api.chrome` / `api.browser` as globals of that context.
*Needs from the engine:* a way to inject the objects before the extension's own scripts run, and a synchronous renderer→Browser
IPC (`Services/WebContent/WebContentServer.ipc` has the style; this would be a new message).

### 4. Content scripts and isolated worlds  (largest engine task)
On document creation call `webext_content_scripts(id, url, is_top_frame, fallback_url)` and run the returned scripts at their
`runAt` stage. Content scripts must run in an **isolated world**: same DOM, but their own JavaScript globals, so the page cannot
see or tamper with them and they cannot clash with page variables. `world: "MAIN"` scripts run in the page's own world.
*Ladybird today:* `PageClient::run_javascript` (`Services/WebContent/PageClient.cpp`) evaluates a classic script against the
document's relevant settings object, i.e. only the page's own world. A second realm per window that shares DOM wrappers is
new engine work in `LibWeb/Bindings` and `LibWeb/HTML/Scripting` (`WindowRealm`); read `Documentation/WrapperArchitecture.md`
first. Do **not** ship extensions by running content scripts in the main world: that is neither secure nor compatible.

### 5. Messaging
`runtime.sendMessage`, `tabs.sendMessage`, `runtime.connect`, `tabs.connect` and their events are implemented by the core's
message bus (`bus.rs`): it builds `sender`, finds the receiving contexts (by tab and frame for `tabs.*`), and settles replies
and ports. The browser only registers each context (`context_created` with its tab and frame) and delivers the envelopes the
core produces (`deliver_to_context`), in order, to that context's `api.handleHostMessage`.

### 6. Browser-UI APIs: report state, run primitives

The browser does **not** implement `tabs.query`, `windows.getAll`, `tabs.onUpdated`, `action.setBadgeText`,
`contextMenus.create` or `scripting.executeScript` itself. The core implements them, with each browser's argument handling,
permission trimming and error messages. The browser has three duties:

1. **Report its windows and tabs.** Call `webext_host_event(h, "browserState", snapshot)` whenever a window or tab changes
   (comparing a small snapshot a few times a second is enough; Ladybird does that), and before answering a primitive that
   changed something. The snapshot is `{windows:[{id, type, state, focused, incognito, left, top, width, height, tabs:[{id,
   active, pinned, url, title, favIconUrl, status, audible, muted, width, height, openerTabId, zoom}]}], lastFocusedWindowId}`.
   The core diffs consecutive snapshots and fires `tabs.onCreated/onUpdated/onActivated/onMoved/onAttached/onDetached/
   onRemoved/onHighlighted/onZoomChange` and `windows.onCreated/onRemoved/onFocusChanged/onBoundsChanged`, trimmed per
   extension (no `url`/`title` without the `tabs` permission, host permission or an `activeTab` grant). Tab ids must be the
   ids you give contexts in `context_created` (`tabId`), so that `tabs.sendMessage` reaches the tab's content scripts.
2. **Run primitives.** `call_browser` receives calls named `host.*` with one object argument, already validated:
   `host.tabs.create/update/remove/reload/navigateHistory/duplicate/move/discard/setZoom/capture/frames`,
   `host.scripting.inject`, `host.windows.create/update/remove`, `host.action.openPopup`, `host.runtime.reload`. The table
   with every argument and result is at the top of `crates/webext/src/browser_api.rs`. Calls without the `host.` prefix
   (`cookies.*`, ...) are APIs that are still delegated wholesale; answer them with "<name> is not implemented yet".
3. **Draw the extension UI and report what the user does.** `webext_host_event` with `uiState` (toolbar buttons: icon,
   title, badge text and colours, popup URL, enabled, pinned, site access), `contextMenuItems` (the extension items for a
   context menu about to open), `commands` (shortcuts to register). Report `actionClicked` (answer: a popup URL to open, or
   the core already fired `action.onClicked`), `contextMenuClicked`, `command`, `setActionPinned`. Each of these grants
   `activeTab` for the tab as the browsers do. The core asks you to redraw with the event `__ui.changed` (through
   `dispatch_event`; it is not for extension contexts).

`host.scripting.inject` asks you to evaluate `code` as a classic script of each listed frame's document, in the extension's
content-script context (create it first with `setupProgram`, a content-script program without scripts, if the document has
none), and to hand the completion value to the context's reporter; see `bridge.__cgxReport` in `crates/webext/src/serve.rs`
and `ExtensionBridge::run_injection` in Ladybird.

*Ladybird:* `UI/Qt/ExtensionsUI.cpp` (snapshot, primitives, toolbar, panel, popups, shortcuts),
`LibWebView/WebContentPage.cpp` (frames and injection across processes), `LibWebView/ViewImplementation.cpp` (extension
items in page context menus), `LibWeb/Internals/ExtensionBridge.cpp` (running injected code).

### 7. Network APIs

`declarativeNetRequest`, `webRequest` and `webNavigation` are implemented by the core (`dnr.rs`, `network.rs`). The browser:

1. **Asks which stages matter.** `webext_host_event("networkInterest")` returns `{any, stages, requestBody, navigation}`;
   the core sends the event `__network.changed` (through `dispatch_event`; not for extension contexts) when the answer may
   have changed. Report only those stages, so that a browser without interested extensions pays nothing.
2. **Reports each request** with `webext_network_event(stage, details, on_reply)` and holds it until `on_reply` runs: at
   once for rules, later when a blocking `webRequest` listener must answer. The answer is `{}` (go on), `{cancel}`,
   `{redirectUrl}`, `{requestHeaders}` (the whole new list), `{responseHeaders}` or `{authCredentials}`. The stages and
   their details are in `webext.h` and at the top of `network.rs`. `tabId` and `frameId` must be the ids used everywhere
   else (frame 0 is the top frame), decided by the browser process, not by what a renderer claims.
3. **Reports navigations** with `webext_host_event("navigation", {event, tabId, frameId, parentFrameId, url, ...})`.

*Ladybird:* the renderer asks at `beforeRequest`, `beforeSendHeaders` and `headersReceived` around the HTTP network fetch
(`http_network_fetch_with_extensions` in `LibWeb/Fetch/Fetching/Fetching.cpp`, `LibWeb/Loader/ExtensionNetwork`), through
`did_report_extension_network_stage` to the browser process, which adds the tab and frame
(`WebContentPage::did_report_extension_network_stage`) and calls the core. A redirect is an internal 307 response, so the
usual redirect handling follows it; a redirect to an extension file or a `data:` URL is answered in place. Navigation steps
come from the document's life in the renderer (`Document::make_active`, the parser's end, `LocalNavigable`). Not hooked
yet: worker requests, WebSockets, authentication challenges, the response body (`filterResponseData`).

### 8. User interface
Toolbar buttons, the extensions panel and popups are built on `uiState` (section 6). Popup pages get `isPopup` in their init
data; `Shim/boot.js` then reports the page's preferred size (`__ui.popupSize [width, height]`, which the browser clamps to
25×25..800×600) and routes `window.close()` to `__ui.closePopup`. Still to build: an extensions manager page
(enable/disable, permissions, errors, load unpacked), and a prompt for `request_permissions`. Ladybird has Qt and Android
frontends (`UI/Qt`, `UI/Android`); each needs its own.

### 9. Signatures and policy
`Package::verify_crx3` builds the exact signed bytes and checks that a signing key hashes to the declared ID; you supply the
RSA/ECDSA verification (`LibCrypto`). Mozilla `.xpi` signatures (`Package::has_mozilla_signature`) are PKCS#7 and are not
verified by the core. Decide the policy for unsigned/developer installs, and whether to fetch from the Chrome Web Store or
addons.mozilla.org (check their terms before pointing a third-party browser at them).

### 10. Persistence
Store the blobs from `write_storage` under the profile directory, one per `(extension, key)`; keep the package files for
`webext_read_file` (re-run `webext_install_package` from the stored package at startup, which also restores permissions
and storage).

## Suggested milestones

1. **Serve and install**: items 1 and 10 (+ install UI minimum). *Done when* `webext_extension_info` works for a dropped-in
   package and its popup HTML loads from the extension origin.
2. **Background + storage**: items 2, 3 and the persistence callbacks. *Done when* `Tests/Fixtures/storage-popup` counts
   alarm ticks and survives a restart.
3. **Content scripts (interim)**: item 4 with main-world only for `hello-content-script`, clearly marked insecure and off by
   default, while the isolated-world design lands upstream.
4. **Messaging and UI APIs**: items 5, 6, 8. Target: Vimium-class extensions (content scripts + messaging + tabs).
5. **Ports, commands, webNavigation** in the core, then **network APIs** (item 7).

Check progress at any time with `webext-coverage inspect` on the real extension you are targeting.

## Not verified yet

Honest limits of what has been tested so far: the C++ adapter (`Integration/ladybird/files/`) has been type-checked against
Ladybird's headers but not linked or executed; no extension has been run in a browser; the Chrome-side API availability for
APIs absent from MDN's data is hand-maintained; behaviour that differs between Chrome and Firefox in corner cases (port
handling in match patterns, glob case sensitivity) is noted in the README as assumptions.
