'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { createApi } = require('../webext-shim.js');

const registry = JSON.parse(fs.readFileSync(path.join(__dirname, '../../Data/api-support.json'), 'utf8'));

// Builds a surface like Registry::surface_json, but without permission or manifest-version filtering.
function fullSurface() {
  const surface = {};
  for (const [ns, def] of Object.entries(registry.namespaces)) {
    const entry = { functions: [], events: [], properties: [] };
    for (const [kind, key] of [['functions', 'functions'], ['events', 'events'], ['properties', 'properties']]) {
      for (const list of Object.values(def[kind] || {})) entry[key].push(...list);
    }
    surface[ns] = entry;
  }
  return surface;
}

function make({ mv = 3, surface, bridge } = {}) {
  const calls = [];
  const defaultBridge = {
    call: async (name, args) => { calls.push({ name, args }); return { echoed: name }; },
    callSync: (name, args) => { calls.push({ name, args, sync: true }); return `sync:${name}`; },
  };
  const init = {
    id: 'abcdefghijklmnopabcdefghijklmnop',
    baseUrl: 'chrome-extension://abcdefghijklmnopabcdefghijklmnop/',
    manifest: { manifest_version: mv, name: 'T', version: '1' },
    uiLanguage: 'en-US',
    syncFunctions: registry.sync_functions,
    surface: surface || fullSurface(),
  };
  return { api: createApi(init, bridge || defaultBridge, { console: { error() {}, warn() {} } }), calls, init };
}

test('every function in the registry becomes callable on both objects', () => {
  const { api } = make();
  for (const [ns, def] of Object.entries(registry.namespaces)) {
    for (const list of Object.values(def.functions || {})) {
      for (const fn of list) {
        for (const obj of [api.browser, api.chrome]) {
          let cur = obj[ns];
          assert.ok(cur, `${ns} missing`);
          const parts = fn.split('.');
          for (const p of parts.slice(0, -1)) { cur = cur[p]; assert.ok(cur, `${ns}.${p}`); }
          assert.equal(typeof cur[parts.at(-1)], 'function', `${ns}.${fn}`);
        }
      }
    }
  }
});

test('every event in the registry has the listener API', () => {
  const { api } = make();
  for (const [ns, def] of Object.entries(registry.namespaces)) {
    for (const list of Object.values(def.events || {})) {
      for (const ev of list) {
        let cur = api.chrome[ns];
        for (const p of ev.split('.')) cur = cur[p];
        for (const m of ['addListener', 'removeListener', 'hasListener', 'hasListeners']) assert.equal(typeof cur[m], 'function', `${ns}.${ev}.${m}`);
      }
    }
  }
});

test('registry properties marked implemented exist in the shim', () => {
  const { api, init } = make();
  const implemented = [];
  for (const [ns, def] of Object.entries(registry.namespaces)) {
    for (const [status, list] of Object.entries((def.properties || {}))) {
      if (status === 'implemented' || status === 'partial') for (const p of list) implemented.push(`${ns}.${p}`);
    }
  }
  assert.ok(implemented.includes('runtime.id') && implemented.includes('runtime.lastError'));
  for (const full of implemented) {
    const [ns, ...rest] = full.split('.');
    let cur = api.chrome[ns];
    for (const p of rest.slice(0, -1)) cur = cur[p];
    assert.ok(rest.at(-1) in cur, `${full} is marked implemented but the shim does not define it`);
  }
  assert.equal(api.chrome.runtime.id, init.id);
  assert.equal(api.browser.runtime.id, init.id);
});

test('sync functions listed in the registry are all provided synchronously', () => {
  const { api, calls } = make();
  assert.equal(api.chrome.i18n.getMessage('hi', ['x']), 'sync:i18n.getMessage');
  assert.deepEqual(calls.at(-1), { name: 'i18n.getMessage', args: ['hi', ['x']], sync: true });
  assert.equal(api.chrome.runtime.getURL('/a/b.html'), 'chrome-extension://abcdefghijklmnopabcdefghijklmnop/a/b.html');
  assert.equal(api.browser.runtime.getURL('a.html'), 'chrome-extension://abcdefghijklmnopabcdefghijklmnop/a.html');
  assert.equal(api.chrome.runtime.getManifest().name, 'T');
  assert.equal(api.chrome.i18n.getUILanguage(), 'en-US');
  assert.equal(api.chrome.extension.getURL('x'), 'chrome-extension://abcdefghijklmnopabcdefghijklmnop/x');
  for (const f of registry.sync_functions) {
    const [ns, ...rest] = f.split('.');
    assert.equal(typeof api.chrome[ns][rest.join('.')], 'function', f);
  }
});

test('browser.* is promise based and rejects callbacks', async () => {
  const { api, calls } = make();
  const result = await api.browser.storage.local.get('k');
  assert.deepEqual(result, { echoed: 'storage.local.get' });
  assert.deepEqual(calls.at(-1), { name: 'storage.local.get', args: ['k'] });
  assert.throws(() => api.browser.storage.local.get('k', () => {}), TypeError);
  // Trailing undefined arguments are dropped so optional parameters behave.
  await api.browser.alarms.create('a', { delayInMinutes: 1 }, undefined);
  assert.deepEqual(calls.at(-1).args, ['a', { delayInMinutes: 1 }]);
});

