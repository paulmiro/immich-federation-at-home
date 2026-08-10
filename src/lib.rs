//! Library crate for `immich-federation-at-home`.
//!
//! The binary (`src/main.rs`) is a thin wrapper over this crate. Splitting things this way
//! — rather than putting everything in `main.rs` — is what lets `tests/share_url.rs` (and
//! later integration tests) exercise the real modules through `use
//! immich_federation_at_home::...` instead of re-implementing them.
//!
//! Modules land here incrementally as `PLAN.md` §12's task list works through them; so far
//! that's configuration parsing, share-link parsing, the shared Immich HTTP plumbing +
//! DTOs, and the retry helper.

pub mod config;
pub mod immich;
pub mod retry;
pub mod share_url;
