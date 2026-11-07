/*
 * webext.h - C ABI of the webext core (WebExtensions compatibility layer).
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * Link against libwebext_ffi.a (cargo build -p webext-ffi --release).
 *
 * Threading: a handle is single-threaded. Call every function, and expect every callback, on the thread that created
 * it. Strings in are (ptr, len) UTF-8 and only need to live for the call. Strings out are WebExtString and must be
 * freed with webext_string_free. Host callbacks that return a string must allocate it with webext_string_from.
 */
#ifndef WEBEXT_H
#define WEBEXT_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define WEBEXT_ABI_VERSION 1u

typedef struct WebExtHandle WebExtHandle;

typedef struct WebExtStr {
    const uint8_t* ptr;
    size_t len;
} WebExtStr;

/* Owned by the library. A null ptr means "no value". The bytes are followed by a NUL that is not counted in len,
 * so text can be used as a C string. */
typedef struct WebExtString {
    uint8_t* ptr;
    size_t len;
    size_t cap;
} WebExtString;

typedef struct WebExtHostCallbacks {
    void* user_data;
    /* (user, extension id, key) -> stored JSON, or a null string. */
    WebExtString (*read_storage)(void*, WebExtStr, WebExtStr);
    /* (user, extension id, key, json) */
    void (*write_storage)(void*, WebExtStr, WebExtStr, WebExtStr);
    int64_t (*now_ms)(void*);
    /* BCP 47 tag such as "en-US". */
    WebExtString (*ui_locale)(void*);
    /* (user, extension id, event name, JSON array of listener arguments) */
    void (*dispatch_event)(void*, WebExtStr, WebExtStr, WebExtStr);
    /* (user, context id, envelope JSON): hand the envelope to api.handleHostMessage() in that context, in order. */
    void (*deliver_to_context)(void*, uint64_t, WebExtStr);
    /* (user, tab id) -> full tabs.Tab JSON allocated with webext_string_from, or a null string for just {id}. */
    WebExtString (*describe_tab)(void*, int64_t);
    /* Implement the call (ApiCall JSON), then answer with webext_complete(handle, token, ...). */
    void (*call_browser)(void*, WebExtStr, uint64_t);
    /* (user, extension id, PermissionRequest JSON, token); answer with webext_complete_permission. */
    void (*request_permissions)(void*, WebExtStr, WebExtStr, uint64_t);
    /* The earliest pending alarm is due at this time; call webext_tick then. */
    void (*schedule_alarm_check)(void*, int64_t);
} WebExtHostCallbacks;

/* Receives the outcome of webext_dispatch: ok selects a result JSON value or an error object {kind, message}. */
typedef void (*WebExtReplyFn)(void* user, bool ok, WebExtStr json);

uint32_t webext_abi_version(void);

WebExtString webext_string_from(const uint8_t* ptr, size_t len);
void webext_string_free(WebExtString s);

WebExtHandle* webext_create(const WebExtHostCallbacks* callbacks);
void webext_destroy(WebExtHandle* h);

/*
 * Validate and install a .crx/.xpi/.zip. Returns JSON:
 *   {"ok":true,"id","displayName","version","manifestVersion","idSource","warnings":[...]}
 *   {"ok":false,"errors":[{"field","message"}]}
 * id_override may be empty when the package carries an ID. base_url ends with '/', or is "auto" to let the library pick chrome-extension://<id>/ or
 * moz-extension://<uuid>/ (the result's "baseUrl" tells which).
 */
WebExtString webext_install_package(WebExtHandle* h, const uint8_t* data, size_t len, WebExtStr id_override, WebExtStr base_url);
/* Install an unpacked extension from a directory ("Load unpacked", --load-extension). base_url may be "auto". */
WebExtString webext_install_directory(WebExtHandle* h, WebExtStr path, WebExtStr id_override, WebExtStr base_url);
bool webext_uninstall(WebExtHandle* h, WebExtStr id);

/*
 * Answer a request for an extension URL. initiator: serialized origin of the requester ("https://example.com",
 * "chrome-extension://<id>", "null"), or empty for a navigation the browser started itself. Returns
 *   {"status":200|403|404,"mime":"...","csp":"..."|null,"extensionId":"..."}
 * and writes the body to *body_out; returns a null string if the URL belongs to no installed extension.
 */
WebExtString webext_serve(WebExtHandle* h, WebExtStr url, WebExtStr initiator, WebExtString* body_out);
/* Content scripts of all installed extensions matching a document, sources inlined:
 *   [{"extensionId","runAt","world","js":[{"url","source"}],"css":["source"]}]   (never a null string) */
WebExtString webext_content_script_bundles(WebExtHandle* h, WebExtStr url, bool is_top_frame, WebExtStr fallback_url);
/* Per-extension content-script programs for a document: [{"extensionId","program"}]. Each program is a JS function
 * expression to evaluate in the page and call with that extension's bridge. Never a null string. */
