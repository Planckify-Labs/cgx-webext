/*
 * webext-shim.js - builds the `chrome` and `browser` objects for an extension context.
 *
 * It is plain ES2020 with no dependencies and runs unmodified in a background page, extension page or content script.
 * The embedding browser supplies two things:
 *
 *   init    The JSON from Dispatcher::init_data_for(extensionId): { id, baseUrl, manifest, uiLanguage, surface,
 *           syncFunctions }. `surface` lists only the APIs this extension may see, so a missing permission yields
 *           `undefined` for the namespace, exactly like a real browser.
 *   bridge  { call(name, args) -> Promise<result>, callSync(name, args) -> result }
 *           `call` carries an API call to Dispatcher::dispatch and rejects with an Error whose message is the API
 *           error. `callSync` is only used for the few functions that are synchronous in real browsers
 *           (init.syncFunctions, e.g. i18n.getMessage).
 *
 * The host forwards events with api.dispatchEvent(name, args). Messaging between the contexts of an extension (ports and
 * one-shot messages) is decided by the core's message bus; the host only registers contexts (init.contextId) and hands the
 * envelopes the core produces to api.handleHostMessage(envelope), in order, per context.
 *
 * Dialects:
 *   browser.*  Promise-based (Firefox style). Callbacks are not accepted.
 *   chrome.*   Callback-based. With a trailing callback, errors are reported through chrome.runtime.lastError. Without
 *              a callback it returns a Promise in Manifest V3, as Chrome does, and nothing in Manifest V2.
 */
