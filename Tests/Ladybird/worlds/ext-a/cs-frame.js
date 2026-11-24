// Runs in the iframe of page3.html, which removes the frame while work is still pending.
const frameReport = (c, ok, d) => fetch('/report?c=' + c + '&ok=' + (ok ? 1 : 0) + '&d=' + encodeURIComponent(String(d || '')));
frameReport('iso.frame-world', parent !== window && top !== window && self === window && typeof chrome.runtime.id === 'string', '');
window.frameGlobal = 1;
// These are still pending when the frame is removed; what they do afterwards must not crash the browser.
fetch('/slow').then(() => { document.createElement('div').setAttribute('a', 'b'); return fetch('/report?c=lifetime.late-callback-ran&ok=1'); }).catch((e) => { try { document.createElement('p'); } catch (_) {} });
Promise.resolve().then(() => new Promise((r) => setTimeout(r, 2000))).then(() => { const d = document.createElement('div'); d.textContent = 'late'; });
const timer = setInterval(() => { const n = document.createElement('i'); n.id = 'tick'; }, 100);
new MutationObserver(() => {}).observe(document, { childList: true, subtree: true });
