"""Webhook channel: test message, then real down/up alerts from a monitor."""

from __future__ import annotations

import re

from playwright.sync_api import Page, expect

from conftest import App
from fixture_servers import HttpTarget, WebhookCapture
from helpers import choose_monitor_type, expect_status, monitor_id_from_url


def test_webhook_channel_test_and_down_up_alerts(
    app: App, admin_page: Page, webhook: WebhookCapture, http_target: HttpTarget
) -> None:
    page = admin_page
    hook_url = webhook.url("/vole-alerts")

    page.goto("/settings/notifications")
    page.get_by_role("button", name="Add channel").first.click()
    sheet = page.locator("#add-channel")
    expect(sheet).to_be_visible()
    sheet.locator("label.choice").filter(has=page.locator('input[value="webhook"]')).click()
    expect(sheet.locator("#f-c_url")).to_be_visible()
    sheet.locator("#f-name").fill("Capture hook")
    sheet.locator("#f-c_url").fill(hook_url)

    sheet.get_by_role("button", name="Send test").click()
    test_msg = webhook.wait_for(lambda r: r["path"] == "/vole-alerts" and isinstance(r["json"], dict) and r["json"].get("event") == "test")
    assert test_msg["method"] == "POST"
    assert test_msg["headers"].get("content-type", "").startswith("application/json")
    expect(page.locator(".toast").filter(has_text="Test sent")).to_be_visible()

    sheet.get_by_role("button", name="Add channel").click()
    expect(sheet).to_be_hidden()
    channel_link = page.get_by_role("link", name="Capture hook")
    expect(channel_link).to_be_visible()

    # A monitor that alerts to the channel.
    http_target.up()
    page.goto("/monitors/new")
    choose_monitor_type(page, "http")
    page.locator("#f-name").fill("Alerting burrow")
    page.locator("#f-target_url").fill(http_target.url)
    page.locator("#f-retries").fill("0")
    page.get_by_role("radiogroup", name="Alerts to Capture hook").locator("label").filter(has_text="Alerts").click()
    page.get_by_role("button", name="Create monitor").click()
    expect(page).to_have_url(re.compile(r"/monitors/\d+$"))
    mid = monitor_id_from_url(page.url)
    page.get_by_role("button", name="Check now").click()
    expect_status(page, "Up")

    def for_monitor(event: str):
        return lambda r: isinstance(r["json"], dict) and r["json"].get("event") == event and r["json"].get("monitor_id") == mid

    http_target.down()
    try:
        page.get_by_role("button", name="Check now").click()
        expect_status(page, "Down")
        down = webhook.wait_for(for_monitor("down"))
        assert down["json"]["monitor"] == "Alerting burrow"
        assert down["json"]["status"] in ("down", "DOWN", "Down")
        assert "500" in (down["json"].get("message") or "")
    finally:
        http_target.up()

    page.get_by_role("button", name="Check now").click()
    expect_status(page, "Up")
    up = webhook.wait_for(for_monitor("up"))
    assert up["json"]["downtime_seconds"] is not None and up["json"]["downtime_seconds"] >= 0
    assert up["json"]["monitor"] == "Alerting burrow"
