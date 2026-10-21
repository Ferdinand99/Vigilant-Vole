//! Interface translations. The engine (languages, catalogs, plurals,
//! number and date formats, the `Tr` template helpers) lives in
//! [`crate::kit::i18n`]; this module is Vigilant Vole's face of it: the same
//! names, plus the request middleware that reads the site language from the
//! settings table. The translations themselves are `locales/*.toml`,
//! registered in [`crate::product::KIT`].

mod middleware;
#[cfg(test)]
mod tests;

pub use crate::kit::i18n::*;
pub use middleware::{layer, site_language};
