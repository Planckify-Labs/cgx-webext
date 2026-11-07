'use strict';
// Runs the store programs (Shim/store-chrome.js, Shim/store-firefox.js) against a fake bridge, in a context that looks
// enough like a browser window.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

function load(file, answers, userAgent) {
  const calls = [];
  let handler = null;
  const bridge = {
    contextInit: () => JSON.stringify({ store: file.includes('chrome') ? 'chrome' : 'firefox' }),
    call: (name, argsJson) => {
      const args = JSON.parse(argsJson);
      calls.push({ name, args });
      const answer = answers[name];
      if (answer instanceof Error) return Promise.reject(answer);
      const value = typeof answer === 'function' ? answer(...args) : answer;
      return Promise.resolve(value === undefined ? '' : JSON.stringify(value));
    },
    setMessageHandler: (fn) => {
      handler = fn;
    },
  };
  class Navigator {}
  const navigator = new Navigator();
  Object.defineProperty(Navigator.prototype, 'userAgent', { get: () => userAgent });
  const window = { console, Promise, Event, EventTarget, JSON, Object, Date, Map, Set, Array, String, Error, Navigator, navigator };
  window.window = window;
  vm.createContext(window);
  const program = vm.runInContext(fs.readFileSync(path.join(__dirname, '..', file), 'utf8'), window);
  program(bridge);
  return { window, calls, deliver: (message) => handler(JSON.stringify(message)) };
}

const tick = () => new Promise((resolve) => setImmediate(resolve));
const CHROME_UA = 'Mozilla/5.0 (X11; Linux x86_64) Ladybird/1.0 Chrome/153.0.0.0 AppleWebKit/537.36 Safari/537.36';

test('chrome store: static answers and the user agent data', async () => {
  const { window } = load('store-chrome.js', {}, CHROME_UA);
  const { chrome, navigator } = window;
  const version = await new Promise((resolve) => chrome.webstorePrivate.getFullChromeVersion(resolve));
  assert.equal(version.version_number, '153.0.0.0');
  assert.equal(await chrome.webstorePrivate.isInIncognitoMode(), false);
  assert.equal(await chrome.webstorePrivate.getMV2DeprecationStatus(), 'inactive');
  assert.equal(navigator.userAgentData.brands.map((b) => b.brand).join(), 'Not)A;Brand,Chromium,Ladybird');
  const high = await navigator.userAgentData.getHighEntropyValues(['uaFullVersion', 'bogus']);
  assert.equal(high.uaFullVersion, '153.0.0.0');
  assert.equal('bogus' in high, false);
});

test('chrome store: install result codes reach the callback with lastError', async () => {
  const { window, calls } = load('store-chrome.js', { 'cws.beginInstall': 'user_cancelled', 'cws.completeInstall': new Error('Download failed') }, CHROME_UA);
  const { chrome } = window;
  const seen = await new Promise((resolve) => chrome.webstorePrivate.beginInstallWithManifest3({ id: 'x', manifest: '{}' }, (r) => resolve([r, chrome.runtime.lastError])));
  assert.equal(seen[0], 'user_cancelled');
  assert.match(seen[1].message, /cancel/i);
  assert.equal(chrome.runtime.lastError, undefined);
  assert.equal(calls[0].name, 'cws.beginInstall');

  const error = await new Promise((resolve) => chrome.webstorePrivate.completeInstall('x', () => resolve(chrome.runtime.lastError)));
  assert.equal(error.message, 'Download failed');
});

test('chrome store: management calls and events', async () => {
  const info = { id: 'abc', name: 'A', enabled: true };
  const { window, calls, deliver } = load('store-chrome.js', { 'cws.getAll': [info], 'cws.uninstall': undefined }, CHROME_UA);
  const { chrome } = window;
  assert.deepEqual(await new Promise((resolve) => chrome.management.getAll(resolve)), [info]);
  await chrome.management.uninstall('abc', { showConfirmDialog: true });
  assert.deepEqual(calls.at(-1), { name: 'cws.uninstall', args: ['abc', true] });

  const installed = [];
  chrome.management.onInstalled.addListener((i) => installed.push(i.id));
  deliver({ kind: 'event', name: 'management.onInstalled', args: [info] });
  deliver({ kind: 'run', stage: 'document_end' });
  assert.deepEqual(installed, ['abc']);
});

test('firefox store: install events and manager events', async () => {
  const addon = { id: 'a@example.org', version: '1', name: 'A' };
  const { window, calls, deliver } = load('store-firefox.js', { 'amo.getAddonByID': (id) => (id === addon.id ? addon : null), 'amo.install': undefined, 'amo.uninstall': true }, 'Firefox/156.0');
  const manager = window.navigator.mozAddonManager;
  assert.equal(await manager.getAddonByID('nope'), null);
  const found = await manager.getAddonByID(addon.id);
  assert.equal(found.name, 'A');
  assert.equal(found.isEnabled, true);
  assert.equal(await found.uninstall(), true);

  await assert.rejects(manager.createInstall({ url: 'http://example.org/a.xpi' }));
  const install = await manager.createInstall({ url: 'https://addons.mozilla.org/a.xpi', hash: 'sha256:00' });
  const events = [];
  for (const name of ['onDownloadStarted', 'onInstallEnded']) install.addEventListener(name, () => events.push([name, install.state]));
  await install.install();
  assert.deepEqual(calls.at(-1), { name: 'amo.install', args: [1, 'https://addons.mozilla.org/a.xpi', 'sha256:00'] });
  await assert.rejects(install.install());
  deliver({ kind: 'install', installId: 1, event: 'onDownloadStarted', state: 'STATE_DOWNLOADING' });
  deliver({ kind: 'install', installId: 1, event: 'onInstallEnded', state: 'STATE_INSTALLED', progress: 10, maxProgress: 10 });
  assert.deepEqual(events, [['onDownloadStarted', 'STATE_DOWNLOADING'], ['onInstallEnded', 'STATE_INSTALLED']]);
  assert.equal(install.progress, 10);

  const ids = [];
  manager.addEventListener('onInstalled', (e) => ids.push(e.id));
  deliver({ kind: 'event', name: 'onInstalled', id: addon.id });
  assert.deepEqual(ids, [addon.id]);
  await tick();
});
