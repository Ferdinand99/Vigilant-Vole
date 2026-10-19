/* Vigilant Vole — app.js
   Vanilla, no dependencies, loaded with `defer` after htmx. Every feature is a
   progressive enhancement: pages work without it, this makes them feel native.
   Public API on window.VV: confirm, toast, announce, openSheet, closeSheet,
   spring, init. Hooks are data-* attributes, documented in docs/design-system.md. */
(function () {
  "use strict";

  var d = document;
  var root = d.documentElement;
  root.classList.add("js");

  var VV = (window.VV = window.VV || {});
  var reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
  var phone = window.matchMedia("(max-width: 640px)");

  /* View transitions (boosted navigation, theme switch) are skipped when the
     user navigates away or hides the tab mid-transition. That rejects the
     transition's promises, and Firefox reports the unhandled rejection as a
     console error. A skipped transition is harmless: mark them handled. */
  if (typeof d.startViewTransition === "function") {
    var startTransition = d.startViewTransition.bind(d);
    d.startViewTransition = function () {
      var t = startTransition.apply(null, arguments);
      ["ready", "finished", "updateCallbackDone"].forEach(function (k) {
        if (t && t[k] && t[k].catch) t[k].catch(function () {});
      });
      return t;
    };
  }

  /* ---------------------------------------------------------------- utils */
  function $(sel, scope) {
    return (scope || d).querySelector(sel);
  }
  function $$(sel, scope) {
    return Array.prototype.slice.call((scope || d).querySelectorAll(sel));
  }
  function icon(name) {
    return '<svg class="icon" aria-hidden="true"><use href="/static/icons.svg#' + name + '"></use></svg>';
  }
  function esc(s) {
    return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
    });
  }
  /* Interface strings: the layouts embed <script type="application/json"
     id="vv-i18n"> (the server's `js.*` keys in the page language), exposed as
     window.VV_I18N. English is the fallback for anything missing. */
  var I18N = null;
  function L(key, fallback, vars) {
    if (I18N === null) {
      try {
        var blob = d.getElementById("vv-i18n");
        I18N = blob ? JSON.parse(blob.textContent) || {} : {};
      } catch (e) {
        I18N = {};
      }
      window.VV_I18N = I18N;
    }
    var s = I18N[key] || fallback;
    if (vars) {
      Object.keys(vars).forEach(function (k) {
        s = s.split("{" + k + "}").join(String(vars[k]));
      });
    }
    return s;
  }
  VV.t = L;
  var store = {
    get: function (k) {
      try {
        return window.localStorage.getItem(k);
      } catch (e) {
        return null;
      }
    },
    set: function (k, v) {
      try {
        if (v == null) window.localStorage.removeItem(k);
        else window.localStorage.setItem(k, v);
      } catch (e) {
        /* private mode / blocked storage: preference just isn't remembered */
      }
    },
  };
  function capture(el, pointerId) {
    try {
      el.setPointerCapture(pointerId);
    } catch (e) {
      /* pointer already released (synthetic events): tracking still works */
    }
  }
  function nextFrame(fn) {
    requestAnimationFrame(function () {
      requestAnimationFrame(fn);
    });
  }
  /* Run `done` after the element's transform/opacity transition ends (with a
     timeout so a missing transitionend never strands the UI). */
  function afterTransition(el, done, maxMs) {
    var finished = false;
    function finish(e) {
      if (finished || (e && e.target !== el)) return;
      finished = true;
      el.removeEventListener("transitionend", finish);
      done();
    }
    el.addEventListener("transitionend", finish);
    setTimeout(finish, maxMs || 450);
  }
  function focusable(scope) {
    return $$(
      'a[href], button:not([disabled]), input:not([disabled]):not([type="hidden"]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
      scope
    ).filter(function (el) {
      return el.offsetParent !== null || el === d.activeElement;
    });
  }

  /* ------------------------------------------------------------- springs
     Damping ratio + response (Apple's parameters). Critically damped by
     default; animates from the current value and carries velocity in, so it
     can be interrupted at any time. Units are whatever you pass (px, px/s). */
  function spring(opts) {
    var to = opts.to;
    var damping = opts.damping == null ? 1 : opts.damping;
    var response = opts.response || 0.35;
    var omega = (2 * Math.PI) / response;
    var k = omega * omega;
    var c = 2 * damping * omega;
    var x = opts.from - to;
    var v = opts.velocity || 0;
    var last = performance.now();
    var raf = 0;
    var stopped = false;
    function frame(now) {
      if (stopped) return;
      var dt = Math.min((now - last) / 1000, 0.064);
      last = now;
      var steps = Math.max(1, Math.ceil(dt / 0.004));
      var h = dt / steps;
      for (var i = 0; i < steps; i++) {
        var a = -k * x - c * v;
        v += a * h;
        x += v * h;
      }
      if (Math.abs(x) < 0.25 && Math.abs(v) < 8) {
        opts.onUpdate(to, 0);
        if (opts.onComplete) opts.onComplete();
        return;
      }
      opts.onUpdate(to + x, v);
      raf = requestAnimationFrame(frame);
    }
    if (reduceMotion.matches || opts.instant) {
      opts.onUpdate(to, 0);
      if (opts.onComplete) opts.onComplete();
    } else {
      raf = requestAnimationFrame(frame);
    }
    return {
      stop: function () {
        stopped = true;
        cancelAnimationFrame(raf);
      },
      value: function () {
        return to + x;
      },
      velocity: function () {
        return v;
      },
    };
  }
  VV.spring = spring;

  /* Pointer velocity from the last ~100ms of samples, in px/s. */
  function tracker() {
    var samples = [];
    return {
      add: function (y) {
        var t = performance.now();
        samples.push({ y: y, t: t });
        while (samples.length > 2 && t - samples[0].t > 100) samples.shift();
      },
      velocity: function () {
        if (samples.length < 2) return 0;
        var a = samples[0];
        var b = samples[samples.length - 1];
        var dt = (b.t - a.t) / 1000;
        return dt > 0 ? (b.y - a.y) / dt : 0;
      },
    };
  }
  /* Where a flick would come to rest (Apple's deceleration projection). */
  function project(velocity, rate) {
    rate = rate || 0.998;
    return ((velocity / 1000) * rate) / (1 - rate);
  }
  /* Progressive resistance past a boundary. */
  function rubberband(overshoot, dimension) {
    var c = 0.55;
    return (overshoot * dimension * c) / (dimension + c * Math.abs(overshoot));
  }

  /* ------------------------------------------------------------ announce */
  function liveRegion() {
    var el = $("#vv-announcer");
    if (!el) {
      el = d.createElement("div");
      el.id = "vv-announcer";
      el.className = "vh";
      el.setAttribute("aria-live", "polite");
      el.setAttribute("aria-atomic", "true");
      d.body.appendChild(el);
    }
    return el;
  }
  VV.announce = function (message) {
    var el = liveRegion();
    el.textContent = "";
    setTimeout(function () {
      el.textContent = message;
    }, 50);
  };

  /* --------------------------------------------------------------- theme */
  var THEME_KEY = "vv-theme";
  var THEME_BG = { light: "#f5f5f7", dark: "#0b0b0d" };
  function currentTheme() {
    var t = root.getAttribute("data-theme");
    return t === "light" || t === "dark" ? t : "auto";
  }
  function syncThemeUi() {
    var t = currentTheme();
    $$("[data-theme-set]").forEach(function (el) {
      var on = el.getAttribute("data-theme-set") === t;
      if (el.matches("input")) el.checked = on;
      else el.setAttribute(el.getAttribute("role") === "menuitemradio" ? "aria-checked" : "aria-pressed", String(on));
    });
    $$('meta[name="theme-color"]').forEach(function (m) {
      if (!m.hasAttribute("data-default")) m.setAttribute("data-default", m.getAttribute("content"));
      m.setAttribute("content", t === "auto" ? m.getAttribute("data-default") : THEME_BG[t]);
    });
    initSegmented(d);
  }
  function setTheme(t) {
    function apply() {
      if (t === "light" || t === "dark") root.setAttribute("data-theme", t);
      else root.removeAttribute("data-theme");
      store.set(THEME_KEY, t === "auto" ? null : t);
      syncThemeUi();
    }
    /* Ease the brightness change with a cross-fade instead of a hard cut. */
    if (d.startViewTransition && !reduceMotion.matches) d.startViewTransition(apply);
    else apply();
  }
  VV.setTheme = setTheme;

  /* -------------------------------------------------------------- toasts */
  var TOAST_ICON = { success: "check-circle", error: "alert-circle", danger: "alert-circle", warning: "alert", info: "info" };
  function toaster() {
    var el = $("#toaster");
    if (!el) {
      el = d.createElement("div");
      el.id = "toaster";
      el.className = "toaster";
      el.setAttribute("aria-live", "polite");
      d.body.appendChild(el);
    }
    return el;
  }
  function dismissToast(el) {
    if (!el || el.getAttribute("data-anim") === "exit") return;
    clearTimeout(el._vvTimer);
    el.setAttribute("data-anim", "exit");
    el.style.transform = "";
    afterTransition(el, function () {
      flipSiblings(el.parentNode, function () {
        el.remove();
      });
    });
  }
  function scheduleToast(el, ms) {
    var remaining = ms;
    var started = 0;
    function start() {
      if (d.hidden) return;
      started = Date.now();
      el._vvTimer = setTimeout(function () {
        dismissToast(el);
      }, remaining);
    }
    function pause() {
      clearTimeout(el._vvTimer);
      if (started) remaining -= Date.now() - started;
      started = 0;
    }
    el.addEventListener("pointerenter", pause);
    el.addEventListener("pointerleave", start);
    el.addEventListener("focusin", pause);
    el.addEventListener("focusout", start);
    d.addEventListener("visibilitychange", function () {
      if (d.hidden) pause();
      else if (el.isConnected && !started) start();
    });
    start();
  }
  /* Swipe a toast up to dismiss it; it follows the finger 1:1. */
  function enableToastSwipe(el) {
    el.addEventListener("pointerdown", function (e) {
      if (e.button !== 0 || e.target.closest("button, a")) return;
      var startY = e.clientY;
      var dy = 0;
      var vt = tracker();
      var moved = false;
      capture(el, e.pointerId);
      el.classList.add("is-dragging");
      function move(ev) {
        dy = ev.clientY - startY;
        if (Math.abs(dy) > 4) moved = true;
        vt.add(ev.clientY);
        var y = dy < 0 ? dy : rubberband(dy, 120);
        el.style.transform = "translateY(" + y + "px)";
      }
      function up() {
        el.removeEventListener("pointermove", move);
        el.removeEventListener("pointerup", up);
        el.removeEventListener("pointercancel", up);
        el.classList.remove("is-dragging");
        var v = vt.velocity();
        if (dy + project(v) < -40) {
          dismissToast(el);
        } else if (!moved) {
          dismissToast(el); // a tap dismisses, like iOS banners
        } else {
          el.style.transform = "";
        }
      }
      el.addEventListener("pointermove", move);
      el.addEventListener("pointerup", up);
      el.addEventListener("pointercancel", up);
    });
  }
  function showToast(el, kind) {
    var host = toaster();
    var ms = kind === "error" || kind === "danger" ? 6500 : 4200;
    el.setAttribute("data-anim", "enter");
    flipSiblings(host, function () {
      host.insertBefore(el, host.firstChild);
    });
    while (host.children.length > 3) host.removeChild(host.lastElementChild);
    void el.offsetWidth;
    nextFrame(function () {
      el.removeAttribute("data-anim");
    });
    enableToastSwipe(el);
    scheduleToast(el, ms);
  }
  VV.toast = function (message, kind, opts) {
    kind = kind || "info";
    opts = opts || {};
    var el = d.createElement("div");
    el.className = "toast status-" + kind;
    el.setAttribute("role", kind === "error" || kind === "danger" ? "alert" : "status");
    el.innerHTML =
      icon(TOAST_ICON[kind] || "info") +
      '<span class="toast-message">' +
      esc(message) +
      '</span><button type="button" class="toast-close" data-toast-close aria-label="' +
      esc(L("dismiss", "Dismiss")) +
      '">' +
      icon("x") +
      "</button>";
    showToast(el, kind);
    return el;
  };
  /* Server-rendered flash toasts (ctx.flash) */
  function initToasts(scope) {
    $$("[data-toast]", scope).forEach(function (el) {
      if (el._vvToast) return;
      el._vvToast = true;
      var kind = el.getAttribute("data-toast") || "info";
      var host = toaster();
      if (el.parentNode !== host) host.appendChild(el);
      el.setAttribute("data-anim", "enter");
      void el.offsetWidth;
      nextFrame(function () {
        el.removeAttribute("data-anim");
      });
      enableToastSwipe(el);
      scheduleToast(el, kind === "error" ? 6500 : 4200);
    });
  }

  /* FLIP: animate siblings to their new positions after a DOM change. */
  function flipSiblings(container, mutate) {
    if (!container || reduceMotion.matches) {
      mutate();
      return;
    }
    var kids = Array.prototype.slice.call(container.children);
    var before = kids.map(function (k) {
      return k.getBoundingClientRect().top;
    });
    mutate();
    kids.forEach(function (k, i) {
      if (!k.isConnected || k.getAttribute("data-anim")) return;
      var delta = before[i] - k.getBoundingClientRect().top;
      if (!delta) return;
      k.style.transition = "none";
      k.style.transform = "translateY(" + delta + "px)";
      void k.offsetWidth;
      k.style.transition = "";
      k.style.transform = "";
    });
  }

  /* -------------------------------------------------------------- sheets */
  var sheetStack = [];
  function resolveSheet(target) {
    if (!target) return null;
    return typeof target === "string" ? $(target) : target;
  }
  function openSheet(target, trigger) {
    var sheet = resolveSheet(target);
    if (!sheet || sheet.open) return sheet;
    if (typeof sheet.showModal !== "function") return sheet;
    sheet._vvTrigger = trigger || d.activeElement;
    sheet.setAttribute("data-anim", "enter");
    sheet.showModal();
    sheetStack.push(sheet);
    root.classList.add("has-sheet");
    /* Grow from the trigger on wide screens; slide up on phones. */
    if (!phone.matches && trigger && trigger.getBoundingClientRect) {
      var r = trigger.getBoundingClientRect();
      var ox = r.left + r.width / 2 - sheet.offsetLeft;
      var oy = r.top + r.height / 2 - sheet.offsetTop;
      sheet.style.transformOrigin = ox + "px " + oy + "px";
    } else {
      sheet.style.transformOrigin = "";
    }
    void sheet.offsetWidth;
    nextFrame(function () {
      sheet.removeAttribute("data-anim");
    });
    var auto = $("[autofocus]", sheet);
    if (auto) auto.focus();
    initSegmented(sheet);
    sheet.dispatchEvent(new CustomEvent("vv:sheet-open", { bubbles: true }));
    return sheet;
  }
  function closeSheet(target, returnValue) {
    var sheet = resolveSheet(target) || sheetStack[sheetStack.length - 1];
    if (!sheet || !sheet.open || sheet.getAttribute("data-anim") === "exit") return;
    if (sheet._vvSpring) sheet._vvSpring.stop();
    sheet.setAttribute("data-anim", "exit");
    sheet.style.transform = "";
    sheet.style.removeProperty("--sheet-scrim");
    afterTransition(sheet, function () {
      sheet.close(returnValue || "");
      sheet.removeAttribute("data-anim");
    });
  }
  function onSheetClosed(sheet) {
    sheetStack = sheetStack.filter(function (s) {
      return s !== sheet && s.open;
    });
    if (!sheetStack.length) root.classList.remove("has-sheet");
    var t = sheet._vvTrigger;
    sheet._vvTrigger = null;
    if (t && t.isConnected && typeof t.focus === "function") t.focus({ preventScroll: true });
    sheet.dispatchEvent(new CustomEvent("vv:sheet-close", { bubbles: true }));
    flushDeferredSwaps();
  }
  VV.openSheet = openSheet;
  VV.closeSheet = closeSheet;

  /* Drag the sheet down to dismiss (phones). 1:1 tracking, velocity handoff,
     momentum projection decides close vs. settle back. */
  function enableSheetDrag(sheet, e) {
    if (!phone.matches || e.button !== 0) return;
    if (e.target.closest("button, a, input, select, textarea, label")) return;
    var startY = e.clientY;
    var h = sheet.offsetHeight;
    var y = 0;
    var vt = tracker();
    if (sheet._vvSpring) sheet._vvSpring.stop();
    sheet.classList.add("is-dragging");
    capture(e.currentTarget, e.pointerId);
    var handle = e.currentTarget;
    function render(val) {
      sheet.style.transform = "translateY(" + val + "px)";
      sheet.style.setProperty("--sheet-scrim", String(Math.max(0, 1 - val / h)));
    }
    function move(ev) {
      var dy = ev.clientY - startY;
      y = dy >= 0 ? dy : rubberband(dy, h);
      vt.add(ev.clientY);
      render(y);
    }
    function up() {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", up);
      handle.removeEventListener("pointercancel", up);
      sheet.classList.remove("is-dragging");
      var v = vt.velocity();
      var close = y + project(v) > h * 0.5 || v > 900;
      sheet._vvSpring = spring({
        from: y,
        to: close ? h : 0,
        velocity: v,
        damping: 1,
        response: 0.32,
        onUpdate: render,
        onComplete: function () {
          if (close) {
            sheet.close("");
          }
          sheet.style.transform = "";
          sheet.style.removeProperty("--sheet-scrim");
        },
      });
    }
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", up);
    handle.addEventListener("pointercancel", up);
  }
  function wireSheet(sheet) {
    if (sheet._vvWired) return;
    sheet._vvWired = true;
    sheet.addEventListener("cancel", function (e) {
      e.preventDefault();
      closeSheet(sheet);
    });
    sheet.addEventListener("close", function () {
      onSheetClosed(sheet);
    });
    /* Click on the scrim (outside the panel) closes */
    sheet.addEventListener("pointerdown", function (e) {
      if (e.target !== sheet) return;
      var r = sheet.getBoundingClientRect();
      var inside = e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom;
      if (!inside && !sheet.hasAttribute("data-sheet-static")) closeSheet(sheet);
    });
    $$(".sheet-grabber, .sheet-head", sheet).forEach(function (h) {
      h.addEventListener("pointerdown", function (e) {
        enableSheetDrag(sheet, e);
      });
    });
  }

  /* ------------------------------------------------------------- confirm */
  VV.confirm = function (opts) {
    opts = opts || {};
    return new Promise(function (resolve) {
      var sheet = d.createElement("dialog");
      sheet.className = "sheet sheet-alert";
      sheet.setAttribute("aria-labelledby", "vv-confirm-title");
      sheet.setAttribute("aria-describedby", "vv-confirm-message");
      sheet.innerHTML =
        '<div class="sheet-grabber" aria-hidden="true"></div>' +
        '<div class="sheet-body"><h2 class="sheet-title" id="vv-confirm-title">' +
        esc(opts.title || L("confirm_title", "Are you sure?")) +
        "</h2>" +
        (opts.message ? '<p class="alert-message" id="vv-confirm-message">' + esc(opts.message) + "</p>" : "") +
        '</div><div class="sheet-foot">' +
        '<button type="button" class="btn btn-secondary" data-answer="no">' +
        esc(opts.cancelLabel || L("cancel", "Cancel")) +
        "</button>" +
        '<button type="button" class="btn ' +
        (opts.destructive ? "btn-danger-solid" : "btn-primary") +
        '" data-answer="yes">' +
        esc(opts.confirmLabel || (opts.destructive ? L("delete", "Delete") : L("confirm", "Confirm"))) +
        "</button></div>";
      d.body.appendChild(sheet);
      wireSheet(sheet);
      var answer = false;
      sheet.addEventListener("click", function (e) {
        var b = e.target.closest("[data-answer]");
        if (!b) return;
        answer = b.getAttribute("data-answer") === "yes";
        closeSheet(sheet);
      });
      sheet.addEventListener("close", function () {
        resolve(answer);
        setTimeout(function () {
          sheet.remove();
        }, 0);
      });
      openSheet(sheet, opts.trigger || d.activeElement);
      /* Default focus on the safe choice */
      var cancel = $('[data-answer="no"]', sheet);
      if (cancel) cancel.focus();
    });
  };
  function confirmOptions(el, question) {
    var attr = function (n) {
      return el ? el.getAttribute(n) : null;
    };
    var destructiveAttr = attr("data-confirm-destructive");
    var verb = el && (el.getAttribute("hx-delete") != null ? "delete" : "");
    var destructive =
      destructiveAttr != null
        ? destructiveAttr !== "false"
        : verb === "delete" ||
          /\b(delete|remove|revoke|reset|disconnect|slett\w*|fjern\w*|tilbakestill\w*|deaktiver\w*|trekke? tilbake)\b/i.test(question || "");
    return {
      title: question,
      message: attr("data-confirm-message") || "",
      confirmLabel: attr("data-confirm-label") || (destructive ? L("delete", "Delete") : L("confirm", "Confirm")),
      cancelLabel: attr("data-confirm-cancel") || L("cancel", "Cancel"),
      destructive: destructive,
      trigger: el,
    };
  }
  /* htmx: hx-confirm="Delete this monitor?" → our sheet instead of confirm() */
  d.addEventListener("htmx:confirm", function (e) {
    var q = e.detail && e.detail.question;
    if (!q) return;
    e.preventDefault();
    VV.confirm(confirmOptions(e.detail.elt, q)).then(function (ok) {
      if (ok) e.detail.issueRequest(true);
    });
  });
  /* Plain forms and links/buttons: data-confirm="Delete this channel?" */
  d.addEventListener(
    "submit",
    function (e) {
      var form = e.target;
      if (!form.matches || !form.matches("form[data-confirm]")) return;
      if (form._vvConfirmed) {
        form._vvConfirmed = false;
        return;
      }
      e.preventDefault();
      e.stopImmediatePropagation();
      var submitter = e.submitter;
      VV.confirm(confirmOptions(form, form.getAttribute("data-confirm"))).then(function (ok) {
        if (!ok) return;
        form._vvConfirmed = true;
        if (form.requestSubmit) form.requestSubmit(submitter && submitter.form === form ? submitter : undefined);
        else form.submit();
      });
    },
    true
  );
  d.addEventListener(
    "click",
    function (e) {
      var el = e.target.closest && e.target.closest("a[data-confirm], button[data-confirm]");
      if (!el || el.closest("form[data-confirm]") === el) return;
      if (el._vvConfirmed) {
        el._vvConfirmed = false;
        return;
      }
      e.preventDefault();
      e.stopImmediatePropagation();
      VV.confirm(confirmOptions(el, el.getAttribute("data-confirm"))).then(function (ok) {
        if (!ok) return;
        el._vvConfirmed = true;
        el.click();
      });
    },
    true
  );

  /* --------------------------------------------------------------- menus
     Native popovers (popover + popovertarget) for open/close, light dismiss
     and Esc. We add positioning next to the trigger and arrow-key focus. */
  var lastInvoker = null;
  d.addEventListener(
    "click",
    function (e) {
      var inv = e.target.closest && e.target.closest("[popovertarget]");
      if (inv) lastInvoker = inv;
    },
    true
  );
  function placePopover(pop, inv) {
    if (!inv) return;
    var r = inv.getBoundingClientRect();
    var vw = window.innerWidth;
    var vh = window.innerHeight;
    var gap = 6;
    var s = pop.style;
    s.inset = "auto";
    s.margin = "0";
    var openUp = r.bottom > vh * 0.62 && r.top > vh * 0.38;
    var alignEnd = r.left + r.width / 2 > vw / 2;
    if (openUp) s.bottom = Math.max(8, vh - r.top + gap) + "px";
    else s.top = Math.max(8, r.bottom + gap) + "px";
    if (alignEnd) s.right = Math.max(8, vw - r.right) + "px";
    else s.left = Math.max(8, r.left) + "px";
    s.maxHeight = Math.max(160, (openUp ? r.top : vh - r.bottom) - gap - 12) + "px";
    s.setProperty("--origin", (openUp ? "bottom " : "top ") + (alignEnd ? "right" : "left"));
  }
  d.addEventListener(
    "beforetoggle",
    function (e) {
      var pop = e.target;
      if (!pop.matches || !pop.matches(".menu[popover], .popover[popover]")) return;
      if (e.newState === "open") {
        var inv =
          lastInvoker && lastInvoker.getAttribute("popovertarget") === pop.id
            ? lastInvoker
            : $('[popovertarget="' + pop.id + '"]');
        pop._vvInvoker = inv;
        placePopover(pop, inv);
      }
    },
    true
  );
  d.addEventListener(
    "toggle",
    function (e) {
      var pop = e.target;
      if (!pop.matches || !pop.matches(".menu[popover], .popover[popover]")) return;
      if (e.newState === "open") {
        initSegmented(pop);
        if (pop._vvKeyboard) {
          var first = $(".menu-item, button, a, input", pop);
          if (first) first.focus();
        }
        pop._vvKeyboard = false;
      }
    },
    true
  );
  d.addEventListener(
    "keydown",
    function (e) {
      if ((e.key === "Enter" || e.key === " " || e.key === "ArrowDown") && e.target.matches && e.target.matches("[popovertarget]")) {
        var pop = $("#" + CSS.escape(e.target.getAttribute("popovertarget")));
        if (pop) pop._vvKeyboard = true;
        if (e.key === "ArrowDown" && pop && pop.showPopover && !pop.matches(":popover-open")) {
          e.preventDefault();
          lastInvoker = e.target;
          pop.showPopover();
        }
      }
    },
    true
  );
  d.addEventListener("keydown", function (e) {
    var menu = e.target.closest && e.target.closest(".menu");
    if (!menu) return;
    var items = $$('.menu-item:not([disabled]), .segmented input', menu).filter(function (el) {
      return el.offsetParent !== null || el.matches(".segmented input");
    });
    var i = items.indexOf(d.activeElement);
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      if (e.target.matches(".segmented input")) return;
      e.preventDefault();
      var n = e.key === "ArrowDown" ? i + 1 : i - 1;
      if (n < 0) n = items.length - 1;
      if (n >= items.length) n = 0;
      if (items[n]) items[n].focus();
    } else if (e.key === "Home" || e.key === "End") {
      e.preventDefault();
      var t = e.key === "Home" ? items[0] : items[items.length - 1];
      if (t) t.focus();
    } else if (e.key === "Tab" && menu.hidePopover) {
      menu.hidePopover();
    }
  });
  /* Choosing an item closes its menu (after htmx/other handlers ran). */
  d.addEventListener("click", function (e) {
    var item = e.target.closest && e.target.closest(".menu-item");
    if (!item || item.hasAttribute("data-keep-open")) return;
    var menu = item.closest("[popover]");
    if (menu && menu.hidePopover && menu.matches(":popover-open")) {
      setTimeout(function () {
        try {
          menu.hidePopover();
        } catch (err) {
          /* already closed */
        }
      }, 0);
    }
  });

  /* ---------------------------------------------------------------- tabs */
  function selectTab(tab, focus) {
    var list = tab.closest('[role="tablist"]');
    if (!list) return;
    $$('[role="tab"]', list).forEach(function (t) {
      var on = t === tab;
      t.setAttribute("aria-selected", String(on));
      t.tabIndex = on ? 0 : -1;
      var panel = d.getElementById(t.getAttribute("aria-controls"));
      if (panel) panel.hidden = !on;
    });
    if (focus) tab.focus();
    tab.dispatchEvent(new CustomEvent("vv:tab", { bubbles: true }));
  }

  /* ----------------------------------------------------------- segmented */
  function initSegmented(scope) {
    $$(".segmented", scope).forEach(function (seg) {
      var kids = Array.prototype.slice.call(seg.children).filter(function (k) {
        return k.matches("label, a, button");
      });
      var idx = -1;
      kids.forEach(function (k, i) {
        var input = k.matches("label") ? $("input", k) : null;
        if (
          (input && input.checked) ||
          k.getAttribute("aria-current") === "page" ||
          k.getAttribute("aria-current") === "true" ||
          k.getAttribute("aria-pressed") === "true"
        )
          idx = i;
      });
      seg.style.setProperty("--n", String(kids.length || 1));
      if (idx >= 0) {
        if (!seg.hasAttribute("data-thumb")) {
          /* First paint: place the thumb without sliding */
          seg.style.setProperty("--i", String(idx));
          seg.setAttribute("data-thumb", "");
        } else {
          seg.style.setProperty("--i", String(idx));
        }
      } else {
        seg.removeAttribute("data-thumb");
      }
    });
  }

  /* ------------------------------------------------------- show-when
     <div data-show-when="monitor_type=http,keyword">  (or "!=" to invert)
     Hidden groups also disable their controls so they neither submit nor block
     validation. */
  function controlValue(scope, name) {
    var els = $$('[name="' + CSS.escape(name) + '"]', scope);
    for (var i = 0; i < els.length; i++) {
      var el = els[i];
      if (el.type === "radio") {
        if (el.checked) return el.value;
      } else if (el.type === "checkbox") {
        if (el.checked) return el.value || "on";
      } else {
        return el.value;
      }
    }
    return "";
  }
  function updateShowWhen(scope, changedName) {
    $$("[data-show-when]", scope || d).forEach(function (el) {
      var rule = el.getAttribute("data-show-when");
      var m = rule.match(/^\s*([^!=\s]+)\s*(!?=)\s*(.*)$/);
      if (!m) return;
      if (changedName && m[1] !== changedName) return;
      var form = el.closest("form") || d;
      var value = controlValue(form, m[1]);
      var list = m[3].split(",").map(function (s) {
        return s.trim();
      });
      var match = list.indexOf(value) !== -1;
      var show = m[2] === "=" ? match : !match;
      el.hidden = !show;
      $$("input, select, textarea, button", el).forEach(function (c) {
        if (!show) {
          if (!c.hasAttribute("data-vv-off")) {
            c.setAttribute("data-vv-off", c.disabled ? "1" : "0");
            c.disabled = true;
          }
        } else if (c.hasAttribute("data-vv-off")) {
          c.disabled = c.getAttribute("data-vv-off") === "1";
          c.removeAttribute("data-vv-off");
        }
      });
    });
  }

  /* ------------------------------------------------------- key-value editor */
  function kvSync(editor) {
    var target = editor.getAttribute("data-kv-json");
    if (!target) return;
    var out = $(target);
    if (!out) return;
    out.disabled = false;
    var obj = {};
    $$(".kv-row", $("[data-kv-rows]", editor) || editor).forEach(function (row) {
      var inputs = $$("input, select", row);
      if (inputs.length < 2) return;
      var k = inputs[0].value.trim();
      if (k) obj[k] = inputs[1].value;
    });
    out.value = Object.keys(obj).length ? JSON.stringify(obj) : "";
  }

  /* --------------------------------------------------------------- copy */
  function copyText(text) {
    if (navigator.clipboard && window.isSecureContext) return navigator.clipboard.writeText(text);
    return new Promise(function (resolve, reject) {
      var ta = d.createElement("textarea");
      ta.value = text;
      ta.setAttribute("readonly", "");
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      d.body.appendChild(ta);
      ta.select();
      try {
        d.execCommand("copy") ? resolve() : reject(new Error("copy failed"));
      } catch (err) {
        reject(err);
      }
      ta.remove();
    });
  }
  function onCopy(btn) {
    var text = btn.getAttribute("data-copy-text");
    if (text == null) {
      var src = $(btn.getAttribute("data-copy"));
      if (!src) return;
      text = "value" in src && src.value !== undefined && src.tagName !== "BUTTON" ? src.value : src.textContent;
    }
    copyText(text.trim()).then(
      function () {
        var label = $("span", btn);
        var ic = $("use", btn);
        if (!btn._vvLabel) btn._vvLabel = label ? label.textContent : null;
        if (label) label.textContent = btn.getAttribute("data-copied-label") || L("copied", "Copied");
        if (ic) ic.setAttribute("href", "/static/icons.svg#check");
        VV.announce(L("copied_announce", "Copied to clipboard"));
        clearTimeout(btn._vvCopyTimer);
        btn._vvCopyTimer = setTimeout(function () {
          if (label && btn._vvLabel != null) label.textContent = btn._vvLabel;
          if (ic) ic.setAttribute("href", "/static/icons.svg#copy");
        }, 1600);
      },
      function () {
        VV.toast(L("copy_failed", "Couldn’t copy. Select the text and copy it manually."), "error");
      }
    );
  }

  /* ------------------------------------------------------- relative time
     <time datetime="2026-10-19 14:03:00" data-relative>…</time>
     Times without a zone are UTC (the database format). */
  var rtf = null;
  function parseTime(s) {
    if (!s) return null;
    var iso = s.trim().replace(" ", "T");
    if (!/[zZ]|[+-]\d\d:?\d\d$/.test(iso)) iso += "Z";
    var t = Date.parse(iso);
    return isNaN(t) ? null : new Date(t);
  }
  function relative(date) {
    if (!rtf) {
      try {
        rtf = new Intl.RelativeTimeFormat(root.lang || "en", { numeric: "auto" });
      } catch (e) {
        rtf = null;
      }
    }
    var s = (date.getTime() - Date.now()) / 1000;
    var a = Math.abs(s);
    if (!rtf) return date.toLocaleString(root.lang || undefined);
    if (a < 45) return rtf.format(0, "second");
    if (a < 3600) return rtf.format(Math.round(s / 60), "minute");
    if (a < 86400) return rtf.format(Math.round(s / 3600), "hour");
    if (a < 86400 * 30) return rtf.format(Math.round(s / 86400), "day");
    if (a < 86400 * 365) return rtf.format(Math.round(s / (86400 * 30)), "month");
    return rtf.format(Math.round(s / (86400 * 365)), "year");
  }
  function updateTimes(scope) {
    $$("time[data-relative]", scope).forEach(function (el) {
      var date = parseTime(el.getAttribute("datetime"));
      if (!date) return;
      el.textContent = relative(date);
      if (!el.title) el.title = date.toLocaleString(root.lang || undefined);
    });
  }

  /* ------------------------------------------------- password strength
     <input type="password" data-password-strength> inside a .field with an
     optional <div class="meter" data-strength-meter> and a
     <p class="help" data-strength-label aria-live="polite">. Advisory only —
     the server enforces the real rules. Heuristic: length, character
     variety, and a penalty for common words, sequences and repeats. */
  var STRENGTH_LABELS = ["Too short", "Weak", "Fair", "Good", "Strong"];
  function strengthLabel(score) {
    return L("strength_" + score, STRENGTH_LABELS[score]);
  }
  var COMMON = /(password|passw0rd|qwert|asdf|zxcv|letmein|welcome|admin|login|iloveyou|monkey|dragon|master|secret|sunshine|princess|football|baseball|trustno1|abc123|111111|123123|000000|vigilant)/i;
  function hasRun(pw) {
    var s = pw.toLowerCase();
    for (var i = 0; i + 3 < s.length; i++) {
      var a = s.charCodeAt(i), b = s.charCodeAt(i + 1), c = s.charCodeAt(i + 2), e = s.charCodeAt(i + 3);
      if (b - a === c - b && c - b === e - c && Math.abs(b - a) <= 1) return true; // aaaa, abcd, 4321
    }
    return false;
  }
  function passwordScore(pw, minLen) {
    if (pw.length < minLen) return { score: 0, hint: L("hint_min_length", "use at least {n} characters", { n: minLen }) };
    var distinct = {};
    for (var i = 0; i < pw.length; i++) distinct[pw[i]] = true;
    var unique = Object.keys(distinct).length;
    var kinds = [/[a-z]/, /[A-Z]/, /[0-9]/, /[^A-Za-z0-9]/].filter(function (r) {
      return r.test(pw);
    }).length;
    var s = 1;
    if (pw.length >= 12 || kinds >= 3) s++;
    if (pw.length >= 16 || (pw.length >= 12 && kinds >= 3)) s++;
    if (pw.length >= 20 || (pw.length >= 14 && kinds >= 4)) s++;
    if (unique < 5) return { score: 1, hint: L("hint_repeated", "too many repeated characters") };
    if (COMMON.test(pw) || hasRun(pw)) {
      return { score: Math.min(s, pw.length >= 20 ? 2 : 1), hint: L("hint_common", "avoid common words and sequences") };
    }
    return { score: Math.min(s, 4), hint: "" };
  }
  function updateStrength(input) {
    var field = input.closest(".field") || input.parentNode;
    var meter = field && $("[data-strength-meter]", field);
    var label = field && $("[data-strength-label]", field);
    var min = parseInt(input.getAttribute("minlength"), 10) || 10;
    var r = passwordScore(input.value, min);
    var empty = !input.value;
    if (meter) {
      meter.style.setProperty("--value", empty ? "0" : String(Math.max(r.score, 0.35)));
      if (empty) meter.removeAttribute("data-score");
      else meter.setAttribute("data-score", String(r.score));
    }
    if (label) {
      if (label._vvDefault == null) label._vvDefault = label.textContent;
      if (empty) {
        label.textContent = label._vvDefault;
        label.removeAttribute("data-score");
      } else {
        label.textContent =
          L("strength_prefix", "Strength:") + " " + strengthLabel(r.score) + (r.hint ? " — " + r.hint + "." : "");
        label.setAttribute("data-score", String(r.score));
      }
    }
  }
  VV.passwordScore = passwordScore;

  /* ------------------------------------------------- time zone detection
     <input type="hidden" data-detect-timezone> or <select data-detect-timezone>:
     when the value is empty or "UTC", fill in the browser's zone (selects
     only if it's one of the options) and update [data-timezone-label]. */
  function detectTimezone(scope) {
    var tz;
    try {
      tz = Intl.DateTimeFormat().resolvedOptions().timeZone;
    } catch (e) {
      return;
    }
    if (!tz) return;
    $$("[data-detect-timezone]", scope).forEach(function (el) {
      if (el._vvTz || (el.value && el.value !== "UTC")) return;
      el._vvTz = true;
      if (el.tagName === "SELECT") {
        var has = $$("option", el).some(function (o) {
          return (o.value || o.textContent) === tz;
        });
        if (!has) return;
      }
      el.value = tz;
      var form = el.form || d;
      $$("[data-timezone-label]", form).forEach(function (l) {
        l.textContent = tz;
      });
    });
  }

  /* ------------------------------------------------------------ tooltip */
  var tip = null;
  var tipFor = null;
  var tipTimer = 0;
  var tipWarm = 0;
  function tooltipEl() {
    if (!tip || !tip.isConnected) {
      tip = d.createElement("div");
      tip.className = "tooltip";
      tip.setAttribute("role", "tooltip");
      tip.id = "vv-tooltip";
      d.body.appendChild(tip);
    }
    return tip;
  }
  function showTip(el) {
    var t = tooltipEl();
    tipFor = el;
    t.textContent = el.getAttribute("data-tip");
    t.classList.toggle("is-instant", Date.now() - tipWarm < 400);
    t.style.transform = "translate(-9999px, 0)";
    t.classList.add("is-visible");
    var r = el.getBoundingClientRect();
    var tr = t.getBoundingClientRect();
    var x = Math.min(Math.max(8, r.left + r.width / 2 - tr.width / 2), window.innerWidth - tr.width - 8);
    var y = r.top - tr.height - 8;
    if (y < 8) y = r.bottom + 8;
    t.style.transform = "translate(" + Math.round(x) + "px," + Math.round(y) + "px)";
  }
  function hideTip() {
    clearTimeout(tipTimer);
    if (tip && tip.classList.contains("is-visible")) tipWarm = Date.now();
    if (tip) tip.classList.remove("is-visible");
    tipFor = null;
  }
  d.addEventListener("pointerover", function (e) {
    var el = e.target.closest && e.target.closest("[data-tip]");
    if (!el || el === tipFor) return;
    clearTimeout(tipTimer);
    var warm = Date.now() - tipWarm < 400 || (tip && tip.classList.contains("is-visible"));
    tipTimer = setTimeout(function () {
      showTip(el);
    }, e.pointerType === "touch" || warm ? 0 : 350);
    if (e.pointerType === "touch") {
      tipTimer = setTimeout(hideTip, 1800);
    }
  });
  d.addEventListener("pointerout", function (e) {
    var el = e.target.closest && e.target.closest("[data-tip]");
    if (el && !el.contains(e.relatedTarget) && e.pointerType !== "touch") hideTip();
  });
  d.addEventListener("focusin", function (e) {
    var el = e.target.closest && e.target.closest("[data-tip]");
    if (el && e.target.matches(":focus-visible")) showTip(el);
  });
  d.addEventListener("focusout", hideTip);
  window.addEventListener("scroll", hideTip, { passive: true });

  /* ----------------------------------------------------------- sortable
     <ul data-sortable [data-sortable-group="g"] [data-sortable-name="order[]"]>
       <li data-sortable-item data-id="7"><button data-sortable-handle>…</button>…</li>
     Pointer: 1:1 tracking honouring the grab offset, siblings slide (FLIP),
     release settles with a critically-damped spring carrying the velocity.
     Keyboard: Alt+↑/↓ on the handle. Order is written into hidden inputs and
     a bubbling `vv:reorder` event fires on the list. */
  function sortItems(list) {
    return Array.prototype.slice.call(list.children).filter(function (c) {
      return c.hasAttribute("data-sortable-item");
    });
  }
  function groupLists(list) {
    var g = list.getAttribute("data-sortable-group");
    return g ? $$('[data-sortable][data-sortable-group="' + CSS.escape(g) + '"]') : [list];
  }
  function writeOrder(list) {
    var name = list.getAttribute("data-sortable-name") || "order[]";
    sortItems(list).forEach(function (item) {
      var input = $(":scope > input[data-sortable-input]", item);
      if (!input) {
        input = d.createElement("input");
        input.type = "hidden";
        input.setAttribute("data-sortable-input", "");
        item.appendChild(input);
      }
      input.name = name;
      input.value = item.getAttribute("data-id") || "";
    });
  }
  function fireReorder(lists, item) {
    lists.forEach(function (list) {
      writeOrder(list);
      list.dispatchEvent(
        new CustomEvent("vv:reorder", {
          bubbles: true,
          detail: {
            item: item,
            ids: sortItems(list).map(function (i) {
              return i.getAttribute("data-id");
            }),
          },
        })
      );
    });
  }
  function itemLabel(item) {
    var label = item.getAttribute("data-sortable-label");
    if (!label) {
      var title = $(".list-title", item);
      label = title ? title.textContent : item.innerText || item.textContent;
    }
    return (label || "").trim().replace(/\s+/g, " ").slice(0, 60);
  }
  /* Layout position (ignores transforms), in viewport coordinates. */
  function layoutTopOf(el) {
    var p = el.offsetParent;
    return (p ? p.getBoundingClientRect().top + p.clientTop : 0) + el.offsetTop;
  }
  function flipMove(nodes, mutate) {
    /* visual "before" (includes any in-flight shift) → layout "after" */
    var before = nodes.map(function (n) {
      return n.getBoundingClientRect().top;
    });
    mutate();
    if (reduceMotion.matches) return;
    nodes.forEach(function (n, i) {
      if (n.classList.contains("is-dragging")) return;
      var delta = before[i] - layoutTopOf(n);
      if (!delta) return;
      n.style.transition = "none";
      n.style.transform = "translateY(" + delta + "px)";
      void n.offsetWidth;
      n.style.transition = "transform 220ms var(--ease-out)";
      n.style.transform = "";
      setTimeout(function () {
        n.style.transition = "";
      }, 240);
    });
  }
  function startSort(e, handle) {
    if (e.button !== 0) return;
    var item = handle.closest("[data-sortable-item]");
    var list = item && item.closest("[data-sortable]");
    if (!item || !list) return;
    e.preventDefault();
    var lists = groupLists(list);
    var layoutTop = layoutTopOf(item);
    var grab = e.clientY - item.getBoundingClientRect().top;
    var ty = item.getBoundingClientRect().top - layoutTop; /* grab mid-settle without a jump */
    var lift = 0;
    var vt = tracker();
    var startList = list;
    var moved = false;
    var scrollRaf = 0;
    var lastY = e.clientY;
    if (item._vvSpring) item._vvSpring.stop();
    if (item._vvLift) item._vvLift.stop();
    capture(handle, e.pointerId);
    item.classList.add("is-dragging");
    root.classList.add("is-sorting");
    hideTip();
    function render() {
      var s = reduceMotion.matches ? 1 : 1 + 0.02 * lift;
      item.style.transform = "translateY(" + ty + "px) scale(" + s + ")";
    }
    item._vvLift = spring({
      from: 0,
      to: 1,
      response: 0.25,
      onUpdate: function (v) {
        lift = v;
        render();
      },
    });
    function reposition(y) {
      var cy = y - grab + item.offsetHeight / 2;
      var target = list;
      lists.forEach(function (l) {
        var r = l.getBoundingClientRect();
        if (y >= r.top && y <= r.bottom) target = l;
      });
      var siblings = sortItems(target).filter(function (s) {
        return s !== item;
      });
      var before = null;
      for (var i = 0; i < siblings.length; i++) {
        var mid = layoutTopOf(siblings[i]) + siblings[i].offsetHeight / 2;
        if (cy < mid) {
          before = siblings[i];
          break;
        }
      }
      var currentNext = item.nextElementSibling;
      while (currentNext && !currentNext.hasAttribute("data-sortable-item")) currentNext = currentNext.nextElementSibling;
      if (target === list && before === currentNext) return;
      var nodes = [];
      lists.forEach(function (l) {
        nodes = nodes.concat(sortItems(l));
      });
      flipMove(nodes, function () {
        if (before) target.insertBefore(item, before);
        else {
          var last = sortItems(target).pop();
          if (last && last !== item) last.after(item);
          else target.appendChild(item);
        }
      });
      list = target;
      /* keep the item glued under the finger after its layout slot moved */
      layoutTop = layoutTopOf(item);
      ty = y - grab - layoutTop;
      render();
    }
    function autoscroll() {
      var edge = 56;
      var vh = window.innerHeight;
      var dy = 0;
      if (lastY < edge) dy = -Math.ceil((edge - lastY) / 4);
      else if (lastY > vh - edge) dy = Math.ceil((lastY - (vh - edge)) / 4);
      if (dy) {
        window.scrollBy(0, dy);
        layoutTop -= dy;
        ty = lastY - grab - layoutTop;
        render();
        reposition(lastY);
      }
      scrollRaf = requestAnimationFrame(autoscroll);
    }
    scrollRaf = requestAnimationFrame(autoscroll);
    function move(ev) {
      lastY = ev.clientY;
      vt.add(ev.clientY);
      if (Math.abs(ev.clientY - e.clientY) > 3) moved = true;
      ty = ev.clientY - grab - layoutTop;
      render();
      reposition(ev.clientY);
    }
    function up() {
      cancelAnimationFrame(scrollRaf);
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", up);
      handle.removeEventListener("pointercancel", up);
      root.classList.remove("is-sorting");
      if (item._vvLift) item._vvLift.stop();
      var fromLift = lift;
      item._vvLift = spring({
        from: fromLift,
        to: 0,
        response: 0.3,
        onUpdate: function (v) {
          lift = v;
          render();
        },
      });
      item._vvSpring = spring({
        from: ty,
        to: 0,
        velocity: vt.velocity(),
        damping: 1,
        response: 0.35,
        onUpdate: function (v) {
          ty = v;
          render();
        },
        onComplete: function () {
          item.style.transform = "";
          item.classList.remove("is-dragging");
          flushDeferredSwaps();
        },
      });
      if (moved) {
        fireReorder(startList === list ? [list] : [startList, list], item);
        VV.announce(
          L("moved", "Moved {item} to position {pos} of {total}", {
            item: itemLabel(item),
            pos: sortItems(list).indexOf(item) + 1,
            total: sortItems(list).length,
          })
        );
      }
    }
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", up);
    handle.addEventListener("pointercancel", up);
  }
  function keySort(e, handle) {
    if (!e.altKey || (e.key !== "ArrowUp" && e.key !== "ArrowDown")) return;
    var item = handle.closest("[data-sortable-item]");
    var list = item && item.closest("[data-sortable]");
    if (!item || !list) return;
    e.preventDefault();
    var items = sortItems(list);
    var i = items.indexOf(item);
    var j = e.key === "ArrowUp" ? i - 1 : i + 1;
    if (j < 0 || j >= items.length) return;
    flipMove(items, function () {
      if (j < i) list.insertBefore(item, items[j]);
      else items[j].after(item);
    });
    handle.focus();
    fireReorder([list], item);
    VV.announce(L("moved", "Moved {item} to position {pos} of {total}", { item: itemLabel(item), pos: j + 1, total: items.length }));
  }

  /* ------------------------------------------------------------- SSE
     Live indicator + never swap a list out from under an open sheet, menu or
     drag; the last deferred update is applied once the user is done. */
  var deferred = null;
  function busy() {
    return root.classList.contains("has-sheet") || root.classList.contains("is-sorting") || !!$(".menu:popover-open, .popover:popover-open");
  }
  function flushDeferredSwaps() {
    if (!deferred || busy() || !window.htmx) return;
    var job = deferred;
    deferred = null;
    if (job.elt.isConnected) {
      try {
        window.htmx.swap(job.elt, job.data, { swapStyle: job.elt.getAttribute("hx-swap") || "innerHTML" });
      } catch (err) {
        /* next server event will refresh */
      }
    }
  }
  d.addEventListener("htmx:sseBeforeMessage", function (e) {
    if (busy()) {
      e.preventDefault();
      deferred = { elt: e.detail.elt || e.target, data: e.detail.data };
    }
  });
  d.addEventListener("toggle", function (e) {
    if (e.newState === "closed") setTimeout(flushDeferredSwaps, 0);
  }, true);
  function setLive(state) {
    $$(".live").forEach(function (el) {
      el.setAttribute("data-state", state);
      var label = $(".live-label", el);
      var text = el.getAttribute("data-label-" + state);
      if (label && text) label.textContent = text;
    });
  }
  d.addEventListener("htmx:sseOpen", function () {
    setLive("live");
  });
  d.addEventListener("htmx:sseError", function () {
    setLive(navigator.onLine === false ? "offline" : "reconnecting");
  });
  window.addEventListener("offline", function () {
    setLive("offline");
  });
  window.addEventListener("online", function () {
    setLive("reconnecting");
  });

  /* ------------------------------------------------- htmx integration */
  var inflight = 0;
  var barTimer = 0;
  function bar() {
    var el = $("#vv-progress");
    if (!el) {
      el = d.createElement("div");
      el.id = "vv-progress";
      el.className = "progress-bar";
      el.setAttribute("aria-hidden", "true");
      d.body.appendChild(el);
    }
    return el;
  }
  d.addEventListener("htmx:beforeRequest", function (e) {
    var elt = e.detail && e.detail.elt;
    if (elt && elt.closest && elt.closest("[data-no-progress]")) return;
    inflight++;
    if (inflight === 1) {
      clearTimeout(barTimer);
      barTimer = setTimeout(function () {
        var b = bar();
        b.classList.remove("is-done");
        void b.offsetWidth;
        b.classList.add("is-active");
      }, 150);
    }
  });
  d.addEventListener("htmx:afterRequest", function (e) {
    var elt = e.detail && e.detail.elt;
    if (elt && elt.closest && elt.closest("[data-no-progress]")) return;
    inflight = Math.max(0, inflight - 1);
    if (inflight === 0) {
      clearTimeout(barTimer);
      var b = $("#vv-progress");
      if (b && b.classList.contains("is-active")) {
        b.classList.remove("is-active");
        b.classList.add("is-done");
      }
    }
    /* data-redirect-after="/" on an hx-* element: go there once it succeeds
       (prefer an HX-Redirect response header when you control the handler). */
    if (e.detail && e.detail.successful && elt && elt.getAttribute && elt.getAttribute("data-redirect-after")) {
      window.location.assign(elt.getAttribute("data-redirect-after"));
      return;
    }
    if (e.detail && e.detail.failed && e.detail.xhr && e.detail.xhr.status >= 500) {
      VV.toast(L("server_error", "Something went wrong on the server. Try again."), "error");
    }
  });
  d.addEventListener("htmx:sendError", function () {
    VV.toast(L("network_error", "Can’t reach the server. Check your connection."), "error");
  });
  /* HX-Trigger: {"vv:toast": {"message": "Saved", "kind": "success"}} */
  d.addEventListener("vv:toast", function (e) {
    var det = e.detail || {};
    var msg = det.message || det.value;
    if (msg) VV.toast(msg, det.kind || "success");
  });
  /* HX-Trigger: vv:close-sheet — close the top sheet after a successful save */
  d.addEventListener("vv:close-sheet", function () {
    closeSheet();
  });
  /* Keep focus on the same control when a region is swapped (SSE refresh) */
  var focusId = null;
  d.addEventListener("htmx:beforeSwap", function (e) {
    var a = d.activeElement;
    focusId = a && a.id && e.detail.target && e.detail.target.contains(a) ? a.id : null;
  });
  d.addEventListener("htmx:afterSwap", function () {
    if (focusId) {
      var el = d.getElementById(focusId);
      if (el && el !== d.activeElement) el.focus({ preventScroll: true });
      focusId = null;
    }
  });

  /* ----------------------------------------------------- delegated events */
  d.addEventListener("click", function (e) {
    var t = e.target;
    if (!t.closest) return;
    var el;
    if ((el = t.closest("[data-sheet-open]"))) {
      e.preventDefault();
      var sheet = $(el.getAttribute("data-sheet-open"));
      if (sheet) {
        wireSheet(sheet);
        openSheet(sheet, el);
      }
      return;
    }
    if ((el = t.closest("[data-sheet-close]"))) {
      e.preventDefault();
      closeSheet(el.closest("dialog"));
      return;
    }
    if ((el = t.closest("[data-toast-close]"))) {
      dismissToast(el.closest(".toast"));
      return;
    }
    if ((el = t.closest("[data-copy], [data-copy-text]"))) {
      e.preventDefault();
      onCopy(el);
      return;
    }
    if ((el = t.closest("[data-theme-set]")) && !el.matches("input")) {
      setTheme(el.getAttribute("data-theme-set"));
      return;
    }
    if ((el = t.closest("[data-kv-add]"))) {
      e.preventDefault();
      var editor = el.closest("[data-kv]");
      var tpl = editor && $("template[data-kv-template]", editor);
      var rows = editor && ($("[data-kv-rows]", editor) || editor);
      if (tpl && rows) {
        var frag = tpl.content.cloneNode(true);
        var first = $("input, select", frag);
        rows.appendChild(frag);
        if (first) first.focus();
        kvSync(editor);
      }
      return;
    }
    if ((el = t.closest("[data-kv-remove]"))) {
      e.preventDefault();
      var ed = el.closest("[data-kv]");
      var row = el.closest(".kv-row");
      var rowsEl = row && row.parentNode;
      if (row && rowsEl) {
        var all = $$(".kv-row", rowsEl);
        if (all.length > 1) {
          var idx = all.indexOf(row);
          row.remove();
          var focusRow = all[idx + 1] || all[idx - 1];
          var f = focusRow && $("input", focusRow);
          if (f) f.focus();
        } else {
          $$("input", row).forEach(function (i) {
            i.value = "";
          });
          var f2 = $("input", row);
          if (f2) f2.focus();
        }
        if (ed) kvSync(ed);
      }
      return;
    }
    if ((el = t.closest('[role="tab"]')) && el.closest("[data-tabs]")) {
      e.preventDefault();
      selectTab(el, false);
    }
  });
  d.addEventListener("change", function (e) {
    var t = e.target;
    if (t.matches("[data-theme-set]") && t.checked) setTheme(t.getAttribute("data-theme-set"));
    if (t.closest && t.closest(".segmented")) initSegmented(t.closest(".segmented").parentNode);
    if (t.name) updateShowWhen(d, t.name);
  });
  d.addEventListener("input", function (e) {
    var t = e.target;
    if (t.matches && t.matches("input[data-password-strength]")) updateStrength(t);
    if (t.name && t.tagName !== "SELECT" && t.type !== "radio" && t.type !== "checkbox") updateShowWhen(d, t.name);
    var ed = t.closest && t.closest("[data-kv]");
    if (ed) kvSync(ed);
  });
  d.addEventListener("keydown", function (e) {
    var t = e.target;
    if (!t.closest) return;
    var handle = t.closest("[data-sortable-handle]");
    if (handle) {
      keySort(e, handle);
      return;
    }
    var tab = t.closest('[data-tabs] [role="tab"]');
    if (tab && (e.key === "ArrowRight" || e.key === "ArrowLeft" || e.key === "Home" || e.key === "End")) {
      var tabs = $$('[role="tab"]', tab.closest('[role="tablist"]'));
      var i = tabs.indexOf(tab);
      var n = e.key === "Home" ? 0 : e.key === "End" ? tabs.length - 1 : e.key === "ArrowRight" ? (i + 1) % tabs.length : (i - 1 + tabs.length) % tabs.length;
      e.preventDefault();
      selectTab(tabs[n], true);
    }
  });
  d.addEventListener("pointerdown", function (e) {
    var handle = e.target.closest && e.target.closest("[data-sortable-handle]");
    if (handle) startSort(e, handle);
  });

  /* --------------------------------------------------------------- forms
     Plain (non-htmx) form posts: show the button spinner and block a double
     submit. Server-rendered errors: focus the first invalid field.
     <form data-warn-unsaved>: ask before leaving with unsaved edits. */
  d.addEventListener("submit", function (e) {
    var form = e.target;
    if (e.defaultPrevented || !form.matches || form.matches("[hx-post], [hx-put], [hx-patch], [hx-delete], [hx-get], [data-no-busy]")) return;
    if (form._vvSubmitting) {
      e.preventDefault();
      return;
    }
    form._vvSubmitting = true;
    form._vvDirty = false;
    var btn = e.submitter && e.submitter.classList.contains("btn") ? e.submitter : $('button[type="submit"].btn, button:not([type]).btn', form);
    if (btn) {
      btn.classList.add("is-submitting");
      btn.setAttribute("aria-busy", "true");
    }
    /* bfcache / back navigation: re-enable */
    window.addEventListener(
      "pageshow",
      function () {
        form._vvSubmitting = false;
        if (btn) {
          btn.classList.remove("is-submitting");
          btn.removeAttribute("aria-busy");
        }
      },
      { once: true }
    );
  });
  d.addEventListener("input", function (e) {
    var form = e.target.form;
    if (form && form.hasAttribute("data-warn-unsaved")) form._vvDirty = true;
  });
  window.addEventListener("beforeunload", function (e) {
    var dirty = $$("form[data-warn-unsaved]").some(function (f) {
      return f._vvDirty;
    });
    if (dirty) {
      e.preventDefault();
      e.returnValue = "";
    }
  });
  function focusFirstError(scope) {
    var bad = $('[aria-invalid="true"]', scope);
    if (bad && !bad._vvFocused) {
      bad._vvFocused = true;
      bad.focus({ preventScroll: false });
    }
  }

  /* ------------------------------------------------------------ top bar */
  /* One passive listener; the bar is looked up each time because boosted
     navigation replaces it. */
  function checkScroll() {
    var bar = $(".topbar");
    if (bar) bar.classList.toggle("is-scrolled", window.scrollY > 2);
  }
  function watchScroll() {
    if (!watchScroll.on) {
      watchScroll.on = true;
      window.addEventListener("scroll", checkScroll, { passive: true });
    }
    checkScroll();
  }

  /* --------------------------------------------------------------- init
     Runs on load and for every piece of content htmx swaps in. */
  function init(scope) {
    scope = scope || d;
    initToasts(scope);
    initSegmented(scope);
    updateTimes(scope);
    updateShowWhen(scope.closest ? scope.closest("form") || scope : scope);
    $$("dialog.sheet", scope).forEach(wireSheet);
    $$("[data-kv][data-kv-json]", scope).forEach(kvSync);
    $$("[data-tabs]", scope).forEach(function (tl) {
      var sel = $('[role="tab"][aria-selected="true"]', tl) || $('[role="tab"]', tl);
      if (sel && !sel._vvTabInit) {
        sel._vvTabInit = true;
        selectTab(sel, false);
      }
    });
    $$("[data-sortable]", scope).forEach(writeOrder);
    detectTimezone(scope);
    $$("input[data-password-strength]", scope).forEach(function (i) {
      if (i.value) updateStrength(i);
    });
    focusFirstError(scope);
    /* Scroll the active settings pill into view on phones */
    var active = $('.settings-nav [aria-current="page"]', scope);
    if (active && active.scrollIntoView && window.matchMedia("(max-width: 860px)").matches) {
      active.scrollIntoView({ block: "nearest", inline: "center" });
    }
  }
  VV.init = init;

  function boot() {
    syncThemeUi();
    watchScroll();
    init(d);
    setInterval(function () {
      updateTimes(d);
    }, 30000);
    if (window.htmx) {
      d.body.addEventListener("htmx:load", function (e) {
        if (e.target !== d.body) init(e.target);
      });
      d.addEventListener("htmx:afterSettle", function (e) {
        /* boosted navigation swaps <body> content: re-run page-level setup */
        if (e.detail && e.detail.boosted) {
          syncThemeUi();
          watchScroll();
        }
      });
    }
  }
  if (d.readyState === "loading") d.addEventListener("DOMContentLoaded", boot);
  else boot();
})();
