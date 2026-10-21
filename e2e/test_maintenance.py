"""Manual maintenance window puts a monitor into maintenance and back."""

from __future__ import annotations

import re

from playwright.sync_api import APIRequestContext, Page, expect

from fixture_servers import HttpTarget
from helpers import check_now, create_monitor_http, expect_status


def test_manual_maintenance_window(admin_page: Page, admin_api: APIRequestContext, http_target: HttpTarget) -> None:
    http_target.up()
    mid = create_monitor_http(admin_api, monitor_type="http", name="Database burrow", target_url=http_target.url)
    check_now(admin_api, mid)

    page = admin_page
    page.goto("/maintenance")
    page.get_by_role("link", name="Schedule maintenance").first.click()
    page.get_by_label("Title").fill("Burrow upgrade")
    page.locator('select[name="strategy"]').select_option("manual")
    page.get_by_role("switch", name=re.compile("^In effect now")).check()
    page.locator("#mw-monitors label.list-row").filter(has_text="Database burrow").locator("input").check()
    page.get_by_role("button", name="Save window").click()
    expect(page.locator(".toast")).to_contain_text("Maintenance window saved")

    expect(page).to_have_url(re.compile(r"/maintenance$"))  # opens on the "Active" tab
    row = page.locator("li.list-row").filter(has_text="Burrow upgrade")
    expect(row).to_contain_text("In progress")
    expect(row).to_contain_text("1 monitor")

    check_now(admin_api, mid)
    page.goto(f"/monitors/{mid}")
    expect_status(page, "Maintenance")

    page.goto("/maintenance?tab=active")
    row.get_by_role("button", name="More actions for Burrow upgrade").click()
    page.get_by_role("button", name="End now").click()
    expect(page.locator(".toast")).to_contain_text("Maintenance ended")

    check_now(admin_api, mid)
    page.goto(f"/monitors/{mid}")
    expect_status(page, "Up")
