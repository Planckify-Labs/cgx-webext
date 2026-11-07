/*
 * store-chrome.js - the private browser APIs the Chrome Web Store (https://chromewebstore.google.com/) uses for its
 * "Add to Chrome" and "Remove from Chrome" buttons: chrome.webstorePrivate, chrome.management and
 * navigator.userAgentData.
 *
 * The browser runs this function expression in Chrome Web Store pages only, before the page's own scripts, and calls it
 * with a bridge (see webext::store and docs/STORES.md):
 *
 *   bridge.contextInit()          -> JSON, null if the browser does not consider this document a store page
 *   bridge.call(name, argsJson)   -> Promise<JSON string>, rejected with an Error carrying the message
 *   bridge.setMessageHandler(fn)  -> fn(JSON string) for { kind: 'event', name, args } from the browser
 *
 * Installing and removing happen in the browser, which asks the user first. Everything else the store asks is answered
 * here with what a normal, signed-out, non-managed browser would say.
 */
(function (bridge) {
  'use strict';
  const init = JSON.parse(bridge.contextInit());
  if (!init) return;

  const call = (name, args) => bridge.call(name, JSON.stringify(args)).then((text) => (text === '' ? undefined : JSON.parse(text)));

  // chrome.runtime.lastError is set only while a callback runs, as in Chrome.
  let lastError;
  function runCallback(callback, args, error) {
    lastError = error === undefined ? undefined : { message: String(error) };
    try {
      callback(...args);
    } catch (e) {
      console.error(e);
    } finally {
      lastError = undefined;
    }
  }
  const errorMessage = (e) => (e && e.message) || String(e);

  // An API function: callback style when the last argument is a function, a Promise otherwise.
  function api(impl) {
    return function (...args) {
      const callback = typeof args[args.length - 1] === 'function' ? args.pop() : undefined;
      let promise;
      try {
        promise = Promise.resolve(impl(...args));
      } catch (e) {
        promise = Promise.reject(e);
      }
      if (!callback) return promise;
      promise.then(
        (value) => runCallback(callback, value === undefined ? [] : [value]),
        (e) => runCallback(callback, [], errorMessage(e)),
      );
      return undefined;
    };
  }
  const answer = (value) => api(() => value);
  const unsupported = (name) => api(() => Promise.reject(new Error(name + ' is not supported by this browser.')));

  const listeners = new Map();
  function event(name) {
    const set = new Set();
    listeners.set(name, set);
    return {
      addListener: (f) => typeof f === 'function' && set.add(f),
      removeListener: (f) => set.delete(f),
      hasListener: (f) => set.has(f),
      hasListeners: () => set.size > 0,
    };
  }
  bridge.setMessageHandler((text) => {
    let message;
    try {
      message = JSON.parse(text);
    } catch (e) {
      return;
    }
    if (message.kind !== 'event') return;
    for (const listener of [...(listeners.get(message.name) || [])]) {
      try {
        listener(...(message.args || []));
      } catch (e) {
        console.error(e);
      }
    }
  });

  const chromeVersion = (/Chrome\/(\d+(?:\.\d+){3})/.exec(navigator.userAgent) || [])[1] || init.chromeVersion || '153.0.0.0';
  const majorVersion = chromeVersion.split('.')[0];

  // beginInstallWithManifest3 answers with a Result code; anything but success also sets lastError, as in Chrome.
  function beginInstallWithManifest3(details, callback) {
    const promise = call('cws.beginInstall', [details]).then(
      (result) => result || '',
      (e) => ({ failed: errorMessage(e) }),
    );
    if (typeof callback !== 'function') return promise.then((r) => (typeof r === 'string' ? r : 'unknown_error'));
    promise.then((result) => {
      if (typeof result !== 'string') runCallback(callback, ['unknown_error'], result.failed);
      else if (result === '' || result === 'success') runCallback(callback, [result]);
      else runCallback(callback, [result], result === 'user_cancelled' ? 'User cancelled install' : 'Install failed: ' + result);
    });
    return undefined;
  }

  const webstorePrivate = {
    beginInstallWithManifest3,
    completeInstall: api((id) => call('cws.completeInstall', [String(id)])),
    install: unsupported('webstorePrivate.install'),
    getExtensionStatus: api((id, manifest) => call('cws.getExtensionStatus', [String(id), manifest === undefined || manifest === null ? null : String(manifest)])),
    getFullChromeVersion: answer({ version_number: chromeVersion }),
    getReferrerChain: answer(''),
    getBrowserLogin: answer({ login: '' }),
    getStoreLogin: answer(''),
    setStoreLogin: answer(undefined),
    getWebGLStatus: answer('webgl_allowed'),
    getIsLauncherEnabled: answer(false),
    isInIncognitoMode: answer(!!init.incognito),
    isPendingCustodianApproval: answer(false),
    getEphemeralAppsEnabled: answer(false),
    getMV2DeprecationStatus: answer('inactive'),
    shouldShowEnterprisePromotionBanner: answer(false),
    logEnterprisePromoShown: answer(undefined),
    onEnterprisePromoClick: answer(undefined),
    enableAppLauncher: answer(undefined),
    requestExtension: unsupported('webstorePrivate.requestExtension'),
  };

  const management = {
    getAll: api(() => call('cws.getAll', [])),
    get: api((id) => call('cws.get', [String(id)])),
    getSelf: unsupported('management.getSelf'),
    getPermissionWarningsById: api((id) => call('cws.permissionWarningsById', [String(id)])),
    getPermissionWarningsByManifest: api((manifest) => call('cws.permissionWarningsByManifest', [String(manifest)])),
    setEnabled: api((id, enabled) => call('cws.setEnabled', [String(id), !!enabled])),
    uninstall: api((id, options) => call('cws.uninstall', [String(id), !!(options && options.showConfirmDialog)])),
    uninstallSelf: unsupported('management.uninstallSelf'),
    launchApp: unsupported('management.launchApp'),
    createAppShortcut: unsupported('management.createAppShortcut'),
    setLaunchType: unsupported('management.setLaunchType'),
    generateAppForLink: unsupported('management.generateAppForLink'),
    installReplacementWebApp: unsupported('management.installReplacementWebApp'),
    onInstalled: event('management.onInstalled'),
    onUninstalled: event('management.onUninstalled'),
    onEnabled: event('management.onEnabled'),
    onDisabled: event('management.onDisabled'),
  };

  const startTime = Date.now();
  const chrome = window.chrome && typeof window.chrome === 'object' ? window.chrome : {};
  const runtime = chrome.runtime && typeof chrome.runtime === 'object' ? chrome.runtime : {};
  if (!('lastError' in runtime)) Object.defineProperty(runtime, 'lastError', { get: () => lastError, enumerable: true, configurable: true });
  Object.assign(chrome, {
    webstorePrivate,
    management,
    runtime,
    csi: () => ({ startE: startTime, onloadT: startTime, pageT: Date.now() - startTime, tran: 15 }),
    loadTimes: () => ({ requestTime: startTime / 1000, startLoadTime: startTime / 1000, navigationType: 'Other', wasFetchedViaSpdy: true, connectionInfo: 'h2' }),
  });
  Object.defineProperty(window, 'chrome', { value: chrome, writable: true, configurable: true, enumerable: true });

  // The store decides whether this is a Chromium-based browser from navigator.userAgentData, which Ladybird does not have
  // yet. These are the low-entropy values a Chromium browser exposes, naming Ladybird as the browser brand.
  if (!('userAgentData' in Navigator.prototype)) {
    const platform = /Mac OS X/.test(navigator.userAgent) ? 'macOS' : /Windows/.test(navigator.userAgent) ? 'Windows' : /Android/.test(navigator.userAgent) ? 'Android' : 'Linux';
    const mobile = /Mobile/.test(navigator.userAgent);
    const brands = Object.freeze([
      Object.freeze({ brand: 'Not)A;Brand', version: '24' }),
      Object.freeze({ brand: 'Chromium', version: majorVersion }),
      Object.freeze({ brand: 'Ladybird', version: '1' }),
    ]);
    const fullVersionList = brands.map((b) => ({ brand: b.brand, version: b.brand === 'Chromium' ? chromeVersion : b.version + '.0.0.0' }));
    const userAgentData = {
      brands,
      mobile,
      platform,
      getHighEntropyValues(hints) {
        const values = { brands, mobile, platform };
        const all = { architecture: 'x86', bitness: '64', model: '', platformVersion: '', uaFullVersion: chromeVersion, fullVersionList, wow64: false, formFactors: ['Desktop'] };
        for (const hint of Array.isArray(hints) ? hints : []) if (hint in all) values[hint] = all[hint];
        return Promise.resolve(values);
      },
      toJSON() {
        return { brands, mobile, platform };
      },
    };
    Object.defineProperty(Navigator.prototype, 'userAgentData', { get: () => userAgentData, enumerable: true, configurable: true });
  }
})
