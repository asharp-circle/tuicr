//! Client for the chd-owned PR cache (`chd pr-cache ...`).
//!
//! chd is the only writer of cache files; tuicr reads and contributes through
//! the CLI. A diff is reused only on an exact (head, resolved merge base)
//! match, never on age. Any chd failure (absent, miss, error) falls back to
//! the live fetch.

use crate::error::Result;
use crate::forge::traits::{ForgeRepository, PullRequestDetails};
use crate::process::{run_command_output, run_command_output_with_stdin};

const DISABLE_ENV: &str = "TUICR_CHD_CACHE";

fn enabled() -> bool {
    // Unit tests must never reach the developer's real chd cache.
    if cfg!(test) {
        return false;
    }
    std::env::var(DISABLE_ENV).map(|v| v != "0").unwrap_or(true)
}

fn key_args(op: &str, pr: &PullRequestDetails) -> Vec<String> {
    vec![
        "pr-cache".into(),
        op.into(),
        pr.repository.host.clone(),
        pr.repository.owner.clone(),
        pr.repository.name.clone(),
        pr.number.to_string(),
        pr.head_sha.clone(),
        pr.base_sha.clone(),
    ]
}

/// Cached patch for exactly this head + merge base, if chd has one.
pub fn get_diff(pr: &PullRequestDetails) -> Option<String> {
    if !enabled() || pr.head_sha.is_empty() || pr.base_sha.is_empty() {
        return None;
    }
    run_command_output("chd", None, key_args("get-diff", pr))
        .ok()
        .filter(|patch| !patch.is_empty())
}

/// Contribute a freshly fetched patch; failures are ignored (cache is optional).
pub fn put_diff(pr: &PullRequestDetails, patch: &str) {
    if !enabled() || pr.head_sha.is_empty() || pr.base_sha.is_empty() || patch.is_empty() {
        return;
    }
    let _ = run_command_output_with_stdin("chd", None, key_args("put-diff", pr), patch);
}

/// Immutable file content at a revision SHA, if chd has it.
pub fn get_blob(repo: &ForgeRepository, sha: &str, path: &str) -> Option<String> {
    if !enabled() || sha.is_empty() {
        return None;
    }
    run_command_output("chd", None, blob_args("get-blob", repo, sha, path)).ok()
}

/// Contribute revision content (immutable, so no validation is needed).
pub fn put_blob(repo: &ForgeRepository, sha: &str, path: &str, content: &str) {
    if enabled() && !sha.is_empty() {
        let _ = run_command_output_with_stdin(
            "chd",
            None,
            blob_args("put-blob", repo, sha, path),
            content,
        );
    }
}

fn blob_args(op: &str, repo: &ForgeRepository, sha: &str, path: &str) -> Vec<String> {
    vec![
        "pr-cache".into(),
        op.into(),
        repo.host.clone(),
        repo.owner.clone(),
        repo.name.clone(),
        sha.into(),
        path.into(),
    ]
}

/// Server-validated mutable PR metadata (`pr`, `comments`, `reviews`) JSON.
///
/// chd sends the stored ETag as `If-None-Match`; `Ok` means the server
/// confirmed it current. `Err` means freshness could not be verified and the
/// caller must surface that rather than show cached data as current.
pub fn revalidate_meta(pr: &PullRequestDetails, name: &str) -> std::result::Result<String, String> {
    if !enabled() {
        return Err("chd cache disabled".into());
    }
    let args = vec![
        "pr-cache".to_string(),
        "revalidate".into(),
        pr.repository.host.clone(),
        pr.repository.owner.clone(),
        pr.repository.name.clone(),
        pr.number.to_string(),
        name.into(),
    ];
    run_command_output("chd", None, args).map_err(|e| format!("chd revalidation failed: {e:?}"))
}

/// Use the cached patch only if `pair` accepts it against live metadata;
/// otherwise fetch live and contribute the result.
pub fn diff_with_cache<T>(
    pr: &PullRequestDetails,
    lookup: impl FnOnce(&PullRequestDetails) -> Option<String>,
    store: impl FnOnce(&PullRequestDetails, &str),
    fetch: impl FnOnce() -> Result<String>,
    pair: impl Fn(&str) -> Result<T>,
) -> Result<T> {
    if let Some(cached) = lookup(pr)
        && let Ok(parsed) = pair(&cached)
    {
        return Ok(parsed);
    }
    let live = fetch()?;
    let parsed = pair(&live)?;
    store(pr, &live);
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::traits::ForgeRepository;
    use std::cell::Cell;

    fn pr() -> PullRequestDetails {
        PullRequestDetails {
            repository: ForgeRepository::github("github.com", "o", "r"),
            number: 1,
            title: String::new(),
            url: String::new(),
            state: String::new(),
            is_draft: false,
            author: None,
            head_ref_name: String::new(),
            base_ref_name: String::new(),
            head_sha: "a".into(),
            base_sha: "b".into(),
            body: String::new(),
            updated_at: None,
            closed: false,
            merged_at: None,
            diff_start_sha: None,
        }
    }

    #[test]
    fn hit_skips_live_fetch() {
        let fetched = Cell::new(false);
        let out = diff_with_cache(
            &pr(),
            |_| Some("cached".into()),
            |_, _| panic!("no store on hit"),
            || {
                fetched.set(true);
                Ok("live".into())
            },
            |s| Ok(s.to_string()),
        )
        .unwrap();
        assert_eq!(out, "cached");
        assert!(!fetched.get());
    }

    #[test]
    fn unpairable_cache_falls_back_and_contributes_live() {
        let stored = Cell::new(false);
        let out = diff_with_cache(
            &pr(),
            |_| Some("bad".into()),
            |_, p| {
                assert_eq!(p, "live");
                stored.set(true);
            },
            || Ok("live".into()),
            |s| {
                if s == "bad" {
                    Err(crate::error::TuicrError::Io(std::io::Error::other("x")))
                } else {
                    Ok(s.to_string())
                }
            },
        )
        .unwrap();
        assert_eq!(out, "live");
        assert!(stored.get());
    }

    #[test]
    fn miss_fetches_live() {
        let out = diff_with_cache(
            &pr(),
            |_| None,
            |_, _| {},
            || Ok("live".into()),
            |s| Ok(s.to_string()),
        )
        .unwrap();
        assert_eq!(out, "live");
    }
}
