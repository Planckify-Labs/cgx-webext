"""Shared access to MDN's browser-compat-data (BCD) for the WebExtensions API."""
import json
import os
import urllib.request

RAW = "https://raw.githubusercontent.com/mdn/browser-compat-data/main/webextensions/api/{}.json"
LISTING = "https://api.github.com/repos/mdn/browser-compat-data/contents/webextensions/api"
HERE = os.path.dirname(os.path.abspath(__file__))
REGISTRY = os.path.join(HERE, "..", "Data", "api-support.json")
DEFAULT_CACHE = os.path.join(HERE, ".bcd-cache")

# BCD file name -> registry namespace. BCD documents some APIs under two names; availability is the union of both.
ALIASES = {"menus": "contextMenus", "browserAction": "action"}
# Files that only describe types, not callable APIs.
TYPE_ONLY = {"events", "extensionTypes", "types", "contentScriptGlobalScope", "userScripts_legacy"}
# Namespaces whose members are objects (e.g. BrowserSetting) rather than functions.
OBJECT_NAMESPACES = {"browserSettings", "privacy", "contentSettings", "fontSettings"}
OBJECT_MEMBERS = {("proxy", "settings"), ("devtools", "inspectedWindow"), ("devtools", "network"), ("devtools", "panels")}
# Legacy APIs that new extensions should not use and that the registry deliberately leaves out.
LEGACY = {
    "extension": {"getExtensionTabs", "onRequest", "onRequestExternal", "sendRequest"},
    "tabs": {"getAllInWindow", "getSelected", "sendRequest", "onActiveChanged", "onHighlightChanged", "onSelectionChanged"},
}
# Permissions for namespaces BCD knows but the registry did not.
NEW_NAMESPACE_PERMISSIONS = {"clipboard": "clipboardWrite", "pkcs11": "pkcs11"}


def fetch(url, cache_dir=DEFAULT_CACHE, refresh=False):
    path = os.path.join(cache_dir, url.rsplit("/", 1)[-1]) if cache_dir else None
    if path and os.path.exists(path) and not refresh:
        return open(path, "rb").read()
    req = urllib.request.Request(url, headers={"User-Agent": "cgx-webext-registry-tools"})
    data = urllib.request.urlopen(req, timeout=60).read()
    if path:
        os.makedirs(cache_dir, exist_ok=True)
        open(path, "wb").write(data)
    return data


def supported(compat, browser):
    """True if the browser ships the feature (ignoring flag-only support and removals)."""
    entry = (compat or {}).get("support", {}).get(browser)
    if entry is None:
        return False
    for e in entry if isinstance(entry, list) else [entry]:
        if e.get("flags") or e.get("version_removed"):
            continue
        if e.get("version_added") not in (None, False, "preview"):
            return True
    return False


def deprecated(compat):
    return bool((compat or {}).get("status", {}).get("deprecated"))


def kind_of(ns, name):
    if name.startswith("on") and name[2:3].isupper():
        return "events"
    if name.isupper() or ns in OBJECT_NAMESPACES or (ns, name) in OBJECT_MEMBERS:
        return "properties"
    return "functions"


def load_bcd(cache_dir=DEFAULT_CACHE, refresh=False):
    """Returns { registry_namespace: { member: (kind, in_chrome, in_firefox, deprecated) } } with aliased files merged."""
    listing = json.loads(fetch(LISTING, cache_dir, refresh))
    out = {}
    for entry in sorted(listing, key=lambda f: f["name"]):
        if not entry["name"].endswith(".json"):
            continue
        bns = entry["name"][:-5]
        if bns in TYPE_ONLY:
            continue
        ns = ALIASES.get(bns, bns)
        node = json.loads(fetch(RAW.format(bns), cache_dir, refresh))["webextensions"]["api"][bns]
        members = out.setdefault(ns, {})
        for name, child in node.items():
            if name == "__compat" or not isinstance(child, dict) or name[:1].isupper() or name in LEGACY.get(ns, ()):
                continue  # Types start with a capital letter.
            compat = child.get("__compat")
            chrome, firefox = supported(compat, "chrome"), supported(compat, "firefox")
            prev = members.get(name)
            dep = deprecated(compat)
            if prev:
                chrome, firefox, dep = chrome or prev[1], firefox or prev[2], dep and prev[3]
            members[name] = (kind_of(ns, name), chrome, firefox, dep)
    return out
