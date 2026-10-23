# End-to-end tests

Browser tests for Vigilant Vole, written with [Playwright for Python](https://playwright.dev/python/) and pytest.
They build the binary, start it on a free port with a throw-away `DATA_DIR`, drive it through Chromium (and
Firefox for the smoke subset), and stop it again. Nothing touches your real data or a running instance.

## Run

```bash
python -m venv .venv-e2e
. .venv-e2e/bin/activate            # Windows: .venv-e2e\Scripts\activate
pip install -r e2e/requirements.txt
python -m playwright install chromium firefox

pytest e2e                          # whole suite: everything on Chromium, `smoke` tests also on Firefox (~1–2 min)
pytest e2e --browser chromium       # Chromium only
pytest e2e --browser firefox -m smoke
pytest e2e -k status_page --headed  # watch one test
```

Run from the repository root. The first run compiles the binary with `cargo build` (debug profile).

| Environment variable | Effect |
|---|---|
| `CARGO_TARGET_DIR` | Where `cargo build` puts (and the suite looks for) the binary |
| `VV_E2E_BIN` | Use this binary instead of building one |
| `VV_E2E_SKIP_BUILD` | Don't run `cargo build`; use the existing debug binary |

Failures keep a Playwright trace in `test-results/` (`playwright show-trace test-results/<test>/trace.zip`).
The server log of a run lives in its temporary `DATA_DIR` (`server.log`) until the run ends.

## What's covered

| File | Flows |
|---|---|
| `test_auth.py` | Setup wizard → dashboard, logout/login, wrong password, `?next=` (and off-site `next` ignored), stale or foreign `vv_session` cookies don't block a fresh login |
| `test_monitors.py` | HTTP, keyword (found / missing), TCP inside a group, push (pushing flips it up/down), pause/resume, detail page (status, chart, events), dashboard turns a row red live over SSE without reloading |
| `test_notifications.py` | Webhook channel: “Send test”, then a real down alert and an up alert with the downtime |
| `test_status_pages.py` | Create page, section, add monitors, mouse drag reorder (persists), publish, pinned incident with Markdown, logged-out public page (no monitor targets leaked), JSON API, SVG badges |
| `test_service_monitors.py` | Redis monitor through the form (password never echoed, mask kept on save, check goes Up against a fake Redis), wrong password explained, switching type swaps the RADIUS / SNMP / credential fields |
| `test_maintenance.py` | Manual window puts a monitor into maintenance; ending it brings it back |
| `test_users_security.py` | Viewer sees no edit controls and gets 403 on writes; TOTP 2FA enrol + login, recovery codes are single-use; read/write API token scopes; `/metrics` after enabling it |
| `test_passkeys.py` | Chromium's virtual authenticator: public URL `http://localhost:<port>`, 2FA on, add a passkey under Security (password first), log out, log in with the passkey alone (no password, no code); no passkey button at another address |
| `test_ui_misc.py` | Config export (YAML) → import preview reports everything unchanged; Norwegian user language and `Accept-Language: nb` on a public page; 375 px phone layout (no sideways scroll, sheet opens/closes); theme choice persists; reduced motion |

## Conventions

- One server per session (`app` fixture, admin already created); tests use unique names so they don't depend on
  each other or on order. `fresh_app` gives a brand-new server for the setup wizard.
- Local fixture servers (`fixture_servers.py`): an HTTP target whose status can be flipped, a TCP listener, and a
  webhook capture server. Monitors are checked with “Check now” rather than by waiting for intervals.
- Every page is watched: a console error, uncaught exception or CSP violation fails the test. Expected noise
  (e.g. the 401 page after a wrong password) is allowed per test with `console.allow(regex)`.
- Role, label and text selectors first; ids where the UI repeats a label (e.g. the monitor form's “Name”).
- No sleeps: Playwright's auto-waiting `expect`, and a condition variable for webhook deliveries.
- Tests marked `@pytest.mark.smoke` also run on Firefox.
