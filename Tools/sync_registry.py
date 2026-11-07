#!/usr/bin/env python3
"""Merges browser availability from MDN's browser-compat-data into Data/api-support.json.

What it changes:
  * adds namespaces and members BCD lists that the registry lacks, with status "planned";
  * makes each member's Chrome/Firefox availability match BCD (recorded as the namespace's `engines` plus per-member `only`).
What it never touches: the implementation status of existing members, `mv`, `perm_overrides`, `notes`, `permission`, and
members BCD does not list (BCD omits many Chrome-only APIs). The `storage` namespace is modelled by hand.

  Tools/sync_registry.py [--write] [--refresh]     (default: show what would change)
"""
import argparse
import copy
import json
import os
import re
import sys

import bcd

SKIP_NAMESPACES = {"storage"}
STATUSES = ["implemented", "partial", "delegated", "planned", "unsupported"]


def flags(d, member):
    restrict = d.get("only", {}).get(member)
    return ("chrome" in d["engines"] and restrict != "firefox", "firefox" in d["engines"] and restrict != "chrome")


def members_of(d):
    """{member: (kind, status)} for a namespace dict."""
    out = {}
    for kind in ("functions", "events", "properties"):
        for status, lst in d.get(kind, {}).items():
            for m in lst:
                out[m] = (kind, status)
    return out


def sync(reg, data):
    reg = copy.deepcopy(reg)
    log = {"namespaces_added": [], "members_added": [], "availability_fixed": []}
    for ns, members in data.items():
        if ns in SKIP_NAMESPACES:
            continue
        useful = {m: v for m, v in members.items() if v[1] or v[2]}
        if ns not in reg["namespaces"]:
            if not useful:
                continue
            engines = [e for e, on in (("chrome", any(v[1] for v in useful.values())), ("firefox", any(v[2] for v in useful.values()))) if on]
            reg["namespaces"][ns] = {"permission": bcd.NEW_NAMESPACE_PERMISSIONS.get(ns), "engines": engines}
            log["namespaces_added"].append(ns)
        d = reg["namespaces"][ns]
        existing = members_of(d)
        avail = {m: flags(d, m) for m in existing}

        for name, (kind, chrome, firefox, dep) in useful.items():
            if name not in existing and dep:
                continue  # Do not grow the denominators with deprecated legacy APIs.
            if name not in existing:
                d.setdefault(kind, {}).setdefault("planned", []).append(name)
                existing[name] = (kind, "planned")
                avail[name] = (chrome, firefox)
                log["members_added"].append(f"{ns}.{name} ({'C' if chrome else '-'}{'F' if firefox else '-'})")
            elif avail[name] != (chrome, firefox):
                log["availability_fixed"].append(f"{ns}.{name}: {avail[name]} -> {(chrome, firefox)}")
                avail[name] = (chrome, firefox)

        # Recompute the namespace's engines and the per-member restrictions from the merged availability.
        engines = [e for i, e in enumerate(("chrome", "firefox")) if any(a[i] for a in avail.values())]
        d["engines"] = engines
        only = {}
        for m, (c, f) in avail.items():
            if (c, f) == (("chrome" in engines), ("firefox" in engines)):
                continue
            only[m] = "chrome" if c and not f else "firefox"
        if only:
            d["only"] = dict(sorted(only.items()))
        else:
            d.pop("only", None)
    return reg, log


def render(reg):
    """JSON with one namespace per block and string lists on one line, so diffs stay readable."""
    text = json.dumps(reg, indent=2, ensure_ascii=False)
    text = re.sub(r"\[\s*\]", "[]", text)
    text = re.sub(r"\[\s+((?:\"[^\"\n]*\"|-?\d+)(?:,\s+(?:\"[^\"\n]*\"|-?\d+))*)\s+\]", lambda m: "[" + re.sub(r",\s+", ", ", m.group(1)) + "]", text)
    return text + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--write", action="store_true")
    ap.add_argument("--refresh", action="store_true")
    args = ap.parse_args()
    reg = json.load(open(bcd.REGISTRY))
    new, log = sync(reg, bcd.load_bcd(refresh=args.refresh))
    for key, items in log.items():
        print(f"{key}: {len(items)}")
        for i in items:
            print(f"  {i}")
    if args.write:
        open(bcd.REGISTRY, "w").write(render(new))
        print("wrote", os.path.relpath(bcd.REGISTRY))
    elif any(log.values()):
        print("\n(dry run; pass --write to apply)")
    sys.exit(0)


if __name__ == "__main__":
    main()
