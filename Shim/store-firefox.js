/*
 * store-firefox.js - navigator.mozAddonManager, the private API addons.mozilla.org (https://addons.mozilla.org/) uses
 * for its "Add to Firefox" and "Remove" buttons. Modeled on Firefox's dom/webidl/AddonManager.webidl.
 *
 * The browser runs this function expression in addons.mozilla.org pages only, before the page's own scripts, and calls
 * it with a bridge (see webext::store and docs/STORES.md):
 *
 *   bridge.contextInit()          -> JSON, null if the browser does not consider this document a store page
 *   bridge.call(name, argsJson)   -> Promise<JSON string>, rejected with an Error carrying the message
 *   bridge.setMessageHandler(fn)  -> fn(JSON string) for
 *       { kind: 'install', installId, event, state, error, progress, maxProgress }   progress of an AddonInstall
 *       { kind: 'event', name, id }                                                 onInstalled, onUninstalled, ...
 *
 * Downloading, checking the hash, asking the user and installing happen in the browser.
 */
(function (bridge) {
  'use strict';
  const init = JSON.parse(bridge.contextInit());
  if (!init) return;

  const call = (name, args) => bridge.call(name, JSON.stringify(args)).then((text) => (text === '' ? undefined : JSON.parse(text)));

  function makeEvent(name, fields) {
    const event = new Event(name);
    for (const [key, value] of Object.entries(fields)) Object.defineProperty(event, key, { value, enumerable: true });
    return event;
  }

  class Addon {
    constructor(info) {
      this.id = info.id;
      this.version = info.version;
      this.type = info.type || 'extension';
      this.name = info.name;
      this.description = info.description || '';
      this.isEnabled = info.isEnabled !== false;
      this.isActive = info.isActive !== false;
      this.canUninstall = info.canUninstall !== false;
      Object.freeze(this);
    }
    uninstall() {
      return call('amo.uninstall', [this.id]).then((removed) => removed !== false);
    }
    setEnabled(value) {
      return call('amo.setEnabled', [this.id, !!value]).then(() => undefined);
    }
  }

  const installs = new Map();
  let nextInstallId = 1;

  class AddonInstall extends EventTarget {
    #id;
    #started = false;
    constructor(id) {
      super();
      this.#id = id;
      this.state = 'STATE_AVAILABLE';
      this.error = null;
      this.progress = 0;
      this.maxProgress = -1;
    }
    install() {
      if (this.#started) return Promise.reject(new Error('The install has already started.'));
      this.#started = true;
      const { url, hash } = installs.get(this.#id).request;
      return call('amo.install', [this.#id, url, hash]).then(() => undefined);
    }
    cancel() {
      return call('amo.cancel', [this.#id]).then(() => undefined);
    }
  }

  bridge.setMessageHandler((text) => {
    let message;
    try {
      message = JSON.parse(text);
    } catch (e) {
      return;
    }
    if (message.kind === 'install') {
      const entry = installs.get(message.installId);
      if (!entry) return;
      const install = entry.install;
      if (typeof message.state === 'string') install.state = message.state;
      if (message.error !== undefined) install.error = message.error;
      if (typeof message.progress === 'number') install.progress = message.progress;
      if (typeof message.maxProgress === 'number') install.maxProgress = message.maxProgress;
      if (typeof message.event === 'string') install.dispatchEvent(new Event(message.event));
    } else if (message.kind === 'event' && typeof message.name === 'string') {
      manager.dispatchEvent(makeEvent(message.name, { id: message.id, needsRestart: false }));
    }
  });

  class AddonManager extends EventTarget {
    get permissionPromptsEnabled() {
      return true;
    }
    get abuseReportPanelEnabled() {
      return false;
    }
    getAddonByID(id) {
      return call('amo.getAddonByID', [String(id)]).then((info) => (info ? new Addon(info) : null));
    }
    createInstall(options) {
      const url = String((options && options.url) || '');
      const hash = options && options.hash ? String(options.hash) : null;
      if (!/^https:\/\//.test(url)) return Promise.reject(new Error('Install URL must be https'));
      const id = nextInstallId++;
      const install = new AddonInstall(id);
      installs.set(id, { install, request: { url, hash } });
      return Promise.resolve(install);
    }
    reportAbuse() {
      return Promise.resolve(false);
    }
    sendAbuseReport() {
      return Promise.reject(new Error('Abuse reports are sent from the website.'));
    }
  }

  const manager = new AddonManager();
  Object.defineProperty(Navigator.prototype, 'mozAddonManager', { get: () => manager, enumerable: true, configurable: true });
})
