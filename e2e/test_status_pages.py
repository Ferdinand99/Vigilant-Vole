"""Status page editor (sections, drag reorder, publish), public page, incidents, badges."""

from __future__ import annotations

import re

from playwright.sync_api import APIRequestContext, Page, expect

from conftest import App, ContextFactory
from fixture_servers import HttpTarget
from helpers import check_now, create_monitor_http


def drag_handle_above(page: Page, handle, target) -> None:
    """Pointer drag of `handle` until it sits above `target` (real mouse events)."""
    hb = handle.bounding_box()
    tb = target.bounding_box()
    assert hb and tb
    x, y = hb["x"] + hb["width"] / 2, hb["y"] + hb["height"] / 2
    page.mouse.move(x, y)
    page.mouse.down()
    page.mouse.move(x, y - 8, steps=4)
    page.mouse.move(x, tb["y"] + 2, steps=12)
    page.mouse.move(x, tb["y"] - 4, steps=4)
    page.mouse.up()


def section_order(page: Page) -> list[str]:
    return [t.strip() for t in page.locator("#sp-sections [data-sortable-group] .list-title").all_inner_texts()]


def test_status_page_end_to_end(
    app: App, admin_page: Page, admin_api: APIRequestContext, new_context: ContextFactory, http_target: HttpTarget
) -> None:
    http_target.up()
    site = create_monitor_http(admin_api, monitor_type="http", name="Public site", target_url=http_target.url)
    api = create_monitor_http(admin_api, monitor_type="http", name="Public API", target_url=http_target.url + "?api=1")
    check_now(admin_api, site)
    check_now(admin_api, api)

    page = admin_page
    page.goto("/status-pages")
    page.get_by_role("link", name="New status page").first.click()
    page.get_by_label("Title").fill("Vole status")
    page.get_by_label("Address").fill("vole")
    page.get_by_role("button", name="Create status page").click()
    expect(page).to_have_url(re.compile(r"/status-pages/\d+/edit$"))
    page_id = int(re.search(r"/status-pages/(\d+)/edit", page.url).group(1))
    expect(page.get_by_role("heading", name="Vole status", level=1)).to_be_visible()

    # Add a section (a default one may already exist; use our own).
    page.get_by_label("New section name").fill("Burrows")
    page.get_by_role("button", name="Add section").click()
    section = page.locator("#sp-sections section[data-sortable-item]").filter(
        has=page.locator('input[value="Burrows"]')
    )
    expect(section).to_have_count(1)

    section.get_by_role("button", name="Add monitors to Burrows").click()
    picker = page.locator("#sp-picker")
    expect(picker).to_be_visible()
    picker.locator("label.list-row").filter(has_text="Public site").locator("input").check()
    picker.locator("label.list-row").filter(has_text="Public API").locator("input").check()
    picker.get_by_role("button", name="Add selected").click()
    expect(picker).to_be_hidden()
    items = section.locator("li[data-sortable-item] .list-title")
    expect(items).to_have_count(2)
    first, second = [t.strip() for t in items.all_inner_texts()]
    assert {first, second} == {"Public site", "Public API"}

    # Drag the second monitor above the first with the mouse; it saves on drop.
    with page.expect_response(lambda r: r.url.endswith(f"/status-pages/{page_id}/order") and r.request.method == "POST") as resp:
        drag_handle_above(
            page,
            section.get_by_role("button", name=f"Reorder {second}. Use Alt and the arrow keys."),
            section.locator("li[data-sortable-item]").filter(has_text=first),
        )
    assert resp.value.ok, resp.value.status
    expect(items).to_have_text([second, first])
    page.reload()
    expect(page.locator("#sp-sections section[data-sortable-item]").filter(
        has=page.locator('input[value="Burrows"]')).locator("li[data-sortable-item] .list-title")
    ).to_have_text([second, first])
    order_on_page = [second, first]

    # Publish.
    page.get_by_role("link", name="Settings", exact=True).last.click()
    expect(page).to_have_url(re.compile(r"tab=settings"))
    page.get_by_role("switch", name=re.compile("^Published")).check()
    page.get_by_role("button", name="Save changes").click()
    expect(page.locator(".toast")).to_contain_text("Status page saved")

    # Incident.
    page.get_by_role("link", name="Incidents").click()
    page.get_by_role("button", name="New incident").click()
    sheet = page.locator("#new-incident")
    sheet.get_by_label("Title").fill("Tunnel flooding")
    sheet.locator("label").filter(has_text="Degraded").click()
    sheet.get_by_label("Update").fill("We’re **pumping** water out.")
    sheet.get_by_role("switch", name=re.compile("Pin to the top")).check()
    sheet.get_by_role("button", name="Post incident").click()
    expect(page.locator("article.incident").filter(has_text="Tunnel flooding")).to_be_visible()

    # Public page, logged out.
    visitor = new_context().new_page()
    visitor.goto("/status/vole")
    expect(visitor.locator(".banner")).to_be_visible()
    expect(visitor.locator(".banner-title")).to_have_text(re.compile("All systems operational|Degraded|outage"))
    expect(visitor.get_by_text("Public site")).to_be_visible()
    expect(visitor.get_by_text("Public API")).to_be_visible()
    expect(visitor.get_by_role("heading", name="Burrows")).to_be_visible()
    public_names = visitor.locator("section").filter(has=visitor.get_by_role("heading", name="Burrows")).locator(".list-title")
    expect(public_names).to_have_text(order_on_page)
    incident = visitor.locator("article.incident").filter(has_text="Tunnel flooding")
    expect(incident).to_be_visible()
    expect(incident.locator("strong")).to_have_text("pumping")
    html = visitor.content()
    for secret in (http_target.url, f"127.0.0.1:{http_target.port}", "/health"):
        assert secret not in html, f"public page leaks monitor target {secret!r}"

    anon = app.client()
    js = anon.get("/api/status-page/vole")
    assert js.ok and js.headers.get("access-control-allow-origin") == "*"
    assert str(http_target.port) not in js.text(), "status JSON leaks the target"

    badge = anon.get(f"/api/badge/{site}/status")
    assert badge.status == 200, badge.status
    assert badge.headers["content-type"].startswith("image/svg+xml")
    assert "<svg" in badge.text() and "up" in badge.text().lower()
    uptime = anon.get(f"/api/badge/{api}/uptime?duration=7d&style=flat-square")
    assert uptime.status == 200 and "<svg" in uptime.text()
