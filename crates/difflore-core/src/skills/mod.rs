mod candidates;
mod cloud_sync;
mod crud;
pub mod fs;
mod local;
mod remember;
pub(crate) mod semantic_dedup;
mod stats;
pub mod sweep;
mod types;

pub use candidates::*;
pub use cloud_sync::*;
pub use crud::*;
pub use local::*;
pub use remember::*;
pub use stats::*;
pub use sweep::{
    QuarantineReport, SweepOpts, SweepReport, quarantine_unguided_conv_reviews, sweep_stale_skills,
};

#[cfg(test)]
pub(crate) use remember::remember_content_hash;
pub(crate) use types::{
    SkillRepoRow, SkillRow, fetch_skill_row_by_id, fetch_skill_row_by_id_optional,
};
#[cfg(test)]
pub(crate) use types::{decode_base64_lossy, parse_list_value, parse_skill_frontmatter};

#[cfg(test)]
#[path = "tests.rs"]
mod skills_tests;
