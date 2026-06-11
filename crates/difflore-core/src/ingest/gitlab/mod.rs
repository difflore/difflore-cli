//! GitLab review-import surface.
//!
//! Step 2 of the multi-VCS foundation: PAT storage + token resolution
//! ([`auth`]). The REST import client lands in a later step; nothing here
//! talks to a GitLab API.

pub mod auth;
