# Changelog

## [0.13.1](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.13.0...v0.13.1) (2026-10-27)


### Bug Fixes

* **kit:** security hardening from the October review ([#62](https://github.com/Ferdinand99/Vigilant-Vole/issues/62)) ([5ef03d4](https://github.com/Ferdinand99/Vigilant-Vole/commit/5ef03d4e29ffdf2403b28b02ddeab335e16d8c6d))

## [0.13.0](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.12.1...v0.13.0) (2026-10-25)


### Features

* agent-checked monitors ("Check from") ([#59](https://github.com/Ferdinand99/Vigilant-Vole/issues/59)) ([43c5808](https://github.com/Ferdinand99/Vigilant-Vole/commit/43c5808c716bb2a4370548c0e2475f2521a066e3))

## [0.12.1](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.12.0...v0.12.1) (2026-10-24)


### Bug Fixes

* **deps:** RustSec fixes (rustls-webpki, webpki-root-certs, tokio, ctutils), Dependabot and a weekly audit ([5a13860](https://github.com/Ferdinand99/Vigilant-Vole/commit/5a13860bc1669339907aa7f3ad5c66ac308365c7))

## [0.12.0](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.11.1...v0.12.0) (2026-10-24)


### Features

* **kit:** seal notification channel secrets at rest ([#47](https://github.com/Ferdinand99/Vigilant-Vole/issues/47)) ([5978952](https://github.com/Ferdinand99/Vigilant-Vole/commit/59789523580ba9d9cf4d3ff60b7f0dbac9f3dfab))

## [0.11.1](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.11.0...v0.11.1) (2026-10-24)


### Bug Fixes

* **kit:** passkey button after in-app navigation, clearer passkey notes ([#44](https://github.com/Ferdinand99/Vigilant-Vole/issues/44)) ([21e73a3](https://github.com/Ferdinand99/Vigilant-Vole/commit/21e73a39d909f19f2ed8191c6c071024ffef7859))

## [0.11.0](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.10.0...v0.11.0) (2026-10-23)


### Features

* **kit:** sign in with passkeys (WebAuthn) ([#41](https://github.com/Ferdinand99/Vigilant-Vole/issues/41)) ([a553098](https://github.com/Ferdinand99/Vigilant-Vole/commit/a553098874a25fc232a0f4141f8c74b3fe2d9f60))

## [0.10.0](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.9.0...v0.10.0) (2026-10-23)


### Features

* **kit:** show the product and version in a footer on signed-in pages ([#38](https://github.com/Ferdinand99/Vigilant-Vole/issues/38)) ([0fa12c8](https://github.com/Ferdinand99/Vigilant-Vole/commit/0fa12c866be2d57b4fe55e1066e42be1b72df0a3))

## [0.9.0](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.8.0...v0.9.0) (2026-10-23)


### Features

* keep real monitor results during maintenance windows ([#35](https://github.com/Ferdinand99/Vigilant-Vole/issues/35)) ([2bdef79](https://github.com/Ferdinand99/Vigilant-Vole/commit/2bdef791fe82fc6a22a17cc4b046d46ca6728258))

## [0.8.0](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.7.1...v0.8.0) (2026-10-23)


### Features

* **kit:** building blocks for second products ([#33](https://github.com/Ferdinand99/Vigilant-Vole/issues/33)) ([9abe758](https://github.com/Ferdinand99/Vigilant-Vole/commit/9abe758779b295d312c7454c0e7d9ab4b27a1cd3))
* **kit:** product token scopes, real token roles, per-instance sign-in limits and a General settings page ([#30](https://github.com/Ferdinand99/Vigilant-Vole/issues/30)) ([7688b59](https://github.com/Ferdinand99/Vigilant-Vole/commit/7688b597dee70460de023f11398afcc2254f2465))

## [0.7.1](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.7.0...v0.7.1) (2026-10-22)


### Bug Fixes

* **kit:** close pooled SQLite connections one at a time on shutdown ([#25](https://github.com/Ferdinand99/Vigilant-Vole/issues/25)) ([5ac40f9](https://github.com/Ferdinand99/Vigilant-Vole/commit/5ac40f91cb6092f9a806f8ce106e4768774e6726))
* **kit:** let products supply their own texts and API reference details ([#26](https://github.com/Ferdinand99/Vigilant-Vole/issues/26)) ([3e92a17](https://github.com/Ferdinand99/Vigilant-Vole/commit/3e92a1779648d304877886f88a5286b536054aac))

## [0.7.0](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.6.0...v0.7.0) (2026-10-22)


### Features

* **kit:** make vole-kit ready for a second product (phase 3b) ([#22](https://github.com/Ferdinand99/Vigilant-Vole/issues/22)) ([b3bd3a9](https://github.com/Ferdinand99/Vigilant-Vole/commit/b3bd3a9d81951c159bd06e8b60bd5d7bf4e756ff))

## [0.6.0](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.5.1...v0.6.0) (2026-10-21)


### Features

* replace the logo with the new violet vole ([#18](https://github.com/Ferdinand99/Vigilant-Vole/issues/18)) ([24a2d57](https://github.com/Ferdinand99/Vigilant-Vole/commit/24a2d57ff9b0936543852b7e1d3f0699c6a13709))

## [0.5.1](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.5.0...v0.5.1) (2026-10-21)


### Bug Fixes

* complete database backup downloads when gzip-compressed ([41302ba](https://github.com/Ferdinand99/Vigilant-Vole/commit/41302ba7a235c4d55cab6078dec0f6e057e6df14))
* unique names for pre-migration backups taken in the same second ([41302ba](https://github.com/Ferdinand99/Vigilant-Vole/commit/41302ba7a235c4d55cab6078dec0f6e057e6df14))


### Performance Improvements

* survive 1000 monitors on 1 vCPU (shared dashboard renders, separate check pool, cached uptime and status pages, phase-spread checks) ([41302ba](https://github.com/Ferdinand99/Vigilant-Vole/commit/41302ba7a235c4d55cab6078dec0f6e057e6df14))

## [0.5.0](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.4.0...v0.5.0) (2026-10-21)


### Features

* Kuma parity round 2 — maintenance schedules, incident timeline and feed, subscribers, MCP server, 16 notification providers ([#12](https://github.com/Ferdinand99/Vigilant-Vole/issues/12)) ([f7716e5](https://github.com/Ferdinand99/Vigilant-Vole/commit/f7716e51227747fc27897ff2d46aa362a8252ffc))

## [0.4.0](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.3.0...v0.4.0) (2026-10-20)


### Features

* bulk edit for monitors and "Copy for AI" on the API reference ([#11](https://github.com/Ferdinand99/Vigilant-Vole/issues/11)) ([48bace5](https://github.com/Ferdinand99/Vigilant-Vole/commit/48bace5e5bcca3b02570a46b6ce05919d6dcf374))
* **deploy:** static Linux binaries, systemd installer and Proxmox LXC script ([804018b](https://github.com/Ferdinand99/Vigilant-Vole/commit/804018b0096d93f5ceeba619bc69ef2d876cf93b))

## [0.3.0](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.2.0...v0.3.0) (2026-10-20)


### Features

* **i18n:** 24 new interface languages, incl. right-to-left and CJK ([#8](https://github.com/Ferdinand99/Vigilant-Vole/issues/8)) ([ad67acd](https://github.com/Ferdinand99/Vigilant-Vole/commit/ad67acd78824748e5b2d098f06a7f392b098c5f5))
* SQL Server, MongoDB, real-browser, gRPC keyword, game server and Tailscale monitors; SNMPv3; status page logo, password and analytics ([#6](https://github.com/Ferdinand99/Vigilant-Vole/issues/6)) ([d464f5d](https://github.com/Ferdinand99/Vigilant-Vole/commit/d464f5d11ae671442e6088e40d0114dad10369fb))

## [0.2.0](https://github.com/Ferdinand99/Vigilant-Vole/compare/v0.1.0...v0.2.0) (2026-10-20)


### Features

* **monitors:** service monitor types and HTTP OAuth2/mTLS authentication ([#4](https://github.com/Ferdinand99/Vigilant-Vole/issues/4)) ([423ee3d](https://github.com/Ferdinand99/Vigilant-Vole/commit/423ee3ddf2c0fd2b029399a7097c15438c373d5a))

## 0.1.0 (2026-10-19)


### Features

* full Uptime Kuma parity and beyond ([14645c1](https://github.com/Ferdinand99/Vigilant-Vole/commit/14645c10721efad5ff0ab3ac8704b7efca7b59ab))
