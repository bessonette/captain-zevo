//! Core logic for Captain Zevo: project registry, Traefik config generation,
//! container runtime access and reconciliation. No UI dependencies.

pub mod certs;
pub mod devcontainer;
pub mod doctor;
pub mod envfile;
pub mod error;
pub mod health;
pub mod ide;
pub mod lifecycle;
pub mod model;
pub mod plugins;
pub mod programs;
pub mod reconcile;
pub mod register;
pub mod remote;
pub mod runtime;
pub mod setup;
pub mod store;
pub mod templates;
pub mod traefik;

pub use error::{Error, Result};
