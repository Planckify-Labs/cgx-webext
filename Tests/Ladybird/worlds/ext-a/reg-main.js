const noApi = typeof chrome === 'undefined' || typeof chrome.runtime === 'undefined';
fetch('/report?c=register.main-world&ok=' + (typeof page2Global === 'string' && noApi ? 1 : 0) + '&d=' + typeof page2Global + '/' + noApi);
