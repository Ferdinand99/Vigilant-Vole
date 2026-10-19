"""Session fixtures for the Vigilant Vole end-to-end suite.

* builds the binary once (``cargo build``; skip with ``VV_E2E_BIN=<path>``),
* starts it on a free port with a throw-away ``DATA_DIR`` and waits for
  ``/healthz``; stops it by PID afterwards,
* creates the admin account over HTTP so most tests can start logged in,
* local fixture servers (HTTP target, TCP listener, webhook capture),
* browser contexts that fail the test on any console error, uncaught page
  error or CSP violation.

By default every test runs on Chromium and the tests marked ``smoke`` also
run on Firefox. Pass ``--browser`` to choose yourself.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Iterator

import pytest
from playwright.sync_api import APIRequestContext, Browser, BrowserContext, Page, Playwright, expect

from fixture_servers import HttpTarget, TcpListener, WebhookCapture, free_port

REPO = Path(__file__).resolve().parent.parent
ADMIN_USER = "admin"
ADMIN_PASSWORD = "Burrowing-Vole-Passphrase-42"
SITE_NAME = "E2E Vole"

expect.set_options(timeout=10_000)


# ---------------------------------------------------------------------------
# Browser selection: chromium for everything, firefox for the smoke subset
# ---------------------------------------------------------------------------


def pytest_configure(config: pytest.Config) -> None:
    config.addinivalue_line("markers", "smoke: also runs on Firefox")
    if not config.getoption("--browser", default=None):
        config.option.browser = ["chromium", "firefox"]


def pytest_collection_modifyitems(config: pytest.Config, items: list[pytest.Item]) -> None:
    keep, drop = [], []
    for item in items:
        browser = getattr(item, "callspec", None) and item.callspec.params.get("browser_name")
        if browser and browser != "chromium" and item.get_closest_marker("smoke") is None:
            drop.append(item)
        else:
            keep.append(item)
    if drop:
        config.hook.pytest_deselected(items=drop)
        items[:] = keep


# ---------------------------------------------------------------------------
# The app under test
# ---------------------------------------------------------------------------


def _binary_path() -> Path:
    override = os.environ.get("VV_E2E_BIN")
    if override:
        return Path(override)
    target = Path(os.environ.get("CARGO_TARGET_DIR") or REPO / "target")
    if not target.is_absolute():
        target = REPO / target
    if not os.environ.get("VV_E2E_SKIP_BUILD"):
        subprocess.run(["cargo", "build", "--quiet"], cwd=REPO, check=True)
    exe = "vigilant-vole.exe" if sys.platform == "win32" else "vigilant-vole"
    return target / "debug" / exe


@pytest.fixture(scope="session")
def binary() -> Path:
    path = _binary_path()
    assert path.exists(), f"binary not found at {path}"
    return path


@dataclass
class App:
    base_url: str
    port: int
    data_dir: Path
    log_path: Path
    proc: subprocess.Popen[bytes]
    pw: Playwright | None = None
    _clients: list[APIRequestContext] = field(default_factory=list)

    def url(self, path: str) -> str:
        return self.base_url + path

    def client(self, **kwargs: Any) -> APIRequestContext:
        """An HTTP client that passes the CSRF check (same-origin Origin header)."""
        assert self.pw is not None
        headers = {"Origin": self.base_url, **kwargs.pop("extra_http_headers", {})}
        ctx = self.pw.request.new_context(base_url=self.base_url, extra_http_headers=headers, **kwargs)
        self._clients.append(ctx)
        return ctx

    def login_client(self, username: str, password: str) -> APIRequestContext:
        c = self.client()
        r = c.post("/login", form={"username": username, "password": password, "next": "/"}, max_redirects=0)
        assert r.status == 303 or r.status == 302, f"login as {username} failed: {r.status} {r.text()[:300]}"
        return c

    def log_tail(self, lines: int = 40) -> str:
        try:
            return "\n".join(self.log_path.read_text(errors="replace").splitlines()[-lines:])
        except OSError:
            return ""

    def stop(self) -> None:
        for c in self._clients:
            try:
                c.dispose()
            except Exception:  # noqa: BLE001 - best-effort cleanup
                pass
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=10)


def start_app(binary: Path, pw: Playwright) -> App:
    data_dir = Path(tempfile.mkdtemp(prefix="vv-e2e-"))
    port = free_port()
    log_path = data_dir / "server.log"
    env = {
        **os.environ,
        "DATA_DIR": str(data_dir),
        "PORT": str(port),
        "RUST_LOG": "warn,vigilant_vole=info",
    }
    for key in ("VV_SETUP_TOKEN", "VV_CONFIG_FILE", "VV_TRUST_PROXY"):
        env.pop(key, None)
    log = open(log_path, "wb")  # noqa: SIM115 - closed with the process
    proc = subprocess.Popen([str(binary)], cwd=data_dir, env=env, stdout=log, stderr=subprocess.STDOUT)
    app = App(base_url=f"http://127.0.0.1:{port}", port=port, data_dir=data_dir, log_path=log_path, proc=proc, pw=pw)
    deadline = time.monotonic() + 60
    while True:
        if proc.poll() is not None:
            raise RuntimeError(f"server exited early ({proc.returncode}):\n{app.log_tail()}")
        try:
            with urllib.request.urlopen(app.url("/healthz"), timeout=2) as resp:  # noqa: S310 - localhost
                if resp.status == 200:
                    break
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            pass
        if time.monotonic() > deadline:
            app.stop()
            raise RuntimeError(f"server did not become healthy:\n{app.log_tail()}")
        time.sleep(0.1)  # startup poll only; tests never sleep
    return app


def stop_app(app: App) -> None:
    app.stop()
    shutil.rmtree(app.data_dir, ignore_errors=True)


def run_setup(app: App) -> None:
    c = app.client()
    r = c.post(
        "/setup",
        form={
            "username": ADMIN_USER,
            "password": ADMIN_PASSWORD,
            "confirm": ADMIN_PASSWORD,
            "site_name": SITE_NAME,
            "timezone": "UTC",
        },
        max_redirects=0,
    )
    assert r.status in (302, 303), f"setup failed: {r.status} {r.text()[:500]}"


@pytest.fixture(scope="session")
def app(binary: Path, playwright: Playwright) -> Iterator[App]:
    """One server for the whole session, already set up with an admin."""
    instance = start_app(binary, playwright)
    try:
        run_setup(instance)
        yield instance
    finally:
        stop_app(instance)


@pytest.fixture()
def fresh_app(binary: Path, playwright: Playwright) -> Iterator[App]:
    """A brand-new server that still shows the setup wizard."""
    instance = start_app(binary, playwright)
    try:
        yield instance
    finally:
        stop_app(instance)


@pytest.fixture(scope="session")
def admin_api(app: App) -> APIRequestContext:
    """Cookie-authenticated HTTP client for the admin (passes CSRF via Origin)."""
    return app.login_client(ADMIN_USER, ADMIN_PASSWORD)


@pytest.fixture(scope="session")
def admin_state(admin_api: APIRequestContext) -> dict[str, Any]:
    return admin_api.storage_state()


# ---------------------------------------------------------------------------
# Local fixture servers
# ---------------------------------------------------------------------------


@pytest.fixture(scope="session")
def http_target() -> Iterator[HttpTarget]:
    t = HttpTarget()
    yield t
    t.close()


@pytest.fixture(scope="session")
def tcp_listener() -> Iterator[TcpListener]:
    t = TcpListener()
    yield t
    t.close()


@pytest.fixture(scope="session")
def webhook() -> Iterator[WebhookCapture]:
    w = WebhookCapture()
    yield w
    w.close()


# ---------------------------------------------------------------------------
# Browser contexts with console / CSP watching
# ---------------------------------------------------------------------------

CSP_HOOK = """
document.addEventListener('securitypolicyviolation', (e) => {
  console.error('CSP violation: ' + e.violatedDirective + ' blocked ' + (e.blockedURI || 'inline') + ' in ' + e.sourceFile + ':' + e.lineNumber);
}, true);
"""


class ConsoleWatch:
    """Collects console errors, page errors and CSP violations."""

    def __init__(self) -> None:
        self.errors: list[str] = []
        self._allowed: list[re.Pattern[str]] = []

    def attach(self, page: Page) -> None:
        page.on("console", lambda msg: self._console(page, msg))
        page.on("pageerror", lambda err: self.errors.append(f"[{page.url}] pageerror: {err}"))

    def _console(self, page: Page, msg: Any) -> None:
        text = msg.text
        if msg.type == "error" or "Content-Security-Policy" in text or "Content Security Policy" in text:
            self.errors.append(f"[{page.url}] console.{msg.type}: {text}")

    def allow(self, pattern: str) -> None:
        """Expected noise for this test (e.g. a deliberate 401 page)."""
        self._allowed.append(re.compile(pattern))

    def unexpected(self) -> list[str]:
        return [e for e in self.errors if not any(p.search(e) for p in self._allowed)]


@pytest.fixture()
def console() -> ConsoleWatch:
    return ConsoleWatch()


ContextFactory = Callable[..., BrowserContext]


@pytest.fixture()
def new_context(browser: Browser, app: App, console: ConsoleWatch) -> Iterator[ContextFactory]:
    """Creates browser contexts; every page in them is watched for errors."""
    contexts: list[BrowserContext] = []

    def make(base_url: str | None = None, **kwargs: Any) -> BrowserContext:
        ctx = browser.new_context(base_url=base_url or app.base_url, **kwargs)
        ctx.add_init_script(CSP_HOOK)
        ctx.on("page", console.attach)
        contexts.append(ctx)
        return ctx

    yield make
    for ctx in contexts:
        ctx.close()
    problems = console.unexpected()
    assert not problems, "browser reported errors:\n" + "\n".join(problems)


@pytest.fixture()
def admin_page(new_context: ContextFactory, admin_state: dict[str, Any]) -> Page:
    """A logged-in admin page (fresh context, shared session cookie)."""
    return new_context(storage_state=admin_state).new_page()


@pytest.fixture()
def anon_page(new_context: ContextFactory) -> Page:
    return new_context().new_page()
