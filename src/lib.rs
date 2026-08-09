//! Library crate for `immich-federation-at-home`.
//!
//! The binary (`src/main.rs`) is a thin wrapper over this crate. Splitting things this way
//! — rather than putting everything in `main.rs` — is what lets `tests/share_url.rs` (and
//! later integration tests) exercise the real modules through `use
//! immich_federation_at_home::...` instead of re-implementing them.
//!
//! Modules land here incrementally as `PLAN.md` §12's task list works through them; today
//! that's just configuration parsing and share-link parsing.

pub mod config;
pub mod share_url;
