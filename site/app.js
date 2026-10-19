// Vigilant Vole landing page — progressive enhancement only. Everything on the
// page works without this file: the nav wraps, content is visible, and the
// install snippets can be selected by hand.
(function () {
  "use strict";

  // Tells the CSS the script is running (cancels the reveal safety net)
  document.documentElement.classList.add("js-ok");

  var reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

  // Nav: hairline once content scrolls under it --------------------------
  var bar = document.querySelector(".topbar");
  function onScroll() {
    if (bar) bar.classList.toggle("is-scrolled", window.scrollY > 4);
  }
  window.addEventListener("scroll", onScroll, { passive: true });
  onScroll();

  // Mobile menu ------------------------------------------------------------
  var toggle = document.querySelector(".nav-toggle");
  var menu = document.getElementById("topnav");
  if (toggle && menu) {
    toggle.hidden = false;
    var setOpen = function (open) {
      toggle.setAttribute("aria-expanded", String(open));
      toggle.setAttribute("aria-label", open ? "Close menu" : "Menu");
      menu.classList.toggle("open", open);
      bar.classList.toggle("menu-open", open);
    };
    toggle.addEventListener("click", function () {
      setOpen(toggle.getAttribute("aria-expanded") !== "true");
    });
    menu.addEventListener("click", function (e) {
      if (e.target.closest("a")) setOpen(false);
    });
    document.addEventListener("keydown", function (e) {
      if (e.key === "Escape" && menu.classList.contains("open")) {
        setOpen(false);
        toggle.focus();
      }
    });
    document.addEventListener("click", function (e) {
      if (menu.classList.contains("open") && !bar.contains(e.target)) setOpen(false);
    });
    window.matchMedia("(min-width: 881px)").addEventListener("change", function (mq) {
      if (mq.matches) setOpen(false);
    });
  }

  // Current section in the nav ---------------------------------------------
  var navLinks = Array.prototype.slice.call(document.querySelectorAll('.topnav-link[href^="#"]'));
  if ("IntersectionObserver" in window && navLinks.length) {
    var byId = {};
    navLinks.forEach(function (a) {
      byId[a.getAttribute("href").slice(1)] = a;
    });
    var spy = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (entry) {
          var link = byId[entry.target.id];
          if (!link) return;
          if (entry.isIntersecting) {
            navLinks.forEach(function (a) {
              a.removeAttribute("aria-current");
            });
            link.setAttribute("aria-current", "true");
          } else if (link.getAttribute("aria-current")) {
            link.removeAttribute("aria-current");
          }
        });
      },
      { rootMargin: "-45% 0px -50% 0px" }
    );
    Object.keys(byId).forEach(function (id) {
      var el = document.getElementById(id);
      if (el) spy.observe(el);
    });
  }

  // Reveal on scroll, once ---------------------------------------------------
  var reveals = document.querySelectorAll(".reveal");
  if ("IntersectionObserver" in window && !reduceMotion.matches) {
    var io = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (entry) {
          if (entry.isIntersecting) {
            entry.target.classList.add("is-in");
            io.unobserve(entry.target);
          }
        });
      },
      { rootMargin: "0px 0px -8% 0px", threshold: 0.08 }
    );
    reveals.forEach(function (el) {
      io.observe(el);
    });
  } else {
    reveals.forEach(function (el) {
      el.classList.add("is-in");
    });
  }

  // Copy buttons -------------------------------------------------------------
  function copyText(text) {
    if (navigator.clipboard && window.isSecureContext) {
      return navigator.clipboard.writeText(text);
    }
    return new Promise(function (resolve, reject) {
      var ta = document.createElement("textarea");
      ta.value = text;
      ta.setAttribute("readonly", "");
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      var ok = false;
      try {
        ok = document.execCommand("copy");
      } catch (err) {
        ok = false;
      }
      document.body.removeChild(ta);
      if (ok) resolve();
      else reject(new Error("copy failed"));
    });
  }

  var live = document.createElement("p");
  live.className = "vh";
  live.setAttribute("role", "status");
  document.body.appendChild(live);

  document.querySelectorAll("[data-copy]").forEach(function (btn) {
    var source = document.getElementById(btn.getAttribute("data-copy"));
    if (!source) return;
    var label = btn.querySelector("span");
    var icon = btn.querySelector("use");
    var timer;
    btn.hidden = false;
    btn.setAttribute("aria-label", "Copy " + (source.closest("pre").getAttribute("aria-label") || "snippet"));
    btn.addEventListener("click", function () {
      copyText(source.textContent).then(
        function () {
          clearTimeout(timer);
          btn.dataset.state = "copied";
          label.textContent = "Copied";
          if (icon) icon.setAttribute("href", "assets/icons.svg#check");
          live.textContent = "Copied to the clipboard";
          timer = setTimeout(function () {
            delete btn.dataset.state;
            label.textContent = "Copy";
            if (icon) icon.setAttribute("href", "assets/icons.svg#copy");
            live.textContent = "";
          }, 2000);
        },
        function () {
          live.textContent = "Couldn’t copy. Select the text and copy it by hand.";
        }
      );
    });
  });

})();
