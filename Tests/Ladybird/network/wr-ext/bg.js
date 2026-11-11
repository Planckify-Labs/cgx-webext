const seen = [];
const note = (what) => { seen.push(what); fetch('http://127.0.0.1:8765/report?wr=' + encodeURIComponent(what)).catch(() => {}); };
browser.webRequest.onBeforeRequest.addListener((d) => {
  if (d.url.includes('/report')) return {};
  if (d.url.includes('/ads/')) return { cancel: true };
  if (d.url.endsWith('/redirect-me.js')) return { redirectUrl: d.url.replace('redirect-me', 'redirected') };
  return {};
}, { urls: ['http://127.0.0.1/*'] }, ['blocking']);
browser.webRequest.onBeforeSendHeaders.addListener(async (d) => {
  if (!d.url.endsWith('/api.json')) return {};
  const headers = d.requestHeaders.filter((h) => h.name.toLowerCase() !== 'x-test');
  headers.push({ name: 'X-Added', value: 'by-listener' });
  return { requestHeaders: headers };
}, { urls: ['http://127.0.0.1/*'] }, ['blocking', 'requestHeaders']);
browser.webRequest.onHeadersReceived.addListener((d) => {
  if (!d.url.endsWith('/api.json')) return {};
  return { responseHeaders: d.responseHeaders.filter((h) => h.name.toLowerCase() !== 'x-server').concat([{ name: 'X-Replaced', value: 'yes' }]) };
}, { urls: ['http://127.0.0.1/*'] }, ['blocking', 'responseHeaders']);
browser.webRequest.onCompleted.addListener((d) => { if (d.type === 'main_frame') note(`completed ${d.type} ${d.statusCode} tab=${d.tabId >= 0} frame=${d.frameId}`); }, { urls: ['http://127.0.0.1/*'] });
browser.webRequest.onErrorOccurred.addListener((d) => { if (d.url.endsWith('/ads/ad.js')) note(`error ${d.error}`); }, { urls: ['http://127.0.0.1/*'] });
for (const name of ['onBeforeNavigate', 'onCommitted', 'onDOMContentLoaded', 'onCompleted'])
  browser.webNavigation[name].addListener((d) => note(`nav ${name} frame=${d.frameId} parent=${d.parentFrameId} ${new URL(d.url).pathname}`));
