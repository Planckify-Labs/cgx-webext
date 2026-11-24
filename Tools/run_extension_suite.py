#!/usr/bin/env python3
"""Runs popular store extensions in Ladybird, one at a time, and records what goes wrong.

Each extension runs alone in a fresh profile (scripts/run-ladybird.sh --fresh --trace) on a private X server, on a page
that exercises it. The script reloads that page once the extension has started, opens the extension's toolbar popup, and
saves two screenshots and a summary of the extension API calls that failed and the JavaScript errors.

  Tools/run_extension_suite.py <work-dir> [name ...]

<work-dir>/exts/<name>/ holds each package; missing ones are downloaded with Tools/fetch_extension.sh. Results go to
<work-dir>/results/<name>.{log,json} and <name>-{page,popup}.png. Needs Xvfb, ImageMagick's `import`, libXtst, and a
Ladybird build with ENABLE_WEBEXT=ON. Look at the screenshots: an empty error list does not prove the popup works.
"""
import ctypes
import json
import os
import re
import signal
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
RUN = os.path.join(HERE, "..", "scripts", "run-ladybird.sh")
FETCH = os.path.join(HERE, "fetch_extension.sh")
DISPLAY = ":77"

ADS = "https://adblock-tester.com/"
YOUTUBE = "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
WIKIPEDIA = "https://en.wikipedia.org/wiki/Ladybird"
PLAIN = "https://example.com/"

# name: (Chrome Web Store id or addons.mozilla.org slug, test page)
EXTENSIONS = {
    "ubol": ("ddkjiahejlhfcafbddmgiahcphecmpfh", ADS),
    "adguard": ("bgnkhhnnamicmpeenaelnjfhikgbkllg", ADS),
    "ff-ublock": ("ublock-origin", ADS),
    "ff-adguard": ("adguard-adblocker", ADS),
    "bitwarden": ("nngceckbapebfimnlniiiahkandclblb", PLAIN),
    "ff-bitwarden": ("bitwarden-password-manager", PLAIN),
    "tampermonkey": ("dhdgffkkebhmkfjojejmpbldmpobfkfo", PLAIN),
    "ff-violentmonkey": ("violentmonkey", PLAIN),
    "stylus": ("clngdbkpkpeebahjckkjfobafhncgmne", PLAIN),
    "sponsorblock": ("mnjggcdmjocbbbhaepdhchncahnbgone", YOUTUBE),
    "ryd": ("gebbhagfogifgggkldgodflihgfeippi", YOUTUBE),
    "vdh": ("lmjnegcaeklhafolokijcfjliaokphfk", YOUTUBE),
    "onetab": ("chphlpgkkbolifaimnlloiipkdnihall", PLAIN),
    "sessionbuddy": ("edacconmaakjimmfgnblocblbcdcpbko", PLAIN),
    "readaloud": ("hdhinadidafjejdhmfkjgnolgimiaplp", WIKIPEDIA),
}
BLOCKERS = {"ubol", "adguard", "ff-ublock", "ff-adguard"}

x11 = ctypes.CDLL("libX11.so.6")
xtst = ctypes.CDLL("libXtst.so.6")
x11.XOpenDisplay.restype = ctypes.c_void_p
x11.XKeysymToKeycode.argtypes = [ctypes.c_void_p, ctypes.c_ulong]


def with_display(action):
    display = ctypes.c_void_p(x11.XOpenDisplay(DISPLAY.encode()))
    action(display)
    x11.XFlush(display)
    x11.XCloseDisplay(display)


def click(x, y):
    def act(d):
        xtst.XTestFakeMotionEvent(d, -1, x, y, 0)
        x11.XFlush(d)
        time.sleep(0.2)
        xtst.XTestFakeButtonEvent(d, 1, 1, 0)
        xtst.XTestFakeButtonEvent(d, 1, 0, 0)

    with_display(act)


