// Applies the chosen colour theme before the page renders. A classic script in <head>, so
// it runs before the body parses; it is a file rather than inline because the CSP allows
// only same-origin scripts. Keep it tiny and dependency-free, and keep the storage key and
// values in step with src/theme.ts, which owns the preference after start-up.
(function () {
  var theme = null;
  try {
    theme = localStorage.getItem("apitorrent.theme");
  } catch (e) {
    // Storage blocked (privacy mode, sandboxed frame): follow the OS.
  }
  if (theme !== "light" && theme !== "dark") return;

  var root = document.documentElement;
  root.setAttribute("data-theme", theme);
  // The browser paints its default canvas from this before any stylesheet arrives.
  var scheme = document.querySelector('meta[name="color-scheme"]');
  if (scheme) scheme.setAttribute("content", theme);
  // Pin the mobile browser bar to the chosen theme's colour.
  var bars = document.querySelectorAll('meta[name="theme-color"][data-scheme]');
  for (var i = 0; i < bars.length; i++) {
    bars[i].setAttribute("media", bars[i].getAttribute("data-scheme") === theme ? "all" : "not all");
  }
})();
