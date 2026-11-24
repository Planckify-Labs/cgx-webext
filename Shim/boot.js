/*
 * boot.js - runs first in every extension page (background page, popup, options page, ...).
 *
 * The core's resource server inserts <script src="/__cgx__/shim.js"> and <script src="/__cgx__/boot.js"> at the top of every
 * extension HTML page it serves. The browser provides `__cgxBridge` natively on documents of extension origins only:
 *
 *   __cgxBridge.contextInit()        -> JSON string for createApi() (registers this document as a context; synchronous)
 *   __cgxBridge.call(name, argsJson) -> Promise<string> with the JSON result, rejected with an Error on API errors
 *   __cgxBridge.callSync(name, argsJson) -> JSON string result (for the few functions that are synchronous in real browsers)
 *   __cgxBridge.setMessageHandler(fn(jsonString))   envelopes and events pushed by the browser
 */
(function () {
  'use strict';
  const bridge = globalThis.__cgxBridge;
  const shim = globalThis.__webextShim;
  if (!bridge || !shim) return;

  const init = JSON.parse(bridge.contextInit());
  const api = shim.createApi(init, {
    call: (name, args) => bridge.call(name, JSON.stringify(args === undefined ? [] : args)).then((text) => (text === '' ? undefined : JSON.parse(text))),
    callSync: (name, args) => {
      const text = bridge.callSync(name, JSON.stringify(args === undefined ? [] : args));
      return text === '' ? undefined : JSON.parse(text);
    },
  });

  // Host -> page: { kind: 'envelope', envelope } from the message bus, or { kind: 'event', name, args } from the core.
  bridge.setMessageHandler((text) => {
    const message = JSON.parse(text);
    if (message.kind === 'event') api.dispatchEvent(message.name, message.args);
    else if (message.kind === 'envelope') api.handleHostMessage(message.envelope);
  });

  Object.defineProperty(globalThis, 'chrome', { value: api.chrome, writable: true, configurable: true });
  // Firefox gives extensions both namespaces; Chrome only `chrome`, and extensions test `typeof browser` to tell them apart.
  if (init.dialect === 'firefox') Object.defineProperty(globalThis, 'browser', { value: api.browser, writable: true, configurable: true });
  // A background service worker runs as a page here, so it lacks the worker's importScripts. Classic (non-module) workers
  // get one that loads the extension's own files synchronously; relative URLs resolve against the worker script, as in a
  // worker. Module workers have no importScripts in Chrome either.
  const background = (init.manifest && init.manifest.background) || {};
  if (init.contextKind === 'background' && init.dialect !== 'firefox' && background.service_worker && background.type !== 'module'
      && typeof bridge.importScript === 'function' && typeof globalThis.importScripts !== 'function') {
    const workerUrl = new URL(String(background.service_worker).replace(/^\/+/, ''), init.baseUrl);
    Object.defineProperty(globalThis, 'importScripts', {
      value: function importScripts(...urls) {
        for (const url of urls) bridge.importScript(new URL(String(url), workerUrl).href);
      },
      writable: true,
      configurable: true,
    });
  }

  // An extension's popup sizes itself to its content, as in Chrome and Firefox, and closes with window.close().
  if (init.isPopup) {
    const measure = () => {
      const root = document.documentElement;
      const body = document.body;
      if (!root) return null;
      // The width the content wants: lay the root out at its max-content width for a moment.
      const old = root.style.getPropertyValue('width');
      const oldPriority = root.style.getPropertyPriority('width');
      root.style.setProperty('width', 'max-content', 'important');
      let width = root.getBoundingClientRect().width;
      if (body) {
        const style = getComputedStyle(body);
        width = Math.max(width, body.getBoundingClientRect().width + (parseFloat(style.marginLeft) || 0) + (parseFloat(style.marginRight) || 0));
      }
      if (old) root.style.setProperty('width', old, oldPriority);
      else root.style.removeProperty('width');
      const height = Math.max(root.scrollHeight, body ? body.scrollHeight : 0, root.getBoundingClientRect().height);
      return [Math.ceil(width), Math.ceil(height)];
    };
    let last = '';
    let scheduled = false;
    const report = () => {
      if (scheduled) return;
      scheduled = true;
      // Not requestAnimationFrame: the browser keeps the popup hidden until it is ready, and hidden pages get no frames.
      setTimeout(() => {
        scheduled = false;
        const size = measure();
        if (!size) return;
        const key = size.join('x');
        if (key === last) return;
        last = key;
        bridge.call('__ui.popupSize', JSON.stringify(size)).catch(() => {});
      }, 0);
    };
    const observe = () => {
      report();
      if (typeof ResizeObserver === 'function') {
        const observer = new ResizeObserver(report);
        observer.observe(document.documentElement);
        if (document.body) observer.observe(document.body);
      }
      if (typeof MutationObserver === 'function') new MutationObserver(report).observe(document.documentElement, { childList: true, subtree: true, characterData: true });
    };
    if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', observe, { once: true });
    else observe();
    // Chrome shows a popup once its page has loaded; the browser waits for this.
    addEventListener('load', () => {
      report();
      setTimeout(() => bridge.call('__ui.popupReady', '[]').catch(() => {}), 0);
    });
    globalThis.close = () => { bridge.call('__ui.closePopup', '[]').catch(() => {}); };
  }

  // The raw bridge is not for extension code. Keep our own reference only.
  try {
    delete globalThis.__cgxBridge;
  } catch (e) {
    /* non-configurable in some embeddings; harmless */
  }
})();
