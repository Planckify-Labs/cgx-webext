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
  Object.defineProperty(globalThis, 'browser', { value: api.browser, writable: true, configurable: true });
  // The raw bridge is not for extension code. Keep our own reference only.
  try {
    delete globalThis.__cgxBridge;
  } catch (e) {
    /* non-configurable in some embeddings; harmless */
  }
})();
