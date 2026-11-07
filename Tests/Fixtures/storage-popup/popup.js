chrome.storage.local.get('visits', ({ visits }) => {
  document.body.textContent = chrome.i18n.getMessage('visitCount', [String(visits || 0)]);
});
chrome.tabs.query({ active: true, currentWindow: true }, (tabs) => console.log(tabs));
