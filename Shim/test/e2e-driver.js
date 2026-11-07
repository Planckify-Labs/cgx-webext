'use strict';
// A tiny "browser" for end-to-end tests: hosts one shim instance per extension context and talks to a test harness (the
// Rust tests) over JSON lines on stdin/stdout. It stands in for the renderer processes of a real browser.
//
// Harness -> driver:
//   {op:"create",  ctx, init}              build chrome.*/browser.* for a context from Dispatcher::init_data_for_context
//   {op:"deliver", ctx, msg}               an envelope from the core's message bus  -> api.handleHostMessage(msg)
//   {op:"event",   ctx, name, args}        a browser event                           -> api.dispatchEvent(name, args)
//   {op:"eval",    ctx, id, code}          run `code` (async function body with chrome, browser, g) and report the result
//   {op:"reply",   seq, ok, value|error}   the answer to a {op:"call"} the driver made
//   {op:"flush",   id}                     report back once pending promises and timers have run
//   {op:"destroy", ctx}                    forget a context
// Driver -> harness:
//   {op:"call", ctx, seq, function, args}  the shim's bridge.call()
//   {op:"result", id, ok, value|error}     the outcome of an eval
//   {op:"flushed", id}

const readline = require('node:readline');
const { createApi } = require('../webext-shim.js');

const contexts = new Map();
const pending = new Map();
let seq = 0;

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}

function makeBridge(ctx) {
  return {
    call(name, args) {
      return new Promise((resolve, reject) => {
        const id = ++seq;
        pending.set(id, { resolve, reject });
        send({ op: 'call', ctx, seq: id, function: name, args });
      });
    },
    callSync() {
      throw new Error('synchronous calls are not supported by the e2e driver');
    },
  };
}

const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;

async function handle(cmd) {
  switch (cmd.op) {
    case 'create': {
      const api = createApi(cmd.init, makeBridge(cmd.ctx), { console: { error: (...a) => send({ op: 'log', level: 'error', text: a.map(String).join(' ') }), warn() {} } });
      contexts.set(cmd.ctx, { api, g: {} });
      break;
    }
    case 'destroy':
      contexts.delete(cmd.ctx);
      break;
    case 'deliver':
      contexts.get(cmd.ctx).api.handleHostMessage(cmd.msg);
      break;
    case 'event':
      contexts.get(cmd.ctx).api.dispatchEvent(cmd.name, cmd.args);
      break;
    case 'reply': {
      const p = pending.get(cmd.seq);
      pending.delete(cmd.seq);
      if (cmd.ok) p.resolve(cmd.value);
      else p.reject(new Error(cmd.error));
      break;
    }
    case 'eval': {
      const c = contexts.get(cmd.ctx);
      try {
        const value = await new AsyncFunction('chrome', 'browser', 'g', cmd.code)(c.api.chrome, c.api.browser, c.g);
        send({ op: 'result', id: cmd.id, ok: true, value: value === undefined ? null : value });
      } catch (e) {
        send({ op: 'result', id: cmd.id, ok: false, error: String(e && e.message ? e.message : e) });
      }
      break;
    }
    case 'flush':
      // Let promise chains and zero-delay timers drain; real browsers deliver IPC between these turns.
      for (let i = 0; i < 5; i++) await new Promise((r) => setImmediate(r));
      send({ op: 'flushed', id: cmd.id });
      break;
    default:
      send({ op: 'log', level: 'error', text: `unknown op ${cmd.op}` });
  }
}

// Commands are handled one at a time, in order, so that `deliver` envelopes keep their order.
let chain = Promise.resolve();
readline.createInterface({ input: process.stdin }).on('line', (line) => {
  if (!line.trim()) return;
  const cmd = JSON.parse(line);
  // `reply` must not wait behind a long-running eval that is itself waiting for this reply.
  if (cmd.op === 'reply' || cmd.op === 'deliver' || cmd.op === 'event') handle(cmd).catch((e) => send({ op: 'log', level: 'error', text: String(e.stack || e) }));
  else chain = chain.then(() => handle(cmd)).catch((e) => send({ op: 'log', level: 'error', text: String(e.stack || e) }));
});
