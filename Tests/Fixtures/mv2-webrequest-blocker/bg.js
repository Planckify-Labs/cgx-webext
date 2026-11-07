browser.webRequest.onBeforeRequest.addListener(
  (details) => ({ cancel: details.url.includes('ads') }),
  { urls: ['<all_urls>'] },
  ['blocking']
);
browser.storage.local.get('enabled').then(console.log);
browser.browserAction.setBadgeText({ text: 'on' });
