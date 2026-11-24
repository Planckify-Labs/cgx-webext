chrome.runtime.onInstalled.addListener(() => {
  chrome.action.setBadgeText({ text: 'API' });
  chrome.action.setBadgeBackgroundColor({ color: '#2563eb' });
});
