// document_idle: shows a banner, then asks the background page for the visit count.
(function () {
  const banner = document.createElement('div');
  banner.id = 'cgx-webext-banner';
  banner.style.cssText = 'position:fixed;left:0;right:0;bottom:0;z-index:2147483647;padding:10px 14px;' +
    'font:14px/1.4 sans-serif;color:#fff;background:#1d6b3a;box-shadow:0 -2px 6px rgba(0,0,0,.3)';
  banner.textContent = 'cgx-webext: content script running (document_start ran ' +
    (cgxStartedBeforeDom ? 'before' : 'after') + ' the DOM). Asking the background page...';
  (document.body || document.documentElement).appendChild(banner);

  chrome.runtime.sendMessage({ type: 'visit' }).then(
    (reply) => {
      banner.textContent = 'cgx-webext: content script + background page OK. Pages seen: ' + reply.visits +
        ' (stored in chrome.storage.local since ' + reply.installedAt + ').';
    },
    (error) => {
      banner.style.background = '#8a1c1c';
      banner.textContent = 'cgx-webext: content script ran, but messaging failed: ' + error.message;
    });
})();
