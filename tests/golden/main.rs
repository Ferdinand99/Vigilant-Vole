//! Golden tests: freeze the externally visible behaviour (names, formats,
//! headers, rendered pages, schema, docs, outgoing notifications) so that a
//! refactor which is meant to change nothing visibly changes nothing.
//!
//! Snapshots live in `tests/golden/snapshots/` (insta). A failing golden test
//! means the output changed: if that was intended, review and accept the new
//! snapshot (`cargo insta review`, or `INSTA_UPDATE=always cargo test --test
//! golden` and read the diff). See `docs/golden-tests.md`.
//!
//! Everything volatile (dates, signed cookie values, random tokens, the
//! package version, CRLF line endings of a Windows checkout) is normalised
//! by [`norm`] before it is compared.

#[path = "../common/mod.rs"]
mod common;

mod harness;
mod norm;

mod auth;
mod docs_mcp;
mod i18n;
mod notifications;
mod pages;
mod product;
mod schema;
mod security_headers;
mod static_assets;
