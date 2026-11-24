// document_start, isolated world: the document element exists, and the globals set here are still there at document_idle.
window.earlyGlobal = 'early';
fetch('/report?c=iso.document-start&ok=' + (document.documentElement ? 1 : 0));
