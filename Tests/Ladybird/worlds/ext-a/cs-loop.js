// Creates a lot of wrappers, listeners, observers and pending work in the document's isolated world, then the page leaves.
const n = +location.search.slice(3);
const keep = [];
for (let i = 0; i < 200; i++) {
  const d = document.createElement('div');
  d.expando = { i, text: 'x'.repeat(50) };
  d.addEventListener('click', () => keep.push(d));
  document.body.appendChild(d);
  keep.push(d);
}
new MutationObserver(() => { document.createElement('b'); }).observe(document.documentElement, { childList: true, subtree: true });
new ResizeObserver(() => {}).observe(document.body);
fetch('/slow').then(() => { document.createElement('div').setAttribute('late', '1'); });
Promise.resolve().then(() => queueMicrotask(() => Promise.resolve().then(() => document.createElement('p'))));
setTimeout(() => { document.createElement('i'); }, 1000);
fetch('/report?c=lifetime.loop-' + n + '&ok=1');
