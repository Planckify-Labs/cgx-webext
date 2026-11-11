chrome.commands.getAll().then((commands) => {
  document.getElementById('commands').textContent = commands.map((c) => `${c.name}: ${c.shortcut || '(none)'}`).join(' · ');
});
