// A Firefox extension's content script: `browser`, and wrappedJSObject / cloneInto / exportFunction to reach the page.
const report = (c, ok, d) => fetch('/report?c=' + encodeURIComponent(c) + '&ok=' + (ok ? 1 : 0) + '&d=' + encodeURIComponent(String(d === undefined ? '' : d).slice(0, 200)));
const check = async (name, body) => {
  try { const r = await body(); await report(name, r === true || (r && r.ok), r && r.d); }
  catch (e) { await report(name, false, 'threw: ' + (e && e.message || e)); }
};
window.ffOwnGlobal = 1;
(async () => {
  await check('firefox.has-browser-and-helpers', () => typeof browser.runtime.id === 'string' && typeof cloneInto === 'function' && typeof exportFunction === 'function' && typeof createObjectIn === 'function');
  await check('firefox.window-wrappedJSObject-is-page-window', () => window.wrappedJSObject.pageGlobal === 'page-value' && window.wrappedJSObject.ffOwnGlobal === undefined && typeof pageGlobal === 'undefined' && window.wrappedJSObject !== window);
  await check('firefox.element-wrappedJSObject-shows-page-expando', () => {
    const el = document.getElementById('shared');
    return el.pageExpando === undefined && el.wrappedJSObject.pageExpando === 'page-expando' && document.wrappedJSObject.getElementById('shared') === el.wrappedJSObject;
  });
  await check('firefox.page-prototypes-are-the-pages', () => window.wrappedJSObject.Array.prototype.push.toString().includes('pushCalls') && Array.prototype.push.toString().includes('native'));
  await check('firefox.cloneInto-and-exportFunction', () => {
    window.wrappedJSObject.ffApi = cloneInto({ a: 1, nested: { b: [1, 2] }, fn: () => 7 }, window, { cloneFunctions: true });
    exportFunction(function (x) { return x * 2; }, window, { defineAs: 'ffCall' });
    const w = window.wrappedJSObject;
    return { ok: true, d: `${typeof w.ffCall} ${w.ffCall && w.ffCall.name} ${w.ffApi && Object.getPrototypeOf(w.ffApi) === w.Object.prototype}` };
  });
  await check('firefox.cloneInto-without-cloneFunctions-refuses-functions', () => { try { cloneInto({ f() {} }, window); return false; } catch (e) { return true; } });
  await check('firefox.cloneInto-structured-types', () => {
    const c = cloneInto({ when: new Date(0), m: new Map([[1, 2]]), ab: new Uint8Array([1, 2]) }, window.wrappedJSObject);
    return c.when instanceof window.wrappedJSObject.Date && c.m instanceof window.wrappedJSObject.Map && c.ab instanceof window.wrappedJSObject.Uint8Array && c.ab[1] === 2;
  });
  await check('firefox.createObjectIn', () => { const o = createObjectIn(window, { defineAs: 'ffObject' }); o.x = 1; return window.wrappedJSObject.ffObject === o || window.wrappedJSObject.ffObject !== undefined; });
  browser.runtime.sendMessage({ kind: 'ff-ready' });
})();