(function (root, factory) {
  const api = factory();
  if (typeof module === 'object' && module.exports) module.exports = api;
  else root.__webextShim = api;
})(typeof globalThis !== 'undefined' ? globalThis : this, function () {
  'use strict';

  // Functions the shim answers locally, with no round trip.
  const LOCAL = {
    'runtime.getManifest': (ctx) => () => ctx.init.manifest,
    'runtime.getURL': (ctx) => (path) => ctx.init.baseUrl + String(path).replace(/^\/+/, ''),
    'extension.getURL': (ctx) => (path) => ctx.init.baseUrl + String(path).replace(/^\/+/, ''),
    'i18n.getUILanguage': (ctx) => () => ctx.init.uiLanguage,
  };

  // Constant properties. The storage quotas must equal the ones the core enforces (checked by a Rust test).
  const CONSTANTS = {
    'tabs.TAB_ID_NONE': -1,
    // Extensions do not run in private windows yet, so no context is an incognito one.
    'extension.inIncognitoContext': false,
    'windows.WINDOW_ID_NONE': -1,
    'windows.WINDOW_ID_CURRENT': -2,
    'storage.sync.QUOTA_BYTES': 102400,
    'storage.sync.QUOTA_BYTES_PER_ITEM': 8192,
    'storage.sync.MAX_ITEMS': 512,
    'storage.sync.MAX_WRITE_OPERATIONS_PER_HOUR': 1800,
    'storage.sync.MAX_WRITE_OPERATIONS_PER_MINUTE': 120,
    'storage.local.QUOTA_BYTES': 10485760,
    'storage.session.QUOTA_BYTES': 10485760,
    // The limits the core enforces (crates/webext/src/dnr.rs).
    'declarativeNetRequest.MAX_NUMBER_OF_DYNAMIC_RULES': 30000,
    'declarativeNetRequest.MAX_NUMBER_OF_UNSAFE_DYNAMIC_RULES': 5000,
    'declarativeNetRequest.MAX_NUMBER_OF_SESSION_RULES': 5000,
    'declarativeNetRequest.MAX_NUMBER_OF_UNSAFE_SESSION_RULES': 5000,
    'declarativeNetRequest.MAX_NUMBER_OF_REGEX_RULES': 1000,
    'declarativeNetRequest.MAX_NUMBER_OF_STATIC_RULESETS': 100,
    'declarativeNetRequest.MAX_NUMBER_OF_ENABLED_STATIC_RULESETS': 50,
    'declarativeNetRequest.MAX_NUMBER_OF_DISABLED_STATIC_RULES': 5000,
    'declarativeNetRequest.GUARANTEED_MINIMUM_STATIC_RULES': 30000,
    'declarativeNetRequest.GETMATCHEDRULES_QUOTA_INTERVAL': 10,
    'declarativeNetRequest.MAX_GETMATCHEDRULES_CALLS_PER_INTERVAL': 20,
    'declarativeNetRequest.DYNAMIC_RULESET_ID': '_dynamic',
    'declarativeNetRequest.SESSION_RULESET_ID': '_session',
    'webRequest.MAX_HANDLER_BEHAVIOR_CHANGED_CALLS_PER_10_MINUTES': 20,
  };

  // sendMessage results arrive as { response } when a listener answered and {} when listeners existed but none did, so that
  // "answered with null" and "no answer" stay distinguishable. Browsers resolve the latter with undefined.
  function unwrapMessageResult(result) {
    return result && typeof result === 'object' && Object.prototype.hasOwnProperty.call(result, 'response') ? result.response : undefined;
  }

  function wrapSendMessage(call) {
    return function (...args) {
      const last = args[args.length - 1];
      if (typeof last === 'function') {
        args[args.length - 1] = (result) => last(unwrapMessageResult(result));
        return call(...args);
      }
      const returned = call(...args);
      return returned && typeof returned.then === 'function' ? returned.then(unwrapMessageResult) : returned;
    };
  }

  // ---- Arguments that do not survive JSON ---------------------------------------------------------------------------

  function base64(bytes) {
    let binary = '';
    for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode.apply(null, Array.prototype.slice.call(bytes, i, i + 0x8000));
    if (typeof btoa === 'function') return btoa(binary);
    return Buffer.from(binary, 'binary').toString('base64'); // Node, for tests.
  }

  function isImageData(v) {
    return !!v && typeof v === 'object' && typeof v.width === 'number' && typeof v.height === 'number' && v.data && typeof v.data.length === 'number';
  }

  /** ImageData objects (setIcon) become { width, height, data: base64 RGBA }. */
  function encodeIconDetails(details) {
    if (!details || typeof details !== 'object' || !details.imageData) return details;
    const out = Object.assign({}, details);
    const one = (img) => ({ width: img.width, height: img.height, data: base64(img.data) });
    if (isImageData(details.imageData)) out.imageData = one(details.imageData);
    else {
      out.imageData = {};
      for (const [size, img] of Object.entries(details.imageData)) if (isImageData(img)) out.imageData[size] = one(img);
    }
    return out;
  }

  /** scripting.executeScript({ func }) sends the function's source; it runs in the page, not here. */
  function encodeInjection(injection) {
    if (!injection || typeof injection !== 'object') return injection;
    const func = injection.func || injection.function;
    if (typeof func !== 'function') return injection;
    const out = Object.assign({}, injection);
    delete out.func;
    delete out.function;
    out.__funcSource = Function.prototype.toString.call(func);
    return out;
  }

  const ARGUMENT_ENCODERS = {
    'action.setIcon': (args) => [encodeIconDetails(args[0])].concat(args.slice(1)),
    'pageAction.setIcon': (args) => [encodeIconDetails(args[0])].concat(args.slice(1)),
    'scripting.executeScript': (args) => [encodeInjection(args[0])].concat(args.slice(1)),
  };

  function trimTrailingUndefined(args) {
    let n = args.length;
    while (n > 0 && args[n - 1] === undefined) n--;
    return args.slice(0, n);
  }

  function createEvent(ctx, name) {
    const listeners = new Set();
    // Events that take a filter as addListener's second argument (webNavigation): callback -> filter.
    const filters = new Map();
    const event = {
      addListener(cb, filter) {
        if (typeof cb !== 'function') throw new TypeError(`${name}.addListener: listener must be a function`);
        listeners.add(cb);
        if (filter && typeof filter === 'object') filters.set(cb, filter);
      },
      removeListener(cb) { listeners.delete(cb); filters.delete(cb); },
      hasListener(cb) { return listeners.has(cb); },
      hasListeners() { return listeners.size > 0; },
    };
    ctx.events.set(name, { listeners, filters });
    return event;
  }

  /**
   * webRequest events take a filter and options, and blocking listeners answer. The core keeps the filters (it sees every
   * request and must not wake contexts for nothing), so each listener is registered there under a number.
   */
  function createWebRequestEvent(ctx, name, log) {
    const entries = [];
    return {
      addListener(cb, filter, extraInfoSpec) {
        if (typeof cb !== 'function') throw new TypeError(`${name}.addListener: listener must be a function`);
        if (!filter || !Array.isArray(filter.urls)) throw new TypeError(`${name}.addListener: the filter needs a 'urls' list`);
        const specs = Array.isArray(extraInfoSpec) ? extraInfoSpec.slice() : [];
        const id = ++ctx.listenerSeq;
        const entry = { cb, id, async: specs.includes('asyncBlocking') };
        entries.push(entry);
        ctx.requestListeners.set(id, entry);
        ctx.bridge.call('__webRequest.addListener', [name, id, filter, specs]).catch((error) => {
          entries.splice(entries.indexOf(entry), 1);
          ctx.requestListeners.delete(id);
          log.error(`${name}.addListener: ${error && error.message ? error.message : error}`);
        });
      },
      removeListener(cb) {
        const i = entries.findIndex((e) => e.cb === cb);
        if (i < 0) return;
        const [entry] = entries.splice(i, 1);
        ctx.requestListeners.delete(entry.id);
        ctx.bridge.call('__webRequest.removeListener', [name, entry.id]).catch(() => {});
      },
      hasListener(cb) { return entries.some((e) => e.cb === cb); },
      hasListeners() { return entries.length > 0; },
    };
  }

  /** events.UrlFilter, as webNavigation's listener filters use it. */
  function urlFilterMatches(filter, href) {
    let url;
    try { url = new URL(href); } catch (e) { return false; }
    const host = url.hostname;
    const dotted = '.' + host;
    const query = url.search.replace(/^\?/, '');
    const noFragment = href.split('#')[0];
    const checks = {
      hostContains: (v) => dotted.includes(v),
      hostEquals: (v) => host === v,
      hostPrefix: (v) => host.startsWith(v),
      hostSuffix: (v) => dotted.endsWith(v) || host.endsWith(v),
      pathContains: (v) => url.pathname.includes(v),
      pathEquals: (v) => url.pathname === v,
      pathPrefix: (v) => url.pathname.startsWith(v),
      pathSuffix: (v) => url.pathname.endsWith(v),
      queryContains: (v) => query.includes(v),
      queryEquals: (v) => query === v,
      queryPrefix: (v) => query.startsWith(v),
      querySuffix: (v) => query.endsWith(v),
      urlContains: (v) => noFragment.includes(v),
      urlEquals: (v) => noFragment === v,
      urlPrefix: (v) => noFragment.startsWith(v),
      urlSuffix: (v) => noFragment.endsWith(v),
      urlMatches: (v) => new RegExp(v).test(noFragment),
      originAndPathMatches: (v) => new RegExp(v).test(url.origin + url.pathname),
      schemes: (v) => Array.isArray(v) && v.includes(url.protocol.replace(/:$/, '')),
      ports: (v) => {
        const defaults = { 'http:': 80, 'https:': 443, 'ws:': 80, 'wss:': 443, 'ftp:': 21 };
        const port = url.port ? Number(url.port) : defaults[url.protocol];
        return Array.isArray(v) && v.some((p) => (Array.isArray(p) ? port >= p[0] && port <= p[1] : port === p));
      },
    };
    for (const [key, value] of Object.entries(filter || {})) {
      const check = checks[key];
      try {
        if (check && !check(value)) return false;
      } catch (e) {
        return false;
      }
    }
    return true;
  }

  /** Whether an event with these arguments passes the filter a listener gave to addListener. */
  function passesFilter(name, filter, args) {
    if (!filter || !name.startsWith('webNavigation.')) return true;
    if (!Array.isArray(filter.url) || filter.url.length === 0) return true;
    const href = args && args[0] && args[0].url;
    return typeof href === 'string' && filter.url.some((f) => urlFilterMatches(f, href));
  }

  function createLocalEvent() {
    const listeners = new Set();
    return {
      listeners,
      api: {
        addListener(cb) {
          if (typeof cb !== 'function') throw new TypeError('listener must be a function');
          listeners.add(cb);
        },
        removeListener(cb) { listeners.delete(cb); },
        hasListener(cb) { return listeners.has(cb); },
        hasListeners() { return listeners.size > 0; },
      },
    };
  }

  /** Creates { browser, chrome, dispatchEvent, handleHostMessage, deliverMessage } for one extension context. */
  function createApi(init, bridge, options) {
    const opts = options || {};
    const log = opts.console || (typeof console !== 'undefined' ? console : { error() {}, warn() {} });
    const ctx = { init, bridge, events: new Map(), lastError: undefined, ports: new Map(), portSeq: 0, menuHandlers: new Map(), menuSeq: 0, requestListeners: new Map(), listenerSeq: 0 };
    const syncFunctions = new Set(init.syncFunctions || []);
    const mv3 = init.manifest && init.manifest.manifest_version === 3;

    function makeTree(style) {
      const tree = {};
      for (const [ns, members] of Object.entries(init.surface)) {
        const nsObj = {};
        const nest = (path, leaf, value) => {
          const parts = path.split('.');
          let cur = nsObj;
          for (const p of parts.slice(0, -1)) cur = cur[p] || (cur[p] = {});
          cur[parts[parts.length - 1]] = value;
          return leaf;
        };

        for (const fn of members.functions || []) {
          const full = `${ns}.${fn}`;
          nest(fn, null, makeFunction(style, full));
        }
        for (const ev of members.events || []) {
          const full = `${ns}.${ev}`;
          // One event object per name, shared by browser.* and chrome.*.
          let event = ctx.sharedEvents && ctx.sharedEvents.get(full);
          if (!event) {
            event = ns === 'webRequest' ? createWebRequestEvent(ctx, full, log) : createEvent(ctx, full);
            (ctx.sharedEvents || (ctx.sharedEvents = new Map())).set(full, event);
          }
          nest(ev, null, event);
        }
        tree[ns] = nsObj;
      }

      // Properties that need real values.
      if (tree.runtime) {
        Object.defineProperty(tree.runtime, 'id', { value: init.id, enumerable: true });
        Object.defineProperty(tree.runtime, 'lastError', { get: () => ctx.lastError, enumerable: true });
      }
      // Constants, for the namespaces this extension can see.
      for (const [full, value] of Object.entries(CONSTANTS)) {
        const [ns, ...path] = full.split('.');
        if (!tree[ns]) continue;
        let cur = tree[ns];
        for (const p of path.slice(0, -1)) cur = cur[p] || (cur[p] = {});
        Object.defineProperty(cur, path[path.length - 1], { value, enumerable: true });
      }
      // Browser-specific aliases.
      if (init.surface.action && !tree.browserAction) tree.browserAction = tree.action;
      if (init.surface.contextMenus && !tree.menus) tree.menus = tree.contextMenus;
      return tree;
    }

    function menuKey(id) {
      return typeof id === 'number' ? `n:${id}` : `s:${id}`;
    }

    /**
     * contextMenus.create returns the item's id at once, and takes an optional callback in both namespaces (Firefox's
     * menus.create does too). Items created without an id get one here. `onclick` stays in this context and runs when the
     * item is clicked.
     */
    function menuCreate(full) {
      return function (createProperties, callback) {
        const props = Object.assign({}, createProperties || {});
        if (props.id === undefined || props.id === null) {
          // Unique across the extension's contexts: each context numbers from its own block.
          props.id = (Number(ctx.init.contextId) || 0) * 100000 + ++ctx.menuSeq;
          props.__generatedId = true;
        }
        const onclick = typeof props.onclick === 'function' ? props.onclick : null;
        delete props.onclick;
        if (onclick) ctx.menuHandlers.set(menuKey(props.id), onclick);
        bridge.call(full, [props]).then(
          () => {
            if (typeof callback === 'function') safely(callback, []);
          },
          (error) => {
            if (onclick) ctx.menuHandlers.delete(menuKey(props.id));
            const message = error && error.message ? error.message : String(error);
            if (typeof callback === 'function') {
              ctx.lastError = { message };
              try { safely(callback, []); } finally { ctx.lastError = undefined; }
            } else {
              log.error(`Unchecked runtime.lastError: ${message}`);
            }
          }
        );
        return props.id;
      };
    }

    // Firefox's contentScripts.register resolves to a RegisteredContentScript.
    function contentScriptsRegister(options) {
      return bridge.call('contentScripts.register', [options]).then((result) => ({
        unregister: () => bridge.call('__contentScripts.unregister', [result && result.id]).then(() => undefined),
      }));
    }

    function makeFunction(style, full) {
      if (full === 'contextMenus.create') return menuCreate(full);
      if (full === 'contentScripts.register') return contentScriptsRegister;
      if (full === 'runtime.connect') return (...args) => connect('runtime', args);
      if (full === 'tabs.connect') return (...args) => connect('tabs', args);
      if (LOCAL[full]) return LOCAL[full](ctx);
      if (syncFunctions.has(full)) {
        return (...args) => bridge.callSync(full, trimTrailingUndefined(args));
      }
      const encode = ARGUMENT_ENCODERS[full];
      const generic = function (...rawArgs) {
        const args = encode ? encode(rawArgs.slice()) : rawArgs;
        if (style === 'browser') {
          if (typeof args[args.length - 1] === 'function') {
            throw new TypeError(`browser.${full}: callbacks are not supported; use the returned Promise`);
          }
          return bridge.call(full, trimTrailingUndefined(args));
        }
        // chrome.* style.
        const last = args[args.length - 1];
        if (typeof last === 'function') {
          const callback = args.pop();
          bridge.call(full, trimTrailingUndefined(args)).then(
            (result) => {
              ctx.lastError = undefined;
              safely(callback, [result]);
            },
            (error) => {
              ctx.lastError = { message: error && error.message ? error.message : String(error) };
              try { safely(callback, []); } finally { ctx.lastError = undefined; }
            }
          );
          return undefined;
        }
        const promise = bridge.call(full, trimTrailingUndefined(args));
        if (mv3) return promise;
        promise.catch(() => {});
        return undefined;
      };
      return full === 'runtime.sendMessage' || full === 'tabs.sendMessage' ? wrapSendMessage(generic) : generic;
    }

    function safely(fn, args) {
      try { return fn(...args); } catch (e) { log.error('Uncaught error in extension listener:', e); }
      return undefined;
    }

    // ---- Ports ---------------------------------------------------------------------------------------------------------

    function makePort(portId, name, sender) {
      const onMessage = createLocalEvent();
      const onDisconnect = createLocalEvent();
      const entry = { onMessage, onDisconnect, disconnected: false };
      const port = {
        name,
        onMessage: onMessage.api,
        onDisconnect: onDisconnect.api,
        postMessage(message) {
          if (entry.disconnected) throw new Error('Attempting to use a disconnected port object');
          bridge.call('__bus.post', [portId, message === undefined ? null : message]).catch(() => {});
        },
        disconnect() {
          if (entry.disconnected) return;
          entry.disconnected = true;
          ctx.ports.delete(portId);
          bridge.call('__bus.disconnect', [portId]).catch(() => {});
        },
      };
      if (sender) port.sender = sender; // Only the receiving end has a sender.
      entry.port = port;
      ctx.ports.set(portId, entry);
      return port;
    }

    function disconnectPort(portId, errorMessage) {
      const entry = ctx.ports.get(portId);
      if (!entry || entry.disconnected) return;
      entry.disconnected = true;
      ctx.ports.delete(portId);
      ctx.lastError = errorMessage ? { message: errorMessage } : undefined;
      try {
        for (const listener of [...entry.onDisconnect.listeners]) safely(listener, [entry.port]);
      } finally {
        ctx.lastError = undefined;
      }
    }

    /** runtime.connect([extensionId], [connectInfo]) and tabs.connect(tabId, [connectInfo]). Returns a Port at once. */
    function connect(kind, args) {
      if (ctx.init.contextId == null) throw new Error('Ports are not available: this context was not registered with the host');
      let first;
      let rest = args;
      if (kind === 'runtime') {
        first = typeof args[0] === 'string' ? args[0] : null;
        rest = typeof args[0] === 'string' ? args.slice(1) : args;
      } else {
        first = args[0];
        rest = args.slice(1);
      }
      const connectInfo = rest.find((a) => a && typeof a === 'object') || {};
      const portId = `${ctx.init.contextId}.${++ctx.portSeq}`;
      const name = typeof connectInfo.name === 'string' ? connectInfo.name : '';
      const port = makePort(portId, name, undefined);
      const call = kind === 'runtime' ? 'runtime.connect' : 'tabs.connect';
      bridge.call(call, [first, connectInfo, portId]).catch((error) => disconnectPort(portId, error && error.message ? error.message : String(error)));
      return port;
    }

    /** Handles an envelope produced by the core's message bus. */
    function handleHostMessage(envelope) {
      switch (envelope && envelope.type) {
        case 'connect': {
          const event = ctx.events.get('runtime.onConnect');
          const listening = !!event && event.listeners.size > 0;
          if (listening) {
            const port = makePort(envelope.portId, envelope.name || '', envelope.sender);
            for (const listener of [...event.listeners]) safely(listener, [port]);
          }
          bridge.call('__bus.accept', [envelope.portId, listening]).catch(() => {});
          break;
        }
        case 'port_message': {
          const entry = ctx.ports.get(envelope.portId);
          if (entry && !entry.disconnected) for (const listener of [...entry.onMessage.listeners]) safely(listener, [envelope.message, entry.port]);
          break;
        }
        case 'port_disconnect':
          disconnectPort(envelope.portId, envelope.error);
          break;
        case 'message':
          deliverMessage(envelope.message, envelope.sender).then((r) =>
            bridge
              .call('__bus.reply', [envelope.requestId, r.hasListeners, r.handled, r.response === undefined ? null : r.response])
              .catch(() => {})
          );
          break;
        case 'webRequest': {
          // A request the core matched to one of this context's listeners. Blocking listeners answer with a
          // BlockingResponse: returned, promised (Firefox), or passed to the callback (Chrome's asyncBlocking).
          const entry = ctx.requestListeners.get(envelope.listenerId);
          const wanted = envelope.replyTo !== null && envelope.replyTo !== undefined;
          let answered = false;
          const answer = (response) => {
            if (!wanted || answered) return;
            answered = true;
            const value = response && typeof response === 'object' ? response : null;
            bridge.call('__webRequest.reply', [envelope.replyTo, envelope.listenerId, value]).catch(() => {});
          };
          if (!entry) {
            answer(null);
            break;
          }
          let result;
          try {
            result = entry.async ? entry.cb(envelope.details, answer) : entry.cb(envelope.details);
          } catch (e) {
            log.error('Uncaught error in webRequest listener:', e);
          }
          if (result && typeof result.then === 'function') result.then(answer, () => answer(null));
          else if (!entry.async || result !== undefined) answer(result);
          break;
        }
        default:
          log.warn('Unknown host message', envelope);
      }
    }

    /**
     * Delivers a runtime message to onMessage listeners and resolves with { hasListeners, handled, response }.
     * A listener answers by calling sendResponse, by returning true (then calling sendResponse later), or by returning a
     * Promise (Firefox style). The first answer wins.
     */
    function deliverMessage(message, sender) {
      const entry = ctx.events.get('runtime.onMessage');
      const listeners = entry ? [...entry.listeners] : [];
      return new Promise((resolve) => {
        const hasListeners = listeners.length > 0;
        let settled = false;
        let pending = 0;
        const finish = (response) => {
          if (settled) return;
          settled = true;
          resolve({ hasListeners, handled: true, response });
        };
        const checkDone = () => { if (!settled && pending === 0) resolve({ hasListeners, handled: false, response: undefined }); };

        for (const listener of listeners) {
          let result;
          try {
            result = listener(message, sender, (response) => finish(response));
          } catch (e) {
            log.error('Uncaught error in runtime.onMessage listener:', e);
            continue;
          }
          if (result === true) {
            pending++; // Will call sendResponse asynchronously.
          } else if (result && typeof result.then === 'function') {
            pending++;
            result.then(
              (value) => { pending--; if (value !== undefined) finish(value); else checkDone(); },
              () => { pending--; checkDone(); }
            );
          }
          if (settled) return;
        }
        if (pending === 0) checkDone();
      });
    }

    const browser = makeTree('browser');
    const chrome = makeTree('chrome');

    return {
      browser,
      chrome,

      /** Delivers a browser event to this context's listeners. Returns the number of listeners called. */
      dispatchEvent(name, args) {
        if (name === 'contextMenus.onClicked' && args && args[0]) {
          const onclick = ctx.menuHandlers.get(menuKey(args[0].menuItemId));
          if (onclick) safely(onclick, args);
        }
        const entry = ctx.events.get(name);
        if (!entry) return 0;
        let called = 0;
        for (const listener of [...entry.listeners]) {
          if (entry.filters && !passesFilter(name, entry.filters.get(listener), args)) continue;
          called++;
          safely(listener, args || []);
        }
        return called;
      },

      handleHostMessage,
      deliverMessage,
    };
  }

  return { createApi };
});
