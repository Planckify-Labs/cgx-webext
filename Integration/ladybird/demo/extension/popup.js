const facts = document.getElementById('facts');
function show(items) {
  facts.innerHTML = '';
  for (const [k, v] of items) {
    const li = document.createElement('li');
    li.textContent = k + ': ' + v;
    facts.appendChild(li);
  }
}
(async () => {
  try {
    const stored = await chrome.storage.local.get(null);
    show([
      ['origin', location.origin],
      ['chrome.runtime.id', chrome.runtime.id],
      ['manifest name', chrome.runtime.getManifest().name],
      ['getURL', chrome.runtime.getURL('popup.html')],
      ['storage.local', JSON.stringify(stored)],
    ]);
  } catch (e) {
    show([['error', e.message]]);
  }
})();
