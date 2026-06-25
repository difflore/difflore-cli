//! Cloud API contract layer (formerly `cloud::api_types`).
//!
//! Two tracks, kept deliberately separate:
//!
//! - [`generated`] — types produced by `openapi_contract::generate_types!`
//!   from the vendored spec at `contracts/openapi-spec.json`.
//! - [`dto`] — hand-written DTOs for endpoints that are not (yet) in the
//!   spec. The registry in that file's header may only shrink.
//!
//! Both tracks are re-exported flat so consumers write
//! `crate::contract::TypeName` without caring which track a type lives on.

pub mod dto;
pub mod generated;

pub use dto::*;
pub use generated::*;