def press(keysym):
    def act(d):
        code = x11.XKeysymToKeycode(d, keysym)
        xtst.XTestFakeKeyEvent(d, code, 1, 0)
        xtst.XTestFakeKeyEvent(d, code, 0, 0)

    with_display(act)


def screenshot(path):
    subprocess.run(["import", "-window", "root", path], env={**os.environ, "DISPLAY": DISPLAY}, check=False)


def package_for(work, name):
    folder = os.path.join(work, "exts", name)
    os.makedirs(folder, exist_ok=True)
    if not os.listdir(folder):
        subprocess.run([FETCH, EXTENSIONS[name][0], folder], check=False, stdout=subprocess.DEVNULL)
    files = os.listdir(folder)
    return os.path.join(folder, files[0]) if files else None


def summarize(name, page, log_text, crashed):
    failed = {}
    for m in re.finditer(r"\[webext\]\s+(\S+) FAILED: (.*)", log_text):
        detail = m.group(2)
        try:
            detail = json.loads(detail).get("message", detail)
        except ValueError:
            pass
        failed.setdefault(m.group(1), detail[:160])
    errors = sorted({re.sub(r"\s+", " ", m.group(1))[:200] for m in re.finditer(r"\(js error\)\S*\s+(.*)", log_text)})
    calls = len(re.findall(r"\[webext\] \S+ \S+ context \d+: ", log_text))
    return {"name": name, "page": page, "crashed": crashed, "api_calls": calls, "failed": failed, "js_errors": errors[:12]}


def run(work, name):
    package = package_for(work, name)
    if not package:
        print(f"{name}: could not download the package", flush=True)
        return
    page = EXTENSIONS[name][1]
    results = os.path.join(work, "results")
    os.makedirs(results, exist_ok=True)
    log_path = os.path.join(results, f"{name}.log")
    env = {k: v for k, v in os.environ.items() if k != "WAYLAND_DISPLAY"}
    env.update(DISPLAY=DISPLAY, QT_QPA_PLATFORM="xcb")
    with open(log_path, "w") as log:
        browser = subprocess.Popen([RUN, "--fresh", "--trace", "--force-cpu-painting", package, page],
                                   stdout=log, stderr=subprocess.STDOUT, env=env, start_new_session=True)
    blocker = name in BLOCKERS
    time.sleep(45 if blocker else 30)
    if browser.poll() is None:
        # Extensions often open a welcome tab: go back to the test page (the first tab) and reload it, so that blockers
        # whose filters were not ready on the first load get a second chance.
        click(230, 22)
        time.sleep(1)
        press(0xFFC2)  # F5
        time.sleep(15 if blocker else 8)
        screenshot(os.path.join(results, f"{name}-page.png"))
        click(1106, 57)  # The only extension's toolbar button, in a 1280-pixel-wide window.
        time.sleep(7)
        screenshot(os.path.join(results, f"{name}-popup.png"))
    crashed = browser.poll() is not None
    if not crashed:
        os.killpg(browser.pid, signal.SIGTERM)
        try:
            browser.wait(10)
        except subprocess.TimeoutExpired:
            os.killpg(browser.pid, signal.SIGKILL)
    result = summarize(name, page, open(log_path, errors="replace").read(), crashed)
    json.dump(result, open(os.path.join(results, f"{name}.json"), "w"), indent=1)
    print(f"{name}: calls={result['api_calls']} failed={len(result['failed'])} js_errors={len(result['js_errors'])} crashed={crashed}", flush=True)


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    work = os.path.abspath(sys.argv[1])
    names = sys.argv[2:] or list(EXTENSIONS)
    unknown = [n for n in names if n not in EXTENSIONS]
    if unknown:
        sys.exit(f"unknown extensions: {', '.join(unknown)} (known: {', '.join(EXTENSIONS)})")
    xvfb = subprocess.Popen(["Xvfb", DISPLAY, "-screen", "0", "1280x1000x24"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(2)
    try:
        for name in names:
            run(work, name)
    finally:
        xvfb.terminate()


if __name__ == "__main__":
    main()
