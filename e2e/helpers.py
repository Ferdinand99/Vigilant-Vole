"""Shared UI/HTTP helpers for the e2e tests."""

from __future__ import annotations

import re
from typing import Any

from playwright.sync_api import APIRequestContext, Page, expect

TARGET_FIELD = {
    "http": "#f-target_url",
    "keyword": "#f-target_url",
    "json_query": "#f-target_url",
    "tcp": "#f-target_hostport",
    "ping": "#f-target_host",
}

STATUS_PILL = ".page-title-group .pill"


def monitor_id_from_url(url: str) -> int:
    m = re.search(r"/monitors/(\d+)", url)
    assert m, f"not a monitor URL: {url}"
    return int(m.group(1))


def choose_monitor_type(page: Page, kind: str) -> None:
    page.locator("label.choice").filter(has=page.locator(f'input[name="monitor_type"][value="{kind}"]')).click()


def create_monitor_ui(
    page: Page,
    kind: str,
    name: str,
    target: str | None = None,
    *,
    interval: int | None = None,
    retries: int | None = 0,
    keyword: str | None = None,
    group: str | None = None,
    start_url: str = "/monitors/new",
) -> int:
    """Fills the monitor form like a person would and returns the new id."""
    page.goto(start_url)
    choose_monitor_type(page, kind)
    page.locator("#f-name").fill(name)
    if target is not None:
        page.locator(TARGET_FIELD[kind]).fill(target)
    if interval is not None:
        page.locator("#f-interval_seconds").fill(str(interval))
    if retries is not None and kind not in ("group",):
        page.locator("#f-retries").fill(str(retries))
    if keyword is not None:
        page.locator("#f-keyword").fill(keyword)
    if group is not None:
        page.locator("#f-parent_id").select_option(label=group)
    page.get_by_role("button", name="Create monitor").click()
    expect(page).to_have_url(re.compile(r"/monitors/\d+$"))
    expect(page.get_by_role("heading", name=name, level=1)).to_be_visible()
    return monitor_id_from_url(page.url)


def create_monitor_http(client: APIRequestContext, **fields: Any) -> int:
    """Posts the monitor form directly (faster set-up for tests about other things)."""
    form = {
        "retries": "0",
        "interval_seconds": "60",
        "timeout_seconds": "5",
        "http_method": "GET",
        "accepted_status_codes": "200-299",
        "max_redirects": "5",
        "http_body_encoding": "json",
        "json_operator": "==",
        "dns_record_type": "A",
        "expiry_days": "14",
        "resend_interval": "0",
        "escalation_minutes": "0",
        **{k: str(v) for k, v in fields.items()},
    }
    r = client.post("/monitors/new", form=form, max_redirects=0)
    if r.status not in (302, 303):
        errors = re.findall(r'class="field-error"[^>]*>(?:<svg.*?</svg>)?([^<]*)', r.text(), re.S)
        raise AssertionError(f"create monitor failed: {r.status} {errors}")
    return monitor_id_from_url(r.headers["location"])


def check_now(client: APIRequestContext, monitor_id: int) -> None:
    r = client.post(f"/monitors/{monitor_id}/check", max_redirects=0)
    assert r.status in (302, 303), f"check failed: {r.status} {r.text()[:300]}"


def expect_status(page: Page, label: str) -> None:
    expect(page.locator(STATUS_PILL)).to_have_text(label)


def ui_login(page: Page, username: str, password: str) -> None:
    page.get_by_label("Username").fill(username)
    page.get_by_label("Password", exact=True).fill(password)
    page.get_by_role("button", name="Log in", exact=True).click()


def ui_logout(page: Page) -> None:
    page.get_by_role("button", name="Account and appearance").click()
    page.get_by_role("button", name="Log out").click()
    expect(page).to_have_url(re.compile(r"/login$"))


def create_user_http(admin: APIRequestContext, username: str, password: str, role: str) -> None:
    r = admin.post(
        "/settings/users",
        form={"username": username, "display_name": "", "role": role, "password": password},
        max_redirects=0,
    )
    assert r.status in (302, 303), f"create user failed: {r.status}"
    listing = admin.get("/settings/users").text()
    assert f'<span class="list-sub">{username} ·' in listing, f"user {username} was not created"
