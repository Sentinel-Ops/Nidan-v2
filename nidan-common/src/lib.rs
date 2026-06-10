//! # nidan-common
//!
//! Types partagés, système d'erreurs, configuration et utilitaires
//! utilisés par tous les composants NIDAN.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

pub mod config;
pub mod crypto;
pub mod error;
pub mod logging;
pub mod session;