WebExtString webext_content_script_programs(WebExtHandle* h, WebExtStr url, bool is_top_frame, WebExtStr fallback_url);
/* Where to load the extension's background context from (its page, or a generated one), or a null string. */
WebExtString webext_background_url(WebExtHandle* h, WebExtStr id);
/* Is the document at url the extension's background context (rather than a popup, options page or tab)? */
bool webext_is_background_url(WebExtHandle* h, WebExtStr id, WebExtStr url);
/* The extension id that owns an extension URL, or a null string. */
WebExtString webext_extension_for_url(WebExtHandle* h, WebExtStr url);

/* JSON for Shim/webext-shim.js; null string if unknown. */
WebExtString webext_init_data(WebExtHandle* h, WebExtStr id);

/*
 * Register a place extension code runs, before any call is made from it. JSON:
 *   {"id":7,"extensionId":"...","kind":"background|extension_page|content_script|other","tabId":5,"frameId":0,"url":"..."}
 * Returns false on bad JSON, an unknown extension or a taken id.
 */
bool webext_context_created(WebExtHandle* h, WebExtStr json);
/* The context is gone: its ports close and messages waiting on it settle. Harmless for unknown ids. */
void webext_context_destroyed(WebExtHandle* h, uint64_t context_id);
/* webext_init_data for one registered context (adds contextId/contextKind; trims the API for content scripts). */
WebExtString webext_init_data_for_context(WebExtHandle* h, uint64_t context_id);

/* What the browser needs to host the extension: background context, CSP, action, options, icons, permissions.
 * JSON; null string if unknown. */
WebExtString webext_extension_info(WebExtHandle* h, WebExtStr id);

/* [{index, js, css, runAt, world, allFrames}]; null string if the extension is unknown. */
WebExtString webext_content_scripts(WebExtHandle* h, WebExtStr id, WebExtStr url, bool is_top_frame, WebExtStr fallback_url);

/* initiator_kind: 0 = the extension itself, 1 = web page URL, 2 = other extension ID. */
bool webext_is_web_accessible(WebExtHandle* h, WebExtStr id, WebExtStr path, uint32_t initiator_kind, WebExtStr initiator);
bool webext_read_file(WebExtHandle* h, WebExtStr id, WebExtStr path, WebExtString* out);

/* ApiCall JSON: {"extensionId","function","args":[...],"caller":{"context","tabId","frameId","url"}} */
void webext_dispatch(WebExtHandle* h, WebExtStr call_json, WebExtReplyFn on_reply, void* user);
/* json: result JSON when ok, otherwise a plain-text error message. */
void webext_complete(WebExtHandle* h, uint64_t token, bool ok, WebExtStr json);
void webext_complete_permission(WebExtHandle* h, uint64_t token, bool granted);

void webext_tick(WebExtHandle* h);
void webext_notify_installed(WebExtHandle* h, WebExtStr id, WebExtStr reason, WebExtStr previous_version);
void webext_notify_startup(WebExtHandle* h, WebExtStr id);

/* ---- Extension stores: the Chrome Web Store and addons.mozilla.org (see docs/STORES.md) ----
 * webext_content_script_programs also returns the store program, under the extension id "__store__", for documents on a
 * store's origin. Honour that bridge only in documents the browser itself knows to be on that origin. */

/* "chrome" for https://chromewebstore.google.com pages, "firefox" for https://addons.mozilla.org pages, else null. */
WebExtString webext_store_for_url(WebExtStr url);
/* chrome.management.ExtensionInfo JSON: an array of all installed extensions when id is empty, else one (null if absent). */
WebExtString webext_store_management_info(WebExtHandle* h, WebExtStr id);
/* addons.mozilla.org Addon JSON {id, version, type, name, description, isEnabled, isActive, canUninstall}, or null. */
WebExtString webext_store_amo_addon(WebExtHandle* h, WebExtStr id);
/* What to tell the user before an install: {"ok":true,"name","version","warnings":[...]} or {"ok":false,"error"}.
 * With an empty manifest_json, the warnings of the installed extension id. */
WebExtString webext_store_preview_manifest(WebExtHandle* h, WebExtStr manifest_json, WebExtStr localized_name, WebExtStr id);
/* Check a downloaded package against what the store asked for (empty = no expectation): the extension id, the
 * "sha256:<hex>" file hash, the manifest the user approved (the package may not ask for more).
 * {"ok":true,"id","name","version","warnings":[...]} or {"ok":false,"error"}. Never null. */
WebExtString webext_store_check_package(const uint8_t* data, size_t len, WebExtStr expected_id, WebExtStr expected_hash, WebExtStr approved_manifest);
/* The Chrome Web Store URL that redirects to an extension's .crx, or null for an invalid id. */
WebExtString webext_store_chrome_download_url(WebExtStr id, WebExtStr chrome_version);

#ifdef __cplusplus
}
#endif

#endif /* WEBEXT_H */
