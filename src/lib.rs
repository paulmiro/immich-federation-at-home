//! Library crate for `immich-federation-at-home`.
//!
//! The binary (`src/main.rs`) is a thin wrapper over this crate. Splitting things this way
//! — rather than putting everything in `main.rs` — is what lets `tests/share_url.rs` (and
//! later integration tests) exercise the real modules through `use
//! immich_federation_at_home::...` instead of re-implementing them. It's also what makes
//! `startup.rs`'s ten-step startup sequence and `scheduler.rs`'s sync loop unit testable at
//! all — `main.rs` itself, by design, is never exercised by a test.
//!
//! Modules land here incrementally as `PLAN.md` §12's task list works through them; so far
//! that's configuration parsing, share-link parsing, the shared Immich HTTP plumbing +
//! DTOs, the retry helper, the per-run sync algorithm, the startup sequence, and the
//! scheduler.

pub mod config;
pub mod immich;
pub mod retry;
pub mod scheduler;
pub mod share_url;
pub mod startup;
pub mod sync;

/// Formats an [`anyhow::Error`] and its full cause chain as one single-line,
/// human-readable string — `"<top message>: <cause 1>: <cause 2>: ..."`.
///
/// This crate's entire logging format (`PLAN.md` §8) is single-line `tracing` events; a
/// fatal error should go through the same pipe (get a timestamp, respect `RUST_LOG`
/// filtering, land wherever the configured writer sends everything else) rather than the
/// Rust runtime's own default `Result`-from-`main` formatting (`Error: ...` followed by a
/// multi-line `Caused by:` list), which would look inconsistent dropped into the middle of
/// this program's own log stream. `main.rs` and `scheduler.rs` both use this for exactly
/// that reason — see their own doc comments for where.
pub fn format_error_chain(err: &anyhow::Error) -> String {
    err.chain()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_error_chain_joins_the_full_cause_chain_outermost_first() {
        let err = anyhow::anyhow!("root cause")
            .context("middle")
            .context("outer");
        assert_eq!(format_error_chain(&err), "outer: middle: root cause");
    }

    #[test]
    fn format_error_chain_leaf_error_is_just_its_own_message() {
        let err = anyhow::anyhow!("only message");
        assert_eq!(format_error_chain(&err), "only message");
    }
}
