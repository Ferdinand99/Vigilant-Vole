//! Interface translations. The engine (languages, catalogs, plurals,
//! number and date formats, the `Tr` template helpers, the request
//! middleware) lives in [`crate::kit::i18n`]; this module is Vigilant
//! Vole's face of it: the same names. The translations themselves are
//! `locales/*.toml`, registered in [`crate::product::KIT`].

#[cfg(test)]
mod tests;

pub use crate::kit::i18n::*;
