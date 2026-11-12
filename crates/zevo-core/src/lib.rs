//! Core logic for Captain Zevo: project registry, Traefik config generation,
//! container runtime access and reconciliation. No UI dependencies.

pub mod certs;
pub mod devcontainer;
pub mod doctor;
pub mod error;
pub mod model;
pub mod reconcile;
pub mod register;
pub mod runtime;
pub mod setup;
pub mod store;
pub mod traefik;

pub use error::{Error, Result};
