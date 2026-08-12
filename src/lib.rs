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
//! that's configuration parsing, logging, share-link parsing, the shared Immich HTTP
//! plumbing + DTOs, the retry helper, the per-run sync algorithm, the startup sequence, and
//! the scheduler.

pub mod cache;
pub mod config;
pub mod immich;
pub mod log;
pub mod retry;
pub mod scheduler;
pub mod share_url;
pub mod startup;
pub mod sync;

/// Formats a plain `&dyn std::error::Error` and its full [`std::error::Error::source`]
/// chain as one single-line, human-readable string — `"<top message>: <cause 1>: <cause
/// 2>: ..."`.
///
/// Every `thiserror` error type in this crate describes only its *own* layer in its
/// `#[error(...)]` message and leaves the cause to `#[source]`/`source()` — deliberately,
/// so that whatever prints the error is the one place that decides whether to show the
/// full chain or just the top line. Log sites that print one of these errors bare (no
/// `anyhow` wrapping in play) should call this function rather than `{err}` directly, or
/// the cause is silently dropped. [`format_error_chain`] delegates here for the
/// `anyhow::Error` case.
pub fn format_error_chain_dyn(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(cause) = source {
        parts.push(cause.to_string());
        source = cause.source();
    }
    parts.join(": ")
}

/// Formats an [`anyhow::Error`] and its full cause chain as one single-line,
/// human-readable string — `"<top message>: <cause 1>: <cause 2>: ..."`.
///
/// This crate's entire logging format (`PLAN.md` §8) is single-line events; a fatal error
/// should go through the same pipe (get a timestamp and a level tag, land on the same
/// stream as everything else) rather than the Rust runtime's own default
/// `Result`-from-`main` formatting (`Error: ...` followed by a multi-line `Caused by:`
/// list), which would look inconsistent dropped into the middle of this program's own log
/// stream. `main.rs` and `scheduler.rs` both use this for exactly that reason — see their
/// own doc comments for where.
///
/// Delegates to [`format_error_chain_dyn`] via `anyhow::Error`'s `Deref<Target = dyn
/// std::error::Error + Send + Sync>` (trait-upcast to a plain `dyn std::error::Error`) —
/// one chain-walking implementation, not two.
pub fn format_error_chain(err: &anyhow::Error) -> String {
    format_error_chain_dyn(err.as_ref())
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
