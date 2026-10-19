"""Roles, two-factor login with recovery codes, API tokens and /metrics."""

from __future__ import annotations

import re
import time

import pyotp
from playwright.sync_api import APIRequestContext, Page, expect

from conftest import App, ConsoleWatch, ContextFactory
from fixture_servers import HttpTarget
from helpers import check_now, create_monitor_http, create_user_http, ui_login, ui_logout


def test_viewer_sees_no_edit_controls_and_cannot_post(
    app: App, admin_page: Page, admin_api: APIRequestContext, new_context: ContextFactory, http_target: HttpTarget
) -> None:
    mid = create_monitor_http(admin_api, monitor_type="http", name="Viewer can look", target_url=http_target.url)
    check_now(admin_api, mid)

    page = admin_page
    page.goto("/settings/users")
    page.get_by_role("button", name="New user").click()
    sheet = page.locator("#new-user")
    sheet.get_by_label("Username").fill("vera")
    sheet.locator("label").filter(has=page.locator('input[name="role"][value="viewer"]')).click()
    sheet.get_by_label("Temporary password").fill("Quiet-Meadow-Lookout-31")
    sheet.get_by_role("button", name="Add user").click()
    expect(page.locator(".toast")).to_contain_text("Added vera")
    expect(page.locator("li.list-row").filter(has_text="vera")).to_contain_text("Viewer")

    viewer = new_context().new_page()
    viewer.goto("/login")
    ui_login(viewer, "vera", "Quiet-Meadow-Lookout-31")
    expect(viewer).to_have_url(app.url("/"))
    expect(viewer.get_by_role("link", name="Viewer can look")).to_be_visible()
    expect(viewer.get_by_role("link", name="New monitor")).to_have_count(0)
    expect(viewer.get_by_role("link", name="Select")).to_have_count(0)

    viewer.goto(f"/monitors/{mid}")
    expect(viewer.get_by_role("heading", name="Viewer can look")).to_be_visible()
    for name in ("Edit", "Check now", "Pause"):
        expect(viewer.get_by_role("link", name=name)).to_have_count(0)
        expect(viewer.get_by_role("button", name=name)).to_have_count(0)

    viewer.goto("/settings")
    expect(viewer.get_by_role("link", name="Users")).to_have_count(0)

    origin = {"Origin": app.base_url}
    assert viewer.request.post(f"/monitors/{mid}/pause", headers=origin, form={"return_to": "/"}, max_redirects=0).status == 403
    assert viewer.request.post(f"/monitors/{mid}/edit", headers=origin, form={"name": "hijack"}, max_redirects=0).status == 403
    assert viewer.request.delete(f"/monitors/{mid}", headers=origin).status == 403
    assert viewer.request.get(f"/monitors/{mid}/edit", max_redirects=0).status == 403
    # The monitor is untouched.
    viewer.goto(f"/monitors/{mid}")
    expect(viewer.get_by_role("heading", name="Viewer can look")).to_be_visible()


