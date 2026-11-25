// Внедряется только в Windows-окно. Цвет берётся из загруженных стилей Блокнота.
(() => {
  if (window.top !== window) return;
  let previous;
  const hex = /^#[\da-f]{6}$/i;
  function sync() {
    const root = document.documentElement;
    if (!root) return;
    const css = getComputedStyle(root);
    const background = css.getPropertyValue('--chrome').trim();
    const foreground = css.getPropertyValue('--ink').trim();
    const colors = hex.test(background) && hex.test(foreground)
      ? background.slice(1) + '/' + foreground.slice(1)
      : 'default';
    if (colors === previous) return;
    previous = colors;
    // Tauri отменяет этот переход локально. У страницы нет доступа к нативным командам.
    location.assign('https://bloknot-window-theme.invalid/' + colors);
  }
  function start() {
    new MutationObserver(sync).observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['data-theme'],
    });
    matchMedia('(prefers-color-scheme: dark)').addEventListener('change', sync);
    sync();
  }
  if (document.readyState !== 'complete') window.addEventListener('load', start, { once: true });
  else start();
})();
