// Keeps the badge equal to the number of open tabs, and reacts to the context menu and the shortcut.
async function updateBadge() {
  const tabs = await chrome.tabs.query({});
  await chrome.action.setBadgeText({ text: String(tabs.length) });
  await chrome.action.setBadgeBackgroundColor({ color: '#2e7dd2' });
}

function outline(color) {
  document.documentElement.style.outline = `6px solid ${color}`;
  document.documentElement.style.outlineOffset = '-6px';
  return document.title;
}

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({ id: 'outline', title: 'Outline this page (Ladybird Extension Test)', contexts: ['page'] });
    chrome.contextMenus.create({ id: 'shout', title: 'Log "%s" in the popup history', contexts: ['selection'] });
  });
  updateBadge();
});
chrome.runtime.onStartup.addListener(updateBadge);
chrome.tabs.onCreated.addListener(updateBadge);
chrome.tabs.onRemoved.addListener(updateBadge);

async function remember(entry) {
  const { history = [] } = await chrome.storage.local.get('history');
  history.unshift(`${new Date().toLocaleTimeString()} ${entry}`);
  await chrome.storage.local.set({ history: history.slice(0, 10) });
}

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === 'outline') {
    const [result] = await chrome.scripting.executeScript({ target: { tabId: tab.id }, func: outline, args: ['orange'] });
    await remember(`outlined "${result.result}" from the context menu`);
  } else if (info.menuItemId === 'shout') {
    await remember(`selected "${info.selectionText}"`);
  }
});

chrome.commands.onCommand.addListener(async (command, tab) => {
  if (command !== 'mark-page' || !tab) return;
  const [result] = await chrome.scripting.executeScript({ target: { tabId: tab.id }, func: outline, args: ['crimson'] });
  await remember(`outlined "${result.result}" with the shortcut`);
});

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (message === 'ping') sendResponse(`pong from the service worker (${sender.url ? 'popup' : 'unknown'})`);
});
