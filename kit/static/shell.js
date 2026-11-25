/* vole-kit side-menu shell (Shell::Sidebar): folding the menu to a rail on a
   wide screen, and the drawer on a phone. Loaded in <head>, before first
   paint, so a folded menu never flashes open; everything listens on the
   document, because a boosted page change replaces the <body> and the menu
   with it. The only thing kept on the device is the folded flag. */
(function () {
  var d = document;
  var root = d.documentElement;
  var KEY = "vv-side";
  var PHONE = "(max-width: 720px)";

  function folded() {
    return root.getAttribute("data-side") === "collapsed";
  }
  try {
    if (localStorage.getItem(KEY) === "collapsed") root.setAttribute("data-side", "collapsed");
  } catch (e) {
    /* storage blocked: the menu starts open */
  }

  function side() {
    return d.getElementById("side");
  }
  function backdrop() {
    return d.querySelector(".side-backdrop");
  }
  function isOpen() {
    var s = side();
    return !!s && s.hasAttribute("data-open");
  }

  /* The state a screen reader hears: the fold button says whether the menu is
     expanded, the open button whether the drawer is. */
  function sync() {
    var fold = d.querySelector("[data-side-collapse]");
    if (fold) fold.setAttribute("aria-expanded", String(!folded()));
    var open = d.querySelector("[data-side-open]");
    if (open) open.setAttribute("aria-expanded", String(isOpen()));
  }

  function setDrawer(on, focus) {
    var s = side();
    var b = backdrop();
    if (!s) return;
    s.toggleAttribute("data-open", on);
    if (b) b.toggleAttribute("data-open", on);
    /* Open, the drawer is the whole page for a screen reader and the keyboard:
       what is behind it is out of reach, so focus cannot leave it. */
    var behind = d.querySelectorAll(".side-body, .side-top");
    for (var i = 0; i < behind.length; i++) behind[i].toggleAttribute("inert", on);
    sync();
    if (!focus) return;
    var target = on ? s.querySelector("[data-side-close]") : d.querySelector("[data-side-open]");
    if (target) target.focus();
  }

  d.addEventListener("click", function (e) {
    var t = e.target;
    if (!t || !t.closest) return;
    if (t.closest("[data-side-open]")) {
      setDrawer(true, true);
    } else if (t.closest("[data-side-close]")) {
      setDrawer(false, true);
    } else if (t.closest(".side-backdrop")) {
      setDrawer(false, true);
    } else if (t.closest("[data-side-collapse]")) {
      var next = !folded();
      if (next) root.setAttribute("data-side", "collapsed");
      else root.removeAttribute("data-side");
      try {
        if (next) localStorage.setItem(KEY, "collapsed");
        else localStorage.removeItem(KEY);
      } catch (err) {
        /* storage blocked: the choice lasts until the tab closes */
      }
      sync();
    } else if (t.closest(".side-link") && isOpen()) {
      setDrawer(false, false);
    }
  });

  d.addEventListener("keydown", function (e) {
    if (!isOpen()) return;
    if (e.key === "Escape") {
      /* An open account menu closes first (its own popover handles Escape). */
      if (d.querySelector(".menu:popover-open")) return;
      e.preventDefault();
      setDrawer(false, true);
    } else if (e.key === "Tab") {
      /* Tab goes round inside the drawer. */
      var s = side();
      var list = Array.prototype.slice
        .call(s.querySelectorAll("a[href], button:not([disabled])"))
        .filter(function (n) {
          return n.getClientRects().length > 0;
        });
      if (!list.length) return;
      var first = list[0];
      var last = list[list.length - 1];
      if (e.shiftKey && d.activeElement === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && d.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
    }
  });

  /* Growing to a wide screen ends the drawer. */
  if (window.matchMedia) {
    var mq = window.matchMedia(PHONE);
    var onChange = function (e) {
      if (!e.matches && isOpen()) setDrawer(false, false);
    };
    if (mq.addEventListener) mq.addEventListener("change", onChange);
  }

  /* A boosted page change brings a new menu (closed, labelled for the open
     state): bring the labels in line. */
  d.addEventListener("htmx:afterSettle", sync);
  d.addEventListener("DOMContentLoaded", sync);
  window.addEventListener("pageshow", sync);
})();
