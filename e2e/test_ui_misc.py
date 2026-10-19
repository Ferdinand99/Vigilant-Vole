"""Config round trip, Norwegian UI, phone layout, theme and reduced motion."""

from __future__ import annotations

import re

import pytest
from playwright.sync_api import APIRequestContext, Page, expect

from conftest import App, ContextFactory
from fixture_servers import HttpTarget
from helpers import check_now, create_monitor_http, create_user_http, ui_login


def test_config_export_then_import_preview_is_unchanged(
    admin_page: Page, admin_api: APIRequestContext, http_target: HttpTarget
) -> None:
    create_monitor_http(admin_api, monitor_type="http", name="Exported burrow", target_url=http_target.url)

    page = admin_page
    page.goto("/settings/backup")
    page.locator("label").filter(has=page.locator('input[name="format"][value="yaml"]')).click()
    with page.expect_download() as dl:
        page.get_by_role("button", name="Export configuration").click()
    download = dl.value
    assert download.suggested_filename.endswith((".yaml", ".yml")), download.suggested_filename
    document = open(download.path(), encoding="utf-8").read()
    assert "version:" in document and "Exported burrow" in document
    assert http_target.url in document

    page.get_by_label("Or paste it here").fill(document)
    page.get_by_role("button", name="Preview changes").click()
    plan = page.locator("#import-plan")
    expect(plan.get_by_text("Up to date")).to_be_visible()

    def stat_value(label: str):
        tile = plan.locator(".stat").filter(has=page.locator(".stat-label", has_text=re.compile(rf"^\s*{label}\s*$")))
        return tile.locator(".stat-value")

    for label in ("New", "Changed", "Deleted"):
        expect(stat_value(label)).to_have_text("0")
    expect(stat_value("Unchanged")).not_to_have_text("0")


def test_norwegian_user_language_and_public_page(
    app: App, admin_api: APIRequestContext, new_context: ContextFactory, http_target: HttpTarget
) -> None:
    password = "Fjord-Lemen-Hiking-2024"
    create_user_http(admin_api, "nora", password, "editor")
    page = new_context().new_page()
    page.goto("/login")
    ui_login(page, "nora", password)
    expect(page.locator(".topnav").get_by_role("link", name="Dashboard")).to_be_visible()

    page.goto("/settings/security")
    page.get_by_label("Language").select_option("nb")
    page.get_by_role("button", name="Save profile").click()
    expect(page.locator("html")).to_have_attribute("lang", "nb")
    page.goto("/")
    expect(page.locator(".topnav").get_by_role("link", name="Oversikt")).to_be_visible()
    expect(page.get_by_role("heading", level=1)).not_to_have_text("Monitors")

    # Other users keep their own language.
    admin_html = admin_api.get("/").text()
    assert ">Dashboard<" in admin_html and ">Oversikt<" not in admin_html

    mid = create_monitor_http(admin_api, monitor_type="http", name="Fjord gateway", target_url=http_target.url)
    check_now(admin_api, mid)
    r = admin_api.post(
        "/api/v1/status-pages",
        data={"title": "Fjord status", "slug": "fjord", "published": True, "sections": [{"name": "Tjenester", "monitor_ids": [mid]}]},
    )
    assert r.status == 201, (r.status, r.text())

    nb = new_context(extra_http_headers={"Accept-Language": "nb-NO,nb;q=0.9,en;q=0.5"}).new_page()
    nb.goto("/status/fjord")
    expect(nb.locator("html")).to_have_attribute("lang", "nb")
    expect(nb.locator(".banner-title")).to_have_text("Alle systemer fungerer normalt")

    en = new_context(extra_http_headers={"Accept-Language": "en-GB,en;q=0.9"}).new_page()
    en.goto("/status/fjord")
    expect(en.locator(".banner-title")).to_have_text("All systems operational")


def assert_no_horizontal_scroll(page: Page) -> None:
    overflow = page.evaluate("document.documentElement.scrollWidth - window.innerWidth")
    assert overflow <= 0, f"{page.url} scrolls sideways by {overflow}px"


@pytest.mark.smoke
def test_phone_layout(app: App, new_context: ContextFactory, admin_state, admin_api: APIRequestContext, http_target: HttpTarget) -> None:
    create_monitor_http(admin_api, monitor_type="http", name="A monitor with a rather long name to test wrapping on phones", target_url=http_target.url)
    ctx = new_context(storage_state=admin_state, viewport={"width": 375, "height": 812}, is_mobile=False, has_touch=True)
    page = ctx.new_page()

    page.goto("/")
    expect(page.get_by_role("heading", name="Monitors", level=1)).to_be_visible()
    tabbar = page.locator("nav.tabbar")
    expect(tabbar).to_be_visible()
    expect(page.locator(".topnav")).to_be_hidden()
    assert_no_horizontal_scroll(page)

    tabbar.get_by_role("link", name="Status").click()
    expect(page).to_have_url(app.url("/status-pages"))
    assert_no_horizontal_scroll(page)

    page.goto("/monitors/new")
    expect(page.get_by_role("button", name="Create monitor")).to_be_visible()
    page.locator("#f-name").fill("Phone made")
    assert_no_horizontal_scroll(page)

    page.goto("/settings/tokens")
    page.get_by_role("button", name="New token").click()
    sheet = page.get_by_role("dialog", name="New token")
    expect(sheet).to_be_visible()
    box = sheet.bounding_box()
    assert box and box["width"] <= 375 + 1, box
    assert_no_horizontal_scroll(page)
    sheet.get_by_role("button", name="Cancel").click()
    expect(sheet).to_be_hidden()

    page.goto("/settings")
    assert_no_horizontal_scroll(page)


@pytest.mark.smoke
def test_theme_choice_persists(new_context: ContextFactory, admin_state) -> None:
    page = new_context(storage_state=admin_state, color_scheme="light").new_page()
    page.goto("/")
    page.get_by_role("button", name="Account and appearance").click()
    page.locator("label").filter(has=page.locator('input[data-theme-set="dark"]')).click()
    expect(page.locator("html")).to_have_attribute("data-theme", "dark")
    page.reload()
    expect(page.locator("html")).to_have_attribute("data-theme", "dark")
    bg = page.evaluate("getComputedStyle(document.body).backgroundColor")
    page.get_by_role("button", name="Account and appearance").click()
    page.locator("label").filter(has=page.locator('input[data-theme-set="light"]')).click()
    expect(page.locator("html")).to_have_attribute("data-theme", "light")
    assert page.evaluate("getComputedStyle(document.body).backgroundColor") != bg
    page.goto("/settings/security")
    expect(page.locator("html")).to_have_attribute("data-theme", "light")


def test_reduced_motion_renders_cleanly(new_context: ContextFactory, admin_state, admin_api: APIRequestContext, http_target: HttpTarget) -> None:
    mid = create_monitor_http(admin_api, monitor_type="http", name="Calm burrow", target_url=http_target.url)
    check_now(admin_api, mid)
    page = new_context(storage_state=admin_state, reduced_motion="reduce").new_page()
    assert page.evaluate("matchMedia('(prefers-reduced-motion: reduce)').matches")
    for path in ("/", f"/monitors/{mid}", "/status-pages", "/maintenance", "/settings/notifications"):
        page.goto(path)
        expect(page.locator("main")).to_be_visible()
    page.goto("/settings/tokens")
    page.get_by_role("button", name="New token").click()
    sheet = page.get_by_role("dialog", name="New token")
    expect(sheet).to_be_visible()
    sheet.get_by_role("button", name="Close").click()
    expect(sheet).to_be_hidden()
