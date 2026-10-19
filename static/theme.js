/* Vigilant Vole: runs before first paint (blocking, tiny) so a stored theme
   choice never flashes the wrong colours. Everything else lives in app.js. */
(function () {
  var d = document.documentElement;
  d.classList.add("js");
  if (d.hasAttribute("data-theme-lock")) return;
  try {
    var t = localStorage.getItem("vv-theme");
    if (t === "light" || t === "dark") d.setAttribute("data-theme", t);
  } catch (e) {
    /* storage blocked: follow the system setting */
  }
})();
