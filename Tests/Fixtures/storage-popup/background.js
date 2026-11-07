const api = typeof browser !== 'undefined' ? browser : chrome;

api.runtime.onInstalled.addListener(async () => {
  await api.storage.local.set({ visits: 0, installedAt: Date.now() });
  api.alarms.create('heartbeat', { periodInMinutes: 1 });
});

api.alarms.onAlarm.addListener(async (alarm) => {
  const { visits } = await api.storage.local.get({ visits: 0 });
  await api.storage.local.set({ visits: visits + 1 });
});

api.storage.onChanged.addListener((changes, area) => console.log(area, changes));
api.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  api.storage.local.get('visits').then(sendResponse);
  return true;
});
console.log(api.i18n.getMessage('extName'), api.runtime.getURL('popup.html'));
