//! Types generated from the vendored OpenAPI spec
//! (`crates/difflore-core/contracts/openapi-spec.json`).
//!
//! This file must contain nothing but the `generate_types!` invocation —
//! hand-written DTOs for endpoints not yet in the spec belong in
//! [`super::dto`]. The spec's provenance (source repo commit + sha256) is
//! pinned in `contracts/SOURCE`.
//!
//! Path resolution: the macro resolves the relative path against
//! `CARGO_MANIFEST_DIR`. The workspace `.cargo/config.toml` also sets
//! `OPENAPI_SPEC_PATH=contracts/openapi-spec.json` because the companion
//! `openapi_contract::api!` macro hard-codes a crate-root
//! `openapi-spec.json` lookup and only the env var can redirect it.

openapi_contract::generate_types!("contracts/openapi-spec.json");
