const status = (text) => { document.getElementById('status').textContent = text; };

async function render() {
  const [active] = await chrome.tabs.query({ active: true, currentWindow: true });
  document.getElementById('active').textContent = active ? `${active.title || '(no title)'} — ${active.url}` : 'none';
  const tabs = await chrome.tabs.query({ currentWindow: true });
  const list = document.getElementById('tabs');
  list.replaceChildren(...tabs.map((t) => Object.assign(document.createElement('li'), { textContent: t.title || t.url })));
  const { history = [] } = await chrome.storage.local.get('history');
  document.getElementById('history').replaceChildren(...history.map((h) => Object.assign(document.createElement('li'), { textContent: h })));
  document.getElementById('badge').value = await chrome.action.getBadgeText({});
}

document.getElementById('outline').onclick = async () => {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  try {
    const [r] = await chrome.scripting.executeScript({
      target: { tabId: tab.id },
      func: () => { document.documentElement.style.outline = '6px solid #2e7dd2'; return document.querySelectorAll('*').length; },
    });
    status(`Outlined: the page has ${r.result} elements.`);
  } catch (e) {
    status(`executeScript failed: ${e.message}`);
  }
};
document.getElementById('newtab').onclick = () => chrome.tabs.create({ url: 'https://example.com/' });
document.getElementById('options').onclick = () => chrome.runtime.openOptionsPage();
document.getElementById('ping').onclick = async () => status(await chrome.runtime.sendMessage('ping'));
document.getElementById('setbadge').onclick = async () => {
  await chrome.action.setBadgeText({ text: document.getElementById('badge').value });
  status('Badge updated.');
};
chrome.storage.onChanged.addListener(render);
render();
