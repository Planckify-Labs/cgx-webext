const checks = [
  ['publicSuffix.getDomain', async () => {
    const d = await chrome.publicSuffix.getDomain('www.bbc.co.uk');
    if (d !== 'bbc.co.uk') throw new Error(`got ${d}`);
    return d;
  }],
  ['i18n.detectLanguage', async () => {
    const r = await chrome.i18n.detectLanguage('Bonjour tout le monde, nous sommes heureux');
    if (!r.languages.length || r.languages[0].language !== 'fr') throw new Error(JSON.stringify(r));
    return `${r.languages[0].language} (${r.languages[0].percentage}%)`;
  }],
  ['storage.session.setAccessLevel', async () => {
    await chrome.storage.session.setAccessLevel({ accessLevel: 'TRUSTED_AND_UNTRUSTED_CONTEXTS' });
    await chrome.storage.session.set({ ping: 'pong' });
    const v = await chrome.storage.session.get('ping');
    if (v.ping !== 'pong') throw new Error(JSON.stringify(v));
    return 'ok';
  }],
  ['cookies.set / get / remove', async () => {
    await chrome.cookies.set({ url: 'https://example.com/', name: 'cgx_e2e', value: 'hello', path: '/' });
    const c = await chrome.cookies.get({ url: 'https://example.com/', name: 'cgx_e2e' });
    if (!c || c.value !== 'hello') throw new Error(JSON.stringify(c));
    await chrome.cookies.remove({ url: 'https://example.com/', name: 'cgx_e2e' });
    return `value=${c.value}`;
  }],
  ['bookmarks CRUD + search', async () => {
    const node = await chrome.bookmarks.create({ title: 'CGX Test', url: 'https://example.com/cgx' });
    const found = await chrome.bookmarks.search('CGX Test');
    await chrome.bookmarks.remove(node.id);
    if (!found.length) throw new Error('search returned empty');
    return `created+removed #${node.id}`;
  }],
  ['history.addUrl / search / deleteUrl', async () => {
    await chrome.history.addUrl({ url: 'https://example.com/cgx-hist', title: 'CGX Hist' });
    const items = await chrome.history.search({ text: 'cgx-hist', maxResults: 5 });
    await chrome.history.deleteUrl({ url: 'https://example.com/cgx-hist' });
    return `${items.length} match(es)`;
  }],
  ['topSites.get', async () => {
    const sites = await chrome.topSites.get();
    return `${sites.length} site(s)`;
  }],
  ['browsingData.settings', async () => {
    const s = await chrome.browsingData.settings();
    if (!s.dataToRemove) throw new Error('missing dataToRemove');
    return 'ok';
  }],
  ['sessions.getRecentlyClosed', async () => {
    const list = await chrome.sessions.getRecentlyClosed();
    return `${list.length} closed`;
  }],
  ['notifications create / getAll / clear', async () => {
    const id = await chrome.notifications.create({ type: 'basic', title: 'CGX', message: 'Hello from Ladybird!' });
    const all = await chrome.notifications.getAll();
    await chrome.notifications.clear(id);
    if (!all[id]) throw new Error('not listed');
    return id;
  }],
  ['idle.queryState', async () => {
    const state = await chrome.idle.queryState(60);
    return state;
  }],
  ['tabGroups + tabs.group / ungroup', async () => {
    const tabs = await chrome.tabs.query({ currentWindow: true });
    if (!tabs.length) return 'no tabs';
    const gid = await chrome.tabs.group({ tabIds: [tabs[0].id] });
    const g = await chrome.tabGroups.update(gid, { title: 'Test', color: 'blue' });
    await chrome.tabs.ungroup([tabs[0].id]);
    return `${g.title}/${g.color}`;
  }],
  ['userScripts register / unregister', async () => {
    await chrome.userScripts.register([{ id: 'e2e-u1', matches: ['https://example.com/*'], js: [{ code: '1+1' }] }]);
    const list = await chrome.userScripts.getScripts();
    await chrome.userScripts.unregister({ ids: ['e2e-u1'] });
    return `${list.length} script`;
  }],
  ['offscreen create / has / close', async () => {
    await chrome.offscreen.createDocument({ url: 'popup.html', reasons: ['DOM_PARSER'], justification: 'test' });
    const has = await chrome.offscreen.hasDocument();
    await chrome.offscreen.closeDocument();
    if (!has) throw new Error('hasDocument was false');
    return 'ok';
  }],
  ['sidePanel setOptions / getOptions', async () => {
    await chrome.sidePanel.setOptions({ path: 'popup.html', enabled: true });
    const o = await chrome.sidePanel.getOptions({});
    return o.path;
  }],
  ['fontSettings set / get / clear', async () => {
    await chrome.fontSettings.setDefaultFontSize({ pixelSize: 18 });
    const sz = await chrome.fontSettings.getDefaultFontSize({});
    await chrome.fontSettings.clearDefaultFontSize({});
    if (sz.pixelSize !== 18) throw new Error(JSON.stringify(sz));
    return `${sz.pixelSize}px`;
  }],
  ['privacy BrowserSetting get / set / clear', async () => {
    await chrome.privacy.network.networkPredictionEnabled.set({ value: false });
    const cur = await chrome.privacy.network.networkPredictionEnabled.get({});
    await chrome.privacy.network.networkPredictionEnabled.clear({});
    if (cur.value !== false) throw new Error(JSON.stringify(cur));
    return cur.levelOfControl;
  }],
  ['management.getSelf + warnings', async () => {
    const self = await chrome.management.getSelf();
    const w = await chrome.management.getPermissionWarningsById(self.id);
    return `${w.length} warning(s)`;
  }],
  ['identity.getRedirectURL', async () => {
    const url = await chrome.identity.getRedirectURL('oauth');
    return url;
  }],
  ['system.cpu.getInfo', async () => {
    const info = await chrome.system.cpu.getInfo();
    return `${info.numOfProcessors} CPUs (${info.archName})`;
  }],
  ['power keepAwake', async () => {
    await chrome.power.requestKeepAwake('display');
    await chrome.power.releaseKeepAwake();
    return 'ok';
  }],
];

async function run() {
  const ul = document.getElementById('results');
  const summary = document.getElementById('summary');
  ul.textContent = '';
  let passed = 0;
  for (const [name, fn] of checks) {
    const li = document.createElement('li');
    const left = document.createElement('span');
    left.textContent = name;
    const right = document.createElement('span');
    try {
      const detail = await fn();
      passed++;
      right.className = 'ok';
      right.textContent = `PASS`;
      const d = document.createElement('span');
      d.className = 'detail';
      d.textContent = String(detail);
      li.append(left, d, right);
    } catch (e) {
      right.className = 'fail';
      right.textContent = 'FAIL';
      const d = document.createElement('span');
      d.className = 'detail';
      d.textContent = e && e.message ? e.message : String(e);
      li.append(left, d, right);
    }
    ul.appendChild(li);
  }
  summary.textContent = `${passed}/${checks.length} checks passed`;
}

document.getElementById('rerun').addEventListener('click', run);
run();
