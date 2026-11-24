// A second extension in the same document has a world of its own: it sees neither the page's nor the first extension's globals.
window.extBGlobal = 1;
const ok = typeof csGlobal === 'undefined' && typeof earlyGlobal === 'undefined' && typeof pageGlobal === 'undefined' && typeof chrome.runtime.id === 'string' && el_check();
function el_check() { const el = document.getElementById('shared'); return el.pageExpando === undefined && el.csExpando === undefined; }
fetch('/report?c=ext.b-has-its-own-world&ok=' + (ok ? 1 : 0) + '&d=' + typeof csGlobal);
