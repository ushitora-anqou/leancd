//! Shared helpers for the Lean CD end-to-end test suite.
//!
//! These helpers spin up an ephemeral `kind` cluster with in-cluster Forgejo
//! and Lean CD, and provide thin wrappers over `kubectl`/`git`/`curl`. They are
//! added incrementally alongside the scenarios in `tests/e2e.rs`.

pub mod env;
pub mod fgdelete;
pub mod fixture;
pub mod forgejo;
pub mod git;
pub mod helm;
pub mod kubectl;
pub mod leancd;
pub mod manifests;
pub mod metrics;
pub mod portforward;
pub mod ssh;
pub mod wait;

pub use fixture::{Fixture, run};
pub use forgejo::Forgejo;
