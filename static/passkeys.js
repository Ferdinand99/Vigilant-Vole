/* Passkeys (vole-kit): log in with a passkey, and add one under Settings →
   Security. The login page loads this when the instance's public URL allows
   passkeys, and every signed-in page loads it, because the settings menu
   reaches Security by a boosted (htmx) navigation that doesn't run the new
   page's scripts. So it wires up whatever is on the page now, at load and
   after every htmx swap, and each element only once. The buttons stay
   hidden unless this page is on the public URL's origin in a browser that
   has WebAuthn; otherwise Security says which of the two is missing. The
   server checks everything (vole_kit::auth::webauthn); this only carries
   bytes between it and navigator.credentials. No inline script (CSP): texts
   come in data-msg-* attributes. */
(function () {
  "use strict";
  var d = document;
  if (window.VVPasskeys) return;
  window.VVPasskeys = true;

  /* Elements already wired up. Not an attribute: htmx keeps pages in its
     history cache as HTML, and a restored copy needs wiring up again. */
  var done = typeof WeakSet === "function" ? new WeakSet() : null;
  function once(el) {
    if (!el) return false;
    if (done) {
      if (done.has(el)) return false;
      done.add(el);
      return true;
    }
    if (el.vvPasskeys) return false;
    el.vvPasskeys = true;
    return true;
  }

  /* ---------------------------------------------------- base64url helpers */
  function toBytes(text) {
    var b64 = text.replace(/-/g, "+").replace(/_/g, "/");
    while (b64.length % 4) b64 += "=";
    var raw = atob(b64);
    var out = new Uint8Array(raw.length);
    for (var i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
    return out;
  }
  function toText(buffer) {
    var bytes = new Uint8Array(buffer);
    var raw = "";
    for (var i = 0; i < bytes.length; i++) raw += String.fromCharCode(bytes[i]);
    return btoa(raw).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }

  function supported() {
    return !!(window.PublicKeyCredential && navigator.credentials && window.isSecureContext);
  }
  /* Passkeys only work on the public URL's origin (the RP ID is its host). */
  function usableAt(origin) {
    return supported() && location.origin === origin;
  }

  function creationOptions(o) {
    o.challenge = toBytes(o.challenge);
    o.user.id = toBytes(o.user.id);
    (o.excludeCredentials || []).forEach(function (c) { c.id = toBytes(c.id); });
    return o;
  }
  function requestOptions(o) {
    o.challenge = toBytes(o.challenge);
    (o.allowCredentials || []).forEach(function (c) { c.id = toBytes(c.id); });
    return o;
  }

  /* PublicKeyCredential.toJSON(), by hand (older browsers lack it). */
  function credentialJSON(cred) {
    var r = cred.response;
    var out = { id: cred.id, rawId: toText(cred.rawId), type: cred.type, response: { clientDataJSON: toText(r.clientDataJSON) } };
    if (r.attestationObject) {
      out.response.attestationObject = toText(r.attestationObject);
      out.response.transports = typeof r.getTransports === "function" ? r.getTransports() : [];
    }
    if (r.authenticatorData) {
      out.response.authenticatorData = toText(r.authenticatorData);
      out.response.signature = toText(r.signature);
      out.response.userHandle = r.userHandle ? toText(r.userHandle) : null;
    }
    return out;
  }

  function post(url, body, json) {
    return fetch(url, {
      method: "POST",
      credentials: "same-origin",
      headers: {
        Accept: "application/json",
        "Content-Type": json ? "application/json" : "application/x-www-form-urlencoded"
      },
      body: body
    }).then(function (res) {
      return res.json().catch(function () { return {}; }).then(function (data) {
        if (!res.ok) throw { server: data && data.error };
        return data;
      });
    });
  }

  /* The text for a failure: the server's own, or one for the browser's error. */
  function message(err, el) {
    var msg = function (name) { return el.getAttribute("data-msg-" + name) || ""; };
    if (err && err.server) return err.server;
    var name = err && err.name;
    if (name === "NotAllowedError" || name === "AbortError") return msg("cancelled");
    if (name === "InvalidStateError" && el.hasAttribute("data-msg-exists")) return msg("exists");
    if (name === "NotSupportedError") return msg("unsupported");
    return msg("failed");
  }

  /* After success: go where the server says (reload when it's this page). */
  function go(target) {
    var url = new URL(target || "/", location.href);
    if (url.pathname === location.pathname && url.search === location.search) {
      location.hash = url.hash;
      location.reload();
    } else {
      location.assign(url.href);
    }
  }

  /* ------------------------------------------------------------- log in */
  function setupSignin(signin) {
    if (!usableAt(signin.getAttribute("data-origin"))) return;
    var form = signin.closest("form");
    var autofill = null;
    var busy = false;

    var showError = function (text) {
      if (!text) return;
      var box = d.querySelector("[data-passkey-error]");
      if (!box) {
        box = d.createElement("div");
        box.className = "callout status-danger";
        box.setAttribute("role", "alert");
        box.setAttribute("data-passkey-error", "");
        var ns = "http://www.w3.org/2000/svg";
        var svg = d.createElementNS(ns, "svg");
        svg.setAttribute("class", "icon");
        svg.setAttribute("aria-hidden", "true");
        var use = d.createElementNS(ns, "use");
        use.setAttribute("href", "/static/icons.svg#alert-circle");
        svg.appendChild(use);
        var body = d.createElement("div");
        body.className = "callout-body";
        box.appendChild(svg);
        box.appendChild(body);
        form.parentNode.insertBefore(box, form);
      }
      box.querySelector(".callout-body").textContent = text;
    };

    var logIn = function (mediation) {
      var controller = null;
      return post("/login/passkey/options", "", false)
        .then(function (data) {
          var options = { publicKey: requestOptions(data.publicKey) };
          if (mediation) {
            controller = new AbortController();
            autofill = controller;
            options.mediation = mediation;
            options.signal = controller.signal;
            /* The challenge lasts five minutes: start over before it ends. */
            setTimeout(function () {
              if (autofill === controller) {
                controller.abort();
                startAutofill();
              }
            }, Math.max(30000, (data.publicKey.timeout || 300000) - 15000));
          }
          return navigator.credentials.get(options);
        })
        .then(function (cred) {
          if (!cred) throw { name: "NotAllowedError" };
          var next = form && form.querySelector('[name="next"]');
          busy = true;
          return post("/login/passkey", JSON.stringify({ credential: credentialJSON(cred), next: next ? next.value : "/" }), true);
        })
        .then(function (data) {
          go(data.redirect);
        });
    };

    var autofillOn = false;
    var failures = 0;
    var startAutofill = function () {
      logIn("conditional").catch(function (err) {
        busy = false;
        /* Aborted on purpose (the button, or a fresh challenge): stay quiet. */
        if (err && err.name === "AbortError") return;
        if (err && err.server) showError(err.server);
        /* Try again a little later, but never in a tight loop. */
        failures += 1;
        if (failures < 3) setTimeout(startAutofill, 1500);
      });
    };

    signin.hidden = false;
    signin.addEventListener("click", function () {
      if (busy) return;
      if (autofill) {
        autofill.abort();
        autofill = null;
      }
      busy = true;
      signin.disabled = true;
      logIn(null)
        .catch(function (err) {
          busy = false;
          showError(message(err, signin));
        })
        .then(function () {
          signin.disabled = false;
          if (!busy && autofillOn) startAutofill();
        });
    });

    /* The browser's own suggestion list on the username field
       (autocomplete="username webauthn"). */
    if (typeof window.PublicKeyCredential.isConditionalMediationAvailable === "function") {
      window.PublicKeyCredential.isConditionalMediationAvailable().then(function (ok) {
        if (!ok) return;
        autofillOn = true;
        startAutofill();
      }).catch(function () {});
    }
  }

  /* --------------------------------------------- Settings → Security */
  /* The note says why there is no button: another address than the public
     URL (the server's text, with the link), or a browser without WebAuthn.
     On the right address in a browser that has it, the note goes and the
     button comes. */
  function setupSecurity(note) {
    var add = d.querySelector("[data-passkey-add]");
    var register = d.querySelector("form[data-passkey-register]");
    if (location.origin !== note.getAttribute("data-origin")) return;
    if (!supported()) {
      var body = note.querySelector(".callout-body");
      var text = note.getAttribute("data-msg-unsupported");
      if (body && text) body.textContent = text;
      return;
    }
    if (!add || !register) return;
    note.hidden = true;
    add.hidden = false;
    if (!once(register)) return;
    var submit = d.querySelector('button[type="submit"][form="' + register.id + '"]');
    var adding = false;
    register.addEventListener("submit", function (e) {
      e.preventDefault();
      if (adding) return;
      adding = true;
      if (submit) submit.disabled = true;
      var name = register.elements.name ? register.elements.name.value : "";
      var form = new URLSearchParams(new FormData(register)).toString();
      post(register.action, form, false)
        .then(function (data) {
          return navigator.credentials.create({ publicKey: creationOptions(data.publicKey) });
        })
        .then(function (cred) {
          if (!cred) throw { name: "NotAllowedError" };
          return post("/settings/security/passkeys", JSON.stringify({ name: name, credential: credentialJSON(cred) }), true);
        })
        .then(function (data) {
          go(data.redirect);
        })
        .catch(function (err) {
          adding = false;
          if (submit) submit.disabled = false;
          var text = message(err, register);
          if (window.VV && window.VV.toast) window.VV.toast(text, "error");
        });
    });
  }

  /* Whatever this page has that isn't wired up yet. A no-op elsewhere. */
  function init() {
    var signin = d.querySelector("[data-passkey-signin]");
    if (once(signin)) setupSignin(signin);
    var note = d.querySelector("[data-passkey-note]");
    if (once(note)) setupSecurity(note);
  }

  init();
  /* htmx swaps (boosted navigation, partial updates, the back button). */
  ["htmx:load", "htmx:afterSettle", "htmx:historyRestore"].forEach(function (name) {
    d.addEventListener(name, init);
  });
})();
