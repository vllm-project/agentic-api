// Apply a saved preference before the page paints; CSS handles System mode.
export const themeInitScript = `(() => {
  let theme = 'system';
  try {
    const saved = localStorage.getItem('agentic-api-theme');
    if (saved === 'light' || saved === 'dark') theme = saved;
  } catch {}
  document.documentElement.dataset.theme = theme;
})();`;

export function readTheme() {
  const theme = document.documentElement.dataset.theme;
  return theme === 'light' || theme === 'dark' ? theme : 'system';
}

/** @param {() => void} onChange */
export function subscribeTheme(onChange) {
  window.addEventListener('agentic-api-theme-change', onChange);
  return () => window.removeEventListener('agentic-api-theme-change', onChange);
}

/** @param {'light' | 'dark' | 'system'} theme */
export function saveTheme(theme) {
  document.documentElement.dataset.theme = theme;
  try {
    if (theme === 'system') localStorage.removeItem('agentic-api-theme');
    else localStorage.setItem('agentic-api-theme', theme);
  } catch {
    // The choice still works for this page when browser storage is unavailable.
  }
  window.dispatchEvent(new Event('agentic-api-theme-change'));
}
