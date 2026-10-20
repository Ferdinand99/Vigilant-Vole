"""The service monitor types (Redis, RADIUS, …) through the real form."""

from __future__ import annotations

import re

from playwright.sync_api import Page, expect

from fixture_servers import FakeRedis
from helpers import choose_monitor_type, expect_status

SECRET_MASK = "••••••"


def test_redis_monitor_form_check_and_secret_handling(admin_page: Page, fake_redis: FakeRedis) -> None:
    page = admin_page
    page.goto("/monitors/new")
    choose_monitor_type(page, "redis")

    # Only the controls of this type are on screen.
    expect(page.locator("#f-target_redis")).to_be_visible()
    expect(page.locator("#f-basic_auth_user-svc")).to_be_visible()
    expect(page.locator("#f-ignore_tls-svc")).to_be_attached()
    expect(page.locator("#f-http_method")).to_be_hidden()
    expect(page.locator("#f-secret-radius")).to_be_hidden()
    expect(page.locator("#f-keyword-mqtt")).to_be_hidden()

    page.locator("#f-name").fill("Burrow cache")
    page.locator("#f-target_redis").fill(f"redis://127.0.0.1:{fake_redis.port}/1")
    page.locator("#f-retries").fill("0")
    page.locator("#f-basic_auth_pass-svc").fill(FakeRedis.PASSWORD)
    page.get_by_role("button", name=re.compile("^(Create|Add|Save)")).first.click()
    expect(page).to_have_url(re.compile(r"/monitors/\d+$"))
    monitor_url = page.url

    page.get_by_role("button", name="Check now").click()
    expect_status(page, "Up")
    page.reload()
    expect(page.locator("main")).to_contain_text("PONG")

    # The saved password never comes back to the browser; the mask stays on save.
    page.goto(f"{monitor_url}/edit")
    expect(page.locator("#f-basic_auth_pass-svc")).to_have_value(SECRET_MASK)
    assert FakeRedis.PASSWORD not in page.content()
    page.get_by_role("button", name=re.compile("^Save")).first.click()
    expect(page).to_have_url(re.compile(r"/monitors/\d+$"))
    page.get_by_role("button", name="Check now").click()
    expect_status(page, "Up")


def test_wrong_password_is_explained_on_the_detail_page(admin_page: Page, fake_redis: FakeRedis) -> None:
    page = admin_page
    page.goto("/monitors/new")
    choose_monitor_type(page, "redis")
    page.locator("#f-name").fill("Locked cache")
    page.locator("#f-target_redis").fill(f"127.0.0.1:{fake_redis.port}")
    page.locator("#f-retries").fill("0")
    page.locator("#f-basic_auth_pass-svc").fill("not-the-password")
    page.get_by_role("button", name=re.compile("^(Create|Add|Save)")).first.click()
    page.get_by_role("button", name="Check now").click()
    expect_status(page, "Down")
    expect(page.locator(".callout")).to_contain_text("Authentication failed")


def test_switching_type_swaps_the_connection_fields(admin_page: Page) -> None:
    page = admin_page
    page.goto("/monitors/new")
    choose_monitor_type(page, "radius")
    expect(page.locator("#f-secret-radius")).to_be_visible()
    expect(page.locator("#f-basic_auth_user-svc")).to_be_visible()
    expect(page.locator("#f-secret-snmp")).to_be_hidden()

    choose_monitor_type(page, "snmp")
    expect(page.locator("#f-secret-snmp")).to_be_visible()
    expect(page.locator("#f-keyword-snmp")).to_be_visible()
    expect(page.locator("#f-secret-radius")).to_be_hidden()
    expect(page.locator("#f-basic_auth_user-svc")).to_be_hidden()

    choose_monitor_type(page, "tcp")
    expect(page.locator("#sec-service")).to_be_hidden()

    # The HTTP form still has its own, separate controls.
    choose_monitor_type(page, "keyword")
    expect(page.locator("#f-keyword")).to_be_visible()
    expect(page.locator("#f-basic_auth_user-svc")).to_be_hidden()
