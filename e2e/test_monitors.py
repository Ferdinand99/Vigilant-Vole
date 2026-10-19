"""Creating monitors through the form, the detail page and live dashboard updates."""

from __future__ import annotations

import re

from playwright.sync_api import Page, expect

from conftest import App
from fixture_servers import HttpTarget, TcpListener
from helpers import create_monitor_ui, expect_status


def test_http_monitor_detail_and_live_down_via_sse(app: App, admin_page: Page, http_target: HttpTarget) -> None:
    page = admin_page
    http_target.up()
    mid = create_monitor_ui(page, "http", "Burrow API", http_target.url, interval=60)

    page.get_by_role("button", name="Check now").click()
    expect(page.locator(".toast")).to_contain_text("Checked just now")
    expect_status(page, "Up")
    expect(page.get_by_role("heading", name="Response time")).to_be_visible()
    expect(page.locator("#chart-card svg")).to_be_visible()
    expect(page.get_by_role("heading", name="Events")).to_be_visible()
    expect(page.locator(".dl")).to_contain_text(http_target.url)

    # Dashboard: live indicator connects, then the row turns red without a reload.
    page.goto("/")
    row = page.locator(f"#monitor-row-{mid}")
    expect(row.locator(".dot").first).to_have_class(re.compile(r"status-up"))
    expect(page.get_by_text("Live", exact=True)).to_be_visible()
    page.evaluate("window.__noReload = 42")

    http_target.down()
    try:
        r = page.request.post(f"/monitors/{mid}/check", headers={"Origin": app.base_url}, max_redirects=0)
        assert r.status == 303, r.status
        expect(row.locator(".dot").first).to_have_class(re.compile(r"status-down"), timeout=15_000)
        assert page.evaluate("window.__noReload") == 42, "dashboard reloaded instead of updating live"
    finally:
        http_target.up()

    # Recovery also arrives live, and the detail page lists both events.
    page.request.post(f"/monitors/{mid}/check", headers={"Origin": app.base_url}, max_redirects=0)
    expect(row.locator(".dot").first).to_have_class(re.compile(r"status-up"), timeout=15_000)
    page.goto(f"/monitors/{mid}")
    timeline = page.locator(".timeline")
    expect(timeline).to_contain_text("Down")
    expect(timeline).to_contain_text("HTTP 500")
    expect(timeline).to_contain_text("Lasted")  # closed by the recovery


def test_keyword_monitor_pause_and_resume(admin_page: Page, http_target: HttpTarget) -> None:
    page = admin_page
    http_target.up()
    create_monitor_ui(page, "keyword", "Burrow greeting", http_target.url, keyword="vole burrow")
    page.get_by_role("button", name="Check now").click()
    expect_status(page, "Up")

    page.get_by_role("button", name="Pause").click()
    expect(page.locator(".toast")).to_contain_text("Paused")
    expect_status(page, "Paused")
    expect(page.locator(".callout")).to_contain_text("won’t send alerts")
    expect(page.get_by_role("button", name="Check now")).to_have_count(0)

    page.get_by_role("button", name="Resume").click()
    expect(page.locator(".toast")).to_contain_text("Resumed")
    expect(page.get_by_role("button", name="Pause")).to_be_visible()
    page.get_by_role("button", name="Check now").click()
    expect_status(page, "Up")


def test_keyword_monitor_missing_keyword_is_down(admin_page: Page, http_target: HttpTarget) -> None:
    page = admin_page
    http_target.up()
    create_monitor_ui(page, "keyword", "Missing word", http_target.url, keyword="hamster")
    page.get_by_role("button", name="Check now").click()
    expect_status(page, "Down")
    expect(page.locator(".callout")).to_contain_text("hamster")


def test_tcp_monitor_inside_group(admin_page: Page, tcp_listener: TcpListener) -> None:
    page = admin_page
    gid = create_monitor_ui(page, "group", "Core services")
    expect(page.get_by_role("heading", name="Monitors in this group")).to_be_visible()

    page.get_by_role("link", name="Add monitor").click()
    expect(page).to_have_url(re.compile(rf"/monitors/new\?parent={gid}$"))
    expect(page.locator("#f-parent_id")).to_have_value(str(gid))
    tid = create_monitor_ui(page, "tcp", "Burrow socket", tcp_listener.address, start_url=page.url)
    page.get_by_role("button", name="Check now").click()
    expect_status(page, "Up")
    expect(page.get_by_role("link", name="Core services")).to_be_visible()  # back link to the group

    page.goto(f"/monitors/{gid}")
    children = page.get_by_role("list").filter(has=page.get_by_text("Burrow socket"))
    expect(children).to_be_visible()
    expect(page.locator(f'a[href="/monitors/{tid}"]')).to_contain_text("Up")


def test_push_monitor_goes_up_when_pushed(app: App, admin_page: Page) -> None:
    page = admin_page
    create_monitor_ui(page, "push", "Nightly backup", interval=300)
    push_url = page.locator("#push-url").inner_text().strip()
    assert re.search(r"/api/push/[A-Za-z0-9]{32}\?", push_url), push_url
    path = push_url[push_url.index("/api/push/"):]

    r = app.client().get(path.replace("msg=OK", "msg=Backup%20done"))
    assert r.ok, (r.status, r.text())
    page.reload()
    expect_status(page, "Up")

    # Pushing "down" is reported as well.
    r = app.client().get(path.split("?")[0] + "?status=down&msg=disk%20full")
    assert r.status in (200, 429), r.status  # 429 if within a second of the previous push
    if r.status == 200:
        page.reload()
        expect_status(page, "Down")
