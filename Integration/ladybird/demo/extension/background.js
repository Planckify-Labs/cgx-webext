// Runs in the hidden background page. Counts the pages the content script reported, in chrome.storage.local.
chrome.runtime.onInstalled.addListener(() => {
  chrome.storage.local.set({ installedAt: new Date().toISOString(), visits: 0 });
});

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (message.type !== 'visit') return;
  chrome.storage.local.get({ visits: 0, installedAt: '?' }).then(({ visits, installedAt }) => {
    visits += 1;
    chrome.storage.local.set({ visits });
    sendResponse({ visits, installedAt, fromTab: sender.tab ? sender.tab.id : null });
  });
  return true; // Answer asynchronously.
});
