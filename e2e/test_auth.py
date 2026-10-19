"""Setup wizard, login/logout, redirects and the stale-cookie regression."""

from __future__ import annotations

import re

import pytest
from playwright.sync_api import Page, expect

from conftest import ADMIN_PASSWORD, ADMIN_USER, App, ConsoleWatch, ContextFactory


def log_in(page: Page, username: str, password: str) -> None:
    page.get_by_label("Username").fill(username)
    page.get_by_label("Password", exact=True).fill(password)
    page.get_by_role("button", name="Log in").click()


def log_out(page: Page) -> None:
    page.get_by_role("button", name="Account and appearance").click()
    page.get_by_role("button", name="Log out").click()
    expect(page).to_have_url(re.compile(r"/login$"))


@pytest.mark.smoke
def test_setup_wizard_then_logout_and_login(fresh_app: App, new_context: ContextFactory, console: ConsoleWatch) -> None:
    page = new_context(base_url=fresh_app.base_url).new_page()
    page.goto("/")
    expect(page).to_have_url(fresh_app.url("/setup"))
    expect(page.get_by_role("heading", name="Welcome")).to_be_visible()

    page.get_by_label("Username").fill("founder")
    page.get_by_label("Password", exact=True).fill("Meadow-Tunnel-Lantern-77")
    page.get_by_label("Confirm password").fill("Meadow-Tunnel-Lantern-77")
    page.get_by_label("Site name").fill("Wizard Vole")
    page.get_by_role("button", name="Create account").click()

    expect(page).to_have_url(fresh_app.url("/"))
    expect(page.get_by_role("heading", name="Monitors", level=1)).to_be_visible()
    expect(page.locator(".brand")).to_contain_text("Wizard Vole")

    # The wizard is closed for good once an admin exists.
    page.goto("/setup")
    expect(page).not_to_have_url(fresh_app.url("/setup"))

    log_out(page)
    page.goto("/")
    expect(page).to_have_url(fresh_app.url("/login"))

    # Wrong password: stays on the form with a clear message.
    console.allow(r"status of 401")
    log_in(page, "founder", "not-the-password")
    expect(page.get_by_role("alert")).to_contain_text("Incorrect username or password.")
    expect(page.get_by_label("Username")).to_have_value("founder")

    log_in(page, "founder", "Meadow-Tunnel-Lantern-77")
    expect(page).to_have_url(fresh_app.url("/"))
    expect(page.get_by_role("heading", name="Monitors", level=1)).to_be_visible()


@pytest.mark.smoke
def test_login_honours_next(app: App, anon_page: Page) -> None:
    anon_page.goto("/settings/tokens")
    expect(anon_page).to_have_url(app.url("/login?next=%2Fsettings%2Ftokens"))
    log_in(anon_page, ADMIN_USER, ADMIN_PASSWORD)
    expect(anon_page).to_have_url(app.url("/settings/tokens"))
    expect(anon_page.get_by_role("heading", name="API tokens", level=1)).to_be_visible()


@pytest.mark.smoke
def test_login_rejects_offsite_next(app: App, anon_page: Page) -> None:
    anon_page.goto("/login?next=//evil.example/steal")
    log_in(anon_page, ADMIN_USER, ADMIN_PASSWORD)
    expect(anon_page).to_have_url(app.url("/"))


@pytest.mark.smoke
@pytest.mark.parametrize("variant", ["other-path", "foreign-signature", "same-path-garbage"])
def test_stale_session_cookie_does_not_block_login(app: App, new_context: ContextFactory, variant: str) -> None:
    """Regression: a leftover vv_session cookie (another path, or signed by an
    older install) used to shadow the fresh one, bouncing the user back to
    /login after a correct password (seen in Firefox)."""
    ctx = new_context()
    bogus = {
        "other-path": {"name": "vv_session", "value": "v1.1.1.1700000000", "path": "/monitors"},
        # Looks signed (base64 MAC prefix) but by a key this server never had.
        "foreign-signature": {
            "name": "vv_session",
            "value": "Zm9yZWlnbi1rZXktc2lnbmF0dXJlLXRoYXQtaXMtbG9uZw==v2.1.1.1700000000.1700000000",
            "path": "/",
        },
        "same-path-garbage": {"name": "vv_session", "value": "garbage", "path": "/"},
    }[variant]
    ctx.add_cookies([{**bogus, "domain": "127.0.0.1", "httpOnly": True, "sameSite": "Lax"}])
    if variant == "other-path":
        # A second stale cookie on a path that the dashboard links into.
        ctx.add_cookies([{"name": "vv_session", "value": "stale", "domain": "127.0.0.1", "path": "/settings"}])
    page = ctx.new_page()
    page.goto("/login")
    log_in(page, ADMIN_USER, ADMIN_PASSWORD)
    expect(page).to_have_url(app.url("/"))
    expect(page.get_by_role("heading", name="Monitors", level=1)).to_be_visible()

    # Pages under the stale cookie's path work too (both cookies are sent there).
    page.goto("/settings/security")
    expect(page).to_have_url(app.url("/settings/security"))
    expect(page.get_by_role("heading", name="Security", level=1)).to_be_visible()
    page.goto("/monitors/new")
    expect(page).to_have_url(app.url("/monitors/new"))
