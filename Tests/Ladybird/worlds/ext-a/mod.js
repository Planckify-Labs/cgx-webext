// A module imported from a content script (in the isolated world: it sees `chrome`) or by the page (in the page's world).
import { two } from './mod2.js';
export const hasChrome = typeof chrome !== 'undefined' && chrome.runtime !== undefined && typeof chrome.runtime.id === 'string';
export const pageGlobalVisible = typeof pageGlobal === 'string';
export const imported = two;
globalThis.modGlobal = 'module';
