// Runs in the extension's isolated world (document_idle). Everything it reports is something the world must get right.
const report = (c, ok, d) => fetch('/report?c=' + encodeURIComponent(c) + '&ok=' + (ok ? 1 : 0) + '&d=' + encodeURIComponent(String(d === undefined ? '' : d).slice(0, 200)));
const check = async (name, body) => {
  try {
    const result = await body();
    await report(name, result === true || (result && result.ok), result && result.d);
  } catch (e) { await report(name, false, 'threw: ' + (e && e.message || e)); }
};

window.csGlobal = 1;
// Uncaught errors and unhandled rejections of the content script are the extension's business, not the page's.
setTimeout(() => { throw new Error('cs-boom'); }, 100);
Promise.reject(new Error('cs-rejected'));
const el = document.getElementById('shared');
let pingedWith = null;
document.addEventListener('cgx-ping', (e) => { pingedWith = e.detail; });

(async () => {
  await check('iso.page-global-invisible', () => typeof pageGlobal === 'undefined' && el.pageExpando === undefined);
  await check('iso.has-api-as-global', () => ({ ok: window.chrome === chrome && typeof chrome.runtime.id === 'string' && typeof browser === 'undefined' && typeof cloneInto === 'undefined' && typeof exportFunction === 'undefined' && !('wrappedJSObject' in window), d: `${typeof chrome} ${typeof browser} ${typeof cloneInto}` }));
  if (location.pathname === '/page.html') await check('iso.same-world-across-scripts', () => window.earlyGlobal === 'early');
  await check('iso.window-self-references', () => window.self === window && globalThis === window && window.window === window && top === window && parent === window && frames === window);
  await check('iso.array-push-not-tampered', () => { const a = []; return a.push(1, 2) === 2 && a.length === 2; });
  await check('iso.json-stringify-not-tampered', () => JSON.stringify({ a: 1 }) === '{"a":1}');
  await check('iso.query-selector-not-tampered', () => document.querySelector('#shared') === el && el !== null);
  let clicks = 0;
  await check('iso.add-event-listener-works', () => { el.addEventListener('click', () => { clicks++; }); el.click(); return clicks === 1; });
  await check('iso.expando-not-shared', () => { el.csExpando = 'cs'; return el.csExpando === 'cs' && el.pageExpando === undefined; });
  await check('iso.dom-changes-shared', () => { const d = document.createElement('div'); d.id = 'from-cs'; d.textContent = 'made by the content script'; document.body.appendChild(d); return document.getElementById('from-cs') === d && el.textContent === 'shared'; });
  await check('iso.instanceof-dom-classes', () => el instanceof HTMLElement && el instanceof HTMLDivElement && el instanceof Element && el instanceof Node && document.body instanceof HTMLBodyElement && document instanceof Document && window instanceof Window && new Event('x') instanceof Event);
  await check('iso.wrapper-identity', () => document.getElementById('shared') === el && document.body.firstElementChild === el && el.ownerDocument === document);
  await check('iso.settimeout', () => new Promise((r) => setTimeout(function () { r(this === window); }, 10)));
  await check('iso.setinterval', () => new Promise((r) => { let n = 0; const t = setInterval(() => { if (++n === 3) { clearInterval(t); r(true); } }, 5); }));
  await check('iso.promise', async () => { const v = await Promise.all([Promise.resolve(1), new Promise((r) => queueMicrotask(() => r(2)))]); return v[0] + v[1] === 3 && Promise.resolve(1) instanceof Promise; });
  await check('iso.fetch', async () => { const r = await fetch('/api.json'); const j = await r.json(); return j.ok === 1 && r instanceof Response; });
  await check('iso.mutation-observer', () => new Promise((r) => { const mo = new MutationObserver((records) => { mo.disconnect(); r({ ok: records.length === 1 && records[0].target === el && records[0].type === 'attributes', d: `n=${records.length} sameTarget=${records[0] && records[0].target === el} type=${records[0] && records[0].type}` }); }); mo.observe(el, { attributes: true }); el.setAttribute('data-x', '1'); }));
  await check('iso.request-animation-frame', () => new Promise((r) => { requestAnimationFrame((t) => r(typeof t === 'number')); setTimeout(() => r(false), 3000); }));
  await check('iso.custom-event-from-cs-reaches-page-listeners', () => { let got = null; const f = (e) => { got = e.detail; }; document.addEventListener('cgx-own', f); document.dispatchEvent(new CustomEvent('cgx-own', { detail: 'own' })); return got === 'own'; });
  await check('iso.dom-parser-and-html', () => { const d = document.createElement('div'); d.innerHTML = '<b id="b">x</b>'; return d.firstChild instanceof HTMLElement && d.querySelector('#b').textContent === 'x'; });
  await check('iso.eval-and-function', () => new Function('a', 'return a + 1')(1) === 2 && eval('1+1') === 2);
  await check('iso.timer-string-handler-runs-in-this-world', () => new Promise((r) => { setTimeout('window.timerStringRan = typeof csGlobal', 0); setTimeout(() => r(window.timerStringRan === 'number' && typeof window.timerStringRan === 'string'), 50); }));
  await check('iso.messaging', async () => { const reply = await chrome.runtime.sendMessage({ kind: 'ping' }); return reply === 'pong'; });
  await check('iso.storage', async () => { await chrome.storage.local.set({ k: 'v' }); return (await chrome.storage.local.get('k')).k === 'v'; });

  // Dynamic import of a web-accessible module: it runs in this world and sees `chrome` (the Ambient Light case).
  const base = chrome.runtime.getURL('');
  document.documentElement.dataset.extUrl = base;
  await check('module.dynamic-import-sees-chrome', async () => {
    const m = await import(base + 'mod.js');
    return { ok: m.hasChrome === true && m.pageGlobalVisible === false && m.imported === 2 && window.modGlobal === 'module', d: `hasChrome=${m.hasChrome} pageGlobalVisible=${m.pageGlobalVisible} imported=${m.imported} modGlobal=${window.modGlobal}` };
  });
  await check('module.same-instance-on-reimport', async () => (await import(base + 'mod.js')) === (await import(base + 'mod.js')));

  // The page's CSP and Trusted Types policy are not the content script's (csp.html requires trusted types and forbids eval).
  if (location.pathname === '/csp.html') {
    await check('csp.eval-allowed-in-content-script', () => new Function('return 42')() === 42 && eval('6*7') === 42);
    await check('csp.inner-html-allowed-in-content-script', () => { const d = document.createElement('div'); d.innerHTML = '<i>t</i>'; return d.firstChild.localName === 'i'; });
    await check('csp.script-url-allowed-in-content-script', () => { const s = document.createElement('script'); s.src = base + 'inject.js'; document.head.appendChild(s); return true; });
    await check('csp.timer-string-allowed', () => new Promise((r) => { setTimeout('window.cspTimer = 1', 0); setTimeout(() => r(window.cspTimer === 1), 50); }));
  }
  if (location.pathname === '/page.html') {
    await check('iso.page-event-received', () => new Promise((r) => { const t = setInterval(() => { if (pingedWith !== null) { clearInterval(t); r(pingedWith === 'from-page'); } }, 50); setTimeout(() => { clearInterval(t); r(false); }, 4000); }));
    await check('ext.other-extensions-world-invisible', () => typeof extBGlobal === 'undefined');
    chrome.runtime.sendMessage({ kind: 'cs-ready' });
  }
})();
