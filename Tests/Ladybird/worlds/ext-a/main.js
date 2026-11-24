// world: "MAIN": the page's own world. It sees the page's globals and its tampered built-ins, and has no extension API.
// Does not use JSON, addEventListener or querySelector: the page counts calls to those.
const report = (c, ok, d) => fetch('/report?c=' + encodeURIComponent(c) + '&ok=' + (ok ? 1 : 0) + '&d=' + encodeURIComponent(String(d === undefined ? '' : d)));
const hasApi = typeof browser !== 'undefined' || (typeof chrome !== 'undefined' && (chrome.runtime !== undefined || chrome.storage !== undefined));
report('main.world-main', typeof pageGlobal === 'string' && typeof csGlobal === 'undefined' && typeof earlyGlobal === 'undefined' && !hasApi,
       `pageGlobal=${typeof pageGlobal} csGlobal=${typeof csGlobal} api=${hasApi}`);
// The page replaced Array.prototype.push; in its world the replacement is what runs.
report('main.sees-page-tampering', Array.prototype.push.toString().includes('pushCalls'));
