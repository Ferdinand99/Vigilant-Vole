"""Passkeys in a real browser, with Chromium's virtual authenticator (CDP
WebAuthn domain: internal transport, resident keys, user verification).

WebAuthn needs a secure context, so the browser opens the instance at
``http://localhost:<port>`` and that is set as the public URL.
"""

from __future__ import annotations

import re

import pyotp
from playwright.sync_api import Page, expect

from conftest import ADMIN_PASSWORD, ADMIN_USER, App, ConsoleWatch, ContextFactory, run_setup
from helpers import ui_login, ui_logout


def virtual_authenticator(page: Page) -> tuple[object, str]:
    cdp = page.context.new_cdp_session(page)
    cdp.send("WebAuthn.enable")
    added = cdp.send(
        "WebAuthn.addVirtualAuthenticator",
        {
            "options": {
                "protocol": "ctap2",
                "transport": "internal",
                "hasResidentKey": True,
                "hasUserVerification": True,
                "isUserVerified": True,
                "automaticPresenceSimulation": True,
            }
        },
    )
    return cdp, added["authenticatorId"]


def test_passkey_added_under_security_then_logs_in_without_password_or_code(
    fresh_app: App, new_context: ContextFactory, console: ConsoleWatch
) -> None:
    app = fresh_app
    run_setup(app)
    origin = f"http://localhost:{app.port}"
    page = new_context(base_url=origin).new_page()
    cdp, authenticator = virtual_authenticator(page)

    # No public URL yet: the login page offers no passkeys.
    page.goto("/login")
    expect(page.locator("[data-passkey-signin]")).to_have_count(0)
    ui_login(page, ADMIN_USER, ADMIN_PASSWORD)
    expect(page).to_have_url(origin + "/")

    page.goto("/settings/security")
    expect(page.locator("#passkeys")).to_contain_text("Passkeys need this site’s public URL over HTTPS.")

    page.goto("/settings/general")
    page.get_by_label("Public URL").fill(origin)
    page.get_by_role("button", name="Save settings").click()
    expect(page.locator(".toast")).to_contain_text("Settings saved")

    # Without WebAuthn the note says so, instead of naming the address.
    bare = new_context(base_url=origin).new_page()
    bare.add_init_script("delete window.PublicKeyCredential;")
    bare.goto("/login")
    ui_login(bare, ADMIN_USER, ADMIN_PASSWORD)
    expect(bare).to_have_url(origin + "/")
    bare.goto("/settings/security")
    expect(bare.locator("[data-passkey-note]")).to_have_text(
        "This browser doesn’t support passkeys. Try another browser, or log in with your password."
    )
    expect(bare.locator("[data-passkey-add]")).to_be_hidden()

    # At another address it names the public URL, as a link.
    other = new_context(base_url=app.base_url).new_page()
    other.goto("/login")
    ui_login(other, ADMIN_USER, ADMIN_PASSWORD)
    expect(other).to_have_url(app.base_url + "/")
    other.goto("/settings/security")
    note = other.locator("[data-passkey-note]")
    expect(note).to_have_text(f"Passkeys work at {origin} over HTTPS. Open that address to add one.")
    expect(note.get_by_role("link", name=origin)).to_have_attribute("href", origin + "/settings/security#passkeys")
    expect(other.locator("[data-passkey-add]")).to_be_hidden()

    # Two-factor on, so there is a code step for the passkey to skip.
    page.goto("/settings/security")
    page.get_by_role("button", name="Set up two-factor").click()
    secret = re.sub(r"\s+", "", page.locator("#totp-secret").inner_text())
    page.get_by_label("6-digit code").fill(pyotp.TOTP(secret).now())
    page.get_by_role("button", name="Turn on").click()
    expect(page.locator(".toast")).to_contain_text("Two-factor authentication is on")

    # Add a passkey: the password first, then the authenticator. Security is
    # reached through the menus, by boosted (htmx) navigation: the button
    # must be there without a reload.
    page.goto("/")
    page.evaluate("window.vvNoReload = true")
    page.locator(".topnav").get_by_role("link", name="Settings").click()
    expect(page).to_have_url(re.compile(r"/settings(/general)?$"))
    page.locator(".settings-nav").get_by_role("link", name="Security").click()
    expect(page).to_have_url(origin + "/settings/security")
    assert page.evaluate("window.vvNoReload === true"), "the navigation reloaded the page"
    expect(page.get_by_role("button", name="Add passkey")).to_be_visible()
    expect(page.locator("[data-passkey-note]")).to_be_hidden()
    page.get_by_role("button", name="Add passkey").click()
    sheet = page.get_by_role("dialog", name="Add passkey")
    sheet.get_by_label("Current password").fill(ADMIN_PASSWORD)
    sheet.get_by_role("button", name="Continue").click()
    expect(page.locator(".toast")).to_contain_text("Passkey added")
    expect(page.locator("#passkeys .list-title")).to_have_text("Passkey 1")
    expect(page.locator("#passkeys")).to_contain_text("Never used")
    credentials = cdp.send("WebAuthn.getCredentials", {"authenticatorId": authenticator})["credentials"]
    assert len(credentials) == 1 and credentials[0]["isResidentCredential"], credentials

    # Away and back a few times by link: the form still sends one request.
    page.evaluate("window.vvNoReload = true")
    for _ in range(2):
        page.locator(".settings-nav").get_by_role("link", name="General").click()
        expect(page).to_have_url(re.compile(r"/settings(/general)?$"))
        page.locator(".settings-nav").get_by_role("link", name="Security").click()
        expect(page).to_have_url(origin + "/settings/security")
    assert page.evaluate("window.vvNoReload === true"), "the navigation reloaded the page"
    options_requests: list[str] = []
    page.on(
        "request",
        lambda r: options_requests.append(r.method) if r.url.endswith("/settings/security/passkeys/options") else None,
    )
    page.get_by_role("button", name="Add passkey").click()
    sheet = page.get_by_role("dialog", name="Add passkey")
    console.allow(r"status of 403")
    sheet.get_by_label("Current password").fill("not the password")
    with page.expect_response(lambda r: r.url.endswith("/settings/security/passkeys/options")):
        sheet.get_by_role("button", name="Continue").click()
    expect(page.locator(".toast.status-error")).to_be_visible()
    assert options_requests == ["POST"], options_requests
    sheet.get_by_role("button", name="Cancel").click()
    expect(sheet).to_be_hidden()

    # Signed out, the login page asks the browser for passkeys at once
    # (autofill, `mediation: "conditional"`). A person picks theirs from the
    # username field's suggestions; the virtual authenticator answers by
    # itself. Either way: no password, no code.
    ui_logout(page)
    expect(page).to_have_url(origin + "/")
    expect(page.get_by_role("button", name="Account and appearance")).to_be_visible()

    # The button, in a browser without passkey autofill.
    page.add_init_script("PublicKeyCredential.isConditionalMediationAvailable = () => Promise.resolve(false);")
    ui_logout(page)
    expect(page.get_by_label("Username")).to_have_attribute("autocomplete", "username webauthn")
    page.get_by_role("button", name="Log in with a passkey").click()
    expect(page).to_have_url(origin + "/")
    expect(page.get_by_role("button", name="Account and appearance")).to_be_visible()

    page.goto("/settings/security")
    expect(page.locator("#passkeys")).to_contain_text("Last used")

    # Opened at another address (not the public URL): no passkey button.
    elsewhere = new_context(base_url=app.base_url).new_page()
    elsewhere.goto("/login")
    expect(elsewhere.locator("[data-passkey-signin]")).to_be_hidden()