test('chrome.* callback style delivers results and sets lastError', async () => {
  let fail = false;
  const bridge = { call: async () => { if (fail) throw new Error('boom'); return 42; }, callSync() {} };
  const { api } = make({ bridge });

  const ok = await new Promise((resolve) => api.chrome.storage.local.get('k', (r) => resolve({ r, err: api.chrome.runtime.lastError })));
  assert.deepEqual(ok, { r: 42, err: undefined });

  fail = true;
  const bad = await new Promise((resolve) => api.chrome.storage.local.get('k', (r) => resolve({ r, err: api.chrome.runtime.lastError && api.chrome.runtime.lastError.message })));
  assert.deepEqual(bad, { r: undefined, err: 'boom' });
  await new Promise((r) => setImmediate(r));
  assert.equal(api.chrome.runtime.lastError, undefined, 'lastError must be cleared after the callback');
  // browser.* sees the same lastError object semantics but never relies on it.
  await assert.rejects(api.browser.storage.local.get('k'), /boom/);
});

test('chrome.* without a callback returns a promise only in MV3', async () => {
  const mv3 = make({ mv: 3 });
  assert.ok(mv3.api.chrome.storage.local.get('k') instanceof Promise);
  const mv2 = make({ mv: 2 });
  assert.equal(mv2.api.chrome.storage.local.get('k'), undefined);
  assert.equal(mv2.calls.at(-1).name, 'storage.local.get', 'the call must still be made');
});

test('listener exceptions in callbacks do not escape or wedge lastError', async () => {
  const errors = [];
  const bridge = { call: async () => { throw new Error('x'); }, callSync() {} };
  const init = { id: 'i', baseUrl: 'b/', manifest: { manifest_version: 3 }, uiLanguage: 'en', syncFunctions: [], surface: { storage: { functions: ['local.get'], events: [], properties: [] }, runtime: { functions: [], events: [], properties: ['id', 'lastError'] } } };
  const api = createApi(init, bridge, { console: { error: (...a) => errors.push(a) } });
  await new Promise((resolve) => api.chrome.storage.local.get('k', () => { setImmediate(resolve); throw new Error('callback bug'); }));
  assert.equal(errors.length, 1);
  assert.equal(api.chrome.runtime.lastError, undefined);
});

test('namespaces without permission are absent', () => {
  const surface = { runtime: { functions: ['getURL'], events: [], properties: ['id', 'lastError'] } };
  const { api } = make({ surface });
  assert.equal(api.chrome.storage, undefined);
  assert.equal(api.browser.tabs, undefined);
  assert.equal(typeof api.chrome.runtime.getURL, 'function');
});

test('events: add/remove/has and dispatch', () => {
  const { api } = make();
  const seen = [];
  const a = (...x) => seen.push(['a', ...x]);
  const b = () => { throw new Error('bad listener'); };
  api.chrome.storage.onChanged.addListener(a);
  api.browser.storage.onChanged.addListener(b); // Same underlying event as chrome.*.
  assert.ok(api.chrome.storage.onChanged.hasListener(a));
  assert.ok(api.chrome.storage.onChanged.hasListeners());
  assert.equal(api.dispatchEvent('storage.onChanged', [{ k: { newValue: 1 } }, 'local']), 2);
  assert.deepEqual(seen, [['a', { k: { newValue: 1 } }, 'local']]);
  api.chrome.storage.onChanged.removeListener(a);
  assert.ok(!api.chrome.storage.onChanged.hasListener(a));
  assert.equal(api.dispatchEvent('no.such.event', []), 0);
  assert.throws(() => api.chrome.storage.onChanged.addListener('nope'), TypeError);
});

test('aliases: browserAction and menus', () => {
  const { api } = make();
  assert.equal(api.chrome.browserAction, api.chrome.action);
  assert.equal(api.chrome.menus, api.chrome.contextMenus);
});

test('onMessage: sendResponse, return true, promise and unhandled', async () => {
  const { api } = make();
  const l = api.chrome.runtime.onMessage;

  assert.deepEqual(await api.deliverMessage({ a: 1 }, {}), { hasListeners: false, handled: false, response: undefined });

  const sync = (m, s, send) => { send({ got: m }); };
  l.addListener(sync);
  assert.deepEqual(await api.deliverMessage('hi', {}), { hasListeners: true, handled: true, response: { got: 'hi' } });
  l.removeListener(sync);

  const later = (m, s, send) => { setTimeout(() => send('late'), 5); return true; };
  l.addListener(later);
  assert.deepEqual(await api.deliverMessage('x', {}), { hasListeners: true, handled: true, response: 'late' });
  l.removeListener(later);

  const prom = async (m) => `promised ${m}`;
  l.addListener(prom);
  assert.deepEqual(await api.deliverMessage('y', {}), { hasListeners: true, handled: true, response: 'promised y' });
  l.removeListener(prom);

  // A throwing listener is skipped; a listener returning nothing leaves the message unhandled.
  l.addListener(() => { throw new Error('x'); });
  l.addListener(() => undefined);
  assert.deepEqual(await api.deliverMessage('z', {}), { hasListeners: true, handled: false, response: undefined });
});

test('constants are defined where the namespace is visible', () => {
  const { api } = make();
  assert.equal(api.chrome.tabs.TAB_ID_NONE, -1);
  assert.equal(api.browser.windows.WINDOW_ID_CURRENT, -2);
  assert.equal(api.chrome.storage.sync.QUOTA_BYTES, 102400);
  assert.equal(api.chrome.storage.local.QUOTA_BYTES, 10485760);
  // Constants are read-only and do not collide with the functions of the same object.
  assert.throws(() => { 'use strict'; api.chrome.tabs.TAB_ID_NONE = 5; }, TypeError);
  assert.equal(typeof api.chrome.storage.sync.get, 'function');
  const { api: limited } = make({ surface: { runtime: { functions: [], events: [], properties: [] } } });
  assert.equal(limited.chrome.tabs, undefined);
});
