//! Authentication primitives and the request-level auth contract. The
//! implementation is [`crate::kit::auth`]; this module is Vigilant Vole's
//! face of it, with the names of its cookies and token prefix.

pub use crate::kit::auth::*;
pub use crate::product::SESSION_COOKIE;

pub mod session {
    //! Session cookies and proxy awareness ([`crate::kit::auth::session`]).
    pub use crate::kit::auth::session::*;
    pub use crate::product::PENDING_2FA_COOKIE;
}

pub mod tokens {
    //! API tokens ([`crate::kit::auth::tokens`]).
    pub use crate::kit::auth::tokens::*;
    pub use crate::product::TOKEN_PREFIX;
}
