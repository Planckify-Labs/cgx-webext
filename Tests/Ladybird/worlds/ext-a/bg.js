const report = (c, ok, d) => fetch('http://127.0.0.1:8765/report?c=' + encodeURIComponent(c) + '&ok=' + (ok ? 1 : 0) + '&d=' + encodeURIComponent(String(d === undefined ? '' : d).slice(0, 200)), { mode: 'no-cors' }).catch(() => {});

// Scripts registered now are run in the next documents they match.
chrome.scripting.registerContentScripts([
  { id: 'reg-main', matches: ['http://127.0.0.1/page2.html'], js: ['reg-main.js'], world: 'MAIN', runAt: 'document_end' },
  { id: 'reg-iso', matches: ['http://127.0.0.1/page2.html'], js: ['reg-iso.js'], runAt: 'document_end' },
]).catch((e) => report('register.registered', false, e.message));

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (message.kind === 'ping') { sendResponse('pong'); return; }
  if (message.kind !== 'cs-ready') return;
  const target = { tabId: sender.tab.id, frameIds: [sender.frameId] };
  (async () => {
    try {
      const [a] = await chrome.scripting.executeScript({ target, func: () => ({ csGlobal: typeof csGlobal, early: typeof earlyGlobal, pageGlobal: typeof pageGlobal, api: typeof chrome.runtime.id, pushTampered: Array.prototype.push.toString().includes('pushCalls') }) });
      report('inject.isolated-is-the-content-script-world', a.result.csGlobal === 'number' && a.result.early === 'string' && a.result.pageGlobal === 'undefined' && a.result.api === 'string' && !a.result.pushTampered, JSON.stringify(a.result));
      await chrome.scripting.executeScript({ target, func: () => { window.injected = 7; } });
      const [b] = await chrome.scripting.executeScript({ target, func: () => typeof injected });
      report('inject.isolated-globals-persist', b.result === 'number', b.result);
      const [c] = await chrome.scripting.executeScript({ target, world: 'MAIN', func: () => ({ pageGlobal: typeof pageGlobal, csGlobal: typeof csGlobal, injected: typeof injected, runtime: typeof chrome === 'undefined' ? 'undefined' : typeof chrome.runtime, browser: typeof browser, pushTampered: Array.prototype.push.toString().includes('pushCalls') }) });
      report('inject.main-is-the-page-world', c.result.pageGlobal === 'string' && c.result.csGlobal === 'undefined' && c.result.injected === 'undefined' && c.result.runtime === 'undefined' && c.result.browser === 'undefined' && c.result.pushTampered, JSON.stringify(c.result));
      const [d] = await chrome.scripting.executeScript({ target, files: ['inject-file.js'] });
      report('inject.files', d.result === 'file:number', d.result);
      const [e] = await chrome.scripting.executeScript({ target, func: async () => { await new Promise((r) => setTimeout(r, 20)); return 'async-ok'; } });
      report('inject.awaits-promise-results', e.result === 'async-ok', e.result);
    } catch (e) { report('inject.executeScript-works', false, e.message); }
  })();
});
