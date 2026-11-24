const report = (c, ok, d) => fetch('http://127.0.0.1:8765/report?c=' + encodeURIComponent(c) + '&ok=' + (ok ? 1 : 0) + '&d=' + encodeURIComponent(String(d === undefined ? '' : d).slice(0, 200)), { mode: 'no-cors' }).catch(() => {});
// tabs.executeScript with code runs in the content-script world of the extension (MV2 / Firefox).
browser.runtime.onMessage.addListener(async (message, sender) => {
  if (message.kind !== 'ff-ready') return;
  try {
    const [r] = await browser.tabs.executeScript(sender.tab.id, { code: '[typeof ffOwnGlobal, typeof pageGlobal, typeof wrappedJSObject, typeof browser]' });
    report('firefox.tabs-executeScript', r[0] === 'number' && r[1] === 'undefined' && r[2] === 'object' && r[3] === 'object', r);
  } catch (e) { report('firefox.tabs-executeScript', false, e.message); }
});
