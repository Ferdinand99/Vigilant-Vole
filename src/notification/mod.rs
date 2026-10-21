//! Notification providers. The providers, their schemas and the delivery
//! code are [`crate::kit::notify`]; this module is Vigilant Vole's face of
//! it: the same names, plus the product's name as a constant.

pub use crate::kit::notify::*;

pub mod message {
    //! The shared voice of every provider ([`crate::kit::notify::message`]).
    pub use crate::kit::notify::message::*;

    /// The product's name.
    pub const PRODUCT: &str = crate::product::NAME;
}
