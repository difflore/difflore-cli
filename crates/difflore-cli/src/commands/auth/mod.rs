//! `difflore auth` — store GitLab credentials used for review import.
//!
//! Cloud login stays under `difflore cloud login`; this namespace is for
//! per-provider source tokens (today: GitLab PATs).

pub(crate) mod gitlab;