def test_two_factor_login_and_single_use_recovery_code(
    app: App, admin_api: APIRequestContext, new_context: ContextFactory, console: ConsoleWatch
) -> None:
    password = "Second-Factor-Burrow-88"
    create_user_http(admin_api, "tess", password, "editor")
    page = new_context().new_page()
    page.goto("/login")
    ui_login(page, "tess", password)
    expect(page).to_have_url(app.url("/"))

    page.goto("/settings/security")
    page.get_by_role("button", name="Set up two-factor").click()
    expect(page.locator(".qr img")).to_be_visible()
    secret = re.sub(r"\s+", "", page.locator("#totp-secret").inner_text())
    totp = pyotp.TOTP(secret)
    page.get_by_label("6-digit code").fill(totp.now())
    page.get_by_role("button", name="Turn on").click()
    expect(page.locator(".toast")).to_contain_text("Two-factor authentication is on")
    codes = page.locator("#recovery-codes").inner_text().split()
    assert len(codes) == 10, codes

    ui_logout(page)
    ui_login(page, "tess", password)
    expect(page).to_have_url(app.url("/login/2fa"))
    # The enrolment code's time step can't be replayed, so use the next one (within the allowed skew).
    page.get_by_label("Code").fill(totp.at(time.time() + 30))
    page.get_by_role("button", name="Verify").click()
    expect(page).to_have_url(app.url("/"))

    ui_logout(page)
    ui_login(page, "tess", password)
    page.get_by_label("Code").fill(codes[0])
    page.get_by_role("button", name="Verify").click()
    expect(page).to_have_url(app.url("/"))
    expect(page.locator(".toast")).to_contain_text("recovery code")

    ui_logout(page)
    console.allow(r"status of 40[01]")
    ui_login(page, "tess", password)
    page.get_by_label("Code").fill(codes[0])
    page.get_by_role("button", name="Verify").click()
    expect(page).to_have_url(app.url("/login/2fa"))
    expect(page.get_by_role("alert")).to_contain_text("That code didn’t work")

    # A different recovery code still works.
    page.get_by_label("Code").fill(codes[1])
    page.get_by_role("button", name="Verify").click()
    expect(page).to_have_url(app.url("/"))


def create_token_ui(page: Page, name: str, scope_label: str, *, reload: bool = True) -> str:
    if reload:
        page.goto("/settings/tokens")
    page.get_by_role("button", name="New token").click()
    sheet = page.get_by_role("dialog", name="New token")
    expect(sheet).to_be_visible()
    sheet.get_by_label("Name").fill(name)
    sheet.locator("label.choice").filter(has_text=scope_label).click()
    sheet.get_by_role("button", name="Create token").click()
    expect(page.locator(".toast")).to_contain_text("Token created")
    token = page.locator(".copy-value").first.inner_text().strip()
    assert re.fullmatch(r"vv_[A-Za-z0-9]{43}", token), token
    return token


def test_api_tokens_scopes_and_metrics(app: App, admin_page: Page, http_target: HttpTarget) -> None:
    read = create_token_ui(admin_page, "Grafana reader", "Read only")
    # Straight from the "token created" page, without navigating away first.
    write = create_token_ui(admin_page, "Deploy script", "Read and write", reload=False)
    expect(admin_page.locator("tr").filter(has_text="Grafana reader")).to_be_visible()

    api = app.pw.request.new_context(base_url=app.base_url)  # no cookies, no Origin
    try:

        def auth(token: str) -> dict[str, str]:
            return {"Authorization": f"Bearer {token}"}

        assert api.get("/api/v1/monitors").status == 401
        listed = api.get("/api/v1/monitors", headers=auth(read))
        assert listed.status == 200 and "data" in listed.json()

        body = {"name": "Made by token", "monitor_type": "http", "target": http_target.url, "interval_seconds": 60}
        denied = api.post("/api/v1/monitors", headers=auth(read), data=body)
        assert denied.status == 403, denied.status
        created = api.post("/api/v1/monitors", headers=auth(write), data=body)
        assert created.status == 201, (created.status, created.text())
        assert created.json()["name"] == "Made by token"

        # Tokens never open the web UI.
        assert api.get("/", headers=auth(write), max_redirects=0).status in (302, 303, 401)

        # /metrics is off until enabled in Integrations.
        assert api.get("/metrics", headers=auth(read)).status in (403, 404)
        admin_page.goto("/settings/integrations")
        admin_page.get_by_role("switch", name="Enable /metrics").check()
        admin_page.get_by_role("button", name=re.compile("^Save")).click()
        expect(admin_page.locator(".toast")).to_be_visible()
        metrics = api.get("/metrics", headers=auth(read))
        assert metrics.status == 200, metrics.status
        text = metrics.text()
        assert "monitor_status" in text and "Made by token" in text
        assert api.get("/metrics").status == 401
    finally:
        api.dispose()
