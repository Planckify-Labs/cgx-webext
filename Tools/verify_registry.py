#!/usr/bin/env python3
"""Compares Data/api-support.json against MDN's browser-compat-data (BCD) for the WebExtensions API.

BCD is the one public dataset that records, per API member, whether Chrome and Firefox support it. This script reports:
  * namespaces and members BCD knows that the registry lacks (our denominators would be too small),
  * registry members BCD does not list (probably typos or wrong namespace),
  * members whose Chrome/Firefox availability differs.

It needs network access and is meant to be run by maintainers and by the scheduled CI job, never by the unit tests.

  Tools/verify_registry.py [--json] [--cache DIR] [--refresh]
"""
import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import bcd  # noqa: E402


def registry_members(reg, ns):
    d = reg["namespaces"].get(ns)
    if not d:
        return {}
    out = {}
    for section in ("functions", "events", "properties"):
        for lst in d.get(section, {}).values():
            for m in lst:
                restrict = d.get("only", {}).get(m)
                chrome = "chrome" in d["engines"] and restrict != "firefox"
                firefox = "firefox" in d["engines"] and restrict != "chrome"
                out[m] = (chrome, firefox)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--refresh", action="store_true")
    args = ap.parse_args()

    reg = json.load(open(bcd.REGISTRY))
    data = bcd.load_bcd(refresh=args.refresh)
    report = {"missing_namespaces": [], "missing_members": {}, "unknown_members": {}, "availability_mismatch": {}}
    for ns, members in data.items():
        if ns not in reg["namespaces"]:
            if any((c or f) and not d for _, c, f, d in members.values()):
                report["missing_namespaces"].append(ns)
            continue
        if ns == "storage":
            continue  # Modelled by hand (nested areas).
        mine = registry_members(reg, ns)
        for name, (kind, chrome, firefox, dep) in members.items():
            if not (chrome or firefox) or (dep and name not in mine):
                continue
            if name not in mine:
                report["missing_members"].setdefault(ns, []).append({"name": name, "kind": kind, "chrome": chrome, "firefox": firefox})
            elif mine[name] != (chrome, firefox):
                report["availability_mismatch"].setdefault(ns, []).append({"name": name, "registry": list(mine[name]), "bcd": [chrome, firefox]})
        extra = sorted(m for m in mine if m not in members and "." not in m)
        if extra:
            report["unknown_members"][ns] = extra

    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print("Namespaces BCD has that the registry lacks:", ", ".join(report["missing_namespaces"]) or "none")
        for title, key in (("Members BCD lists that the registry lacks", "missing_members"), ("Registry members BCD does not list (fine for Chrome-only APIs BCD omits)", "unknown_members"), ("Availability differs", "availability_mismatch")):
            print(f"\n{title}:")
            if not report[key]:
                print("  none")
            for ns, items in sorted(report[key].items()):
                print(f"  {ns}: {', '.join(i if isinstance(i, str) else i['name'] for i in items)}")
    # Only gaps in the registry's own data fail the check; BCD simply not listing a Chrome-only API is expected.
    problems = len(report["missing_namespaces"]) + sum(len(v) for k in ("missing_members", "availability_mismatch") for v in report[k].values())
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
