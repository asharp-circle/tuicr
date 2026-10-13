//! Client for the chd-owned PR cache (`chd pr-cache ...`).
//!
//! chd is the only writer of cache files; tuicr reads and requests fills through
//! the CLI. A diff is reused only on an exact (head, resolved merge base)
//! match, never on age. Any chd failure (absent, miss, error, timeout) falls
//! back to the live fetch.

mod bundle;
mod runner;

pub use bundle::{
    InfoParts, InfoSelection, note_consumed, open_bundle, peek_info_parts, stashed_commits,
    stashed_files, stashed_merge_base, take_stashed_info_parts, take_stashed_reviews,
    take_stashed_viewer,
};
#[cfg(test)]
pub(crate) use runner::install_fake_chd;
pub(crate) use runner::is_enabled;
use runner::{READ_TIMEOUT, enabled, run_chd, spawn_fill};

use crate::error::Result;
use crate::forge::traits::{ForgeRepository, PullRequestDetails};

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

/// Cached PR collection rows for this head and base tip.
pub fn get_list(pr: &PullRequestDetails, name: &str) -> Option<String> {
    get_list_sourced(pr, name).map(|(rows, _)| rows)
}

/// Where a cached collection came from, so provenance names the real source.
pub const FROM_BUNDLE: &str = "commits.bundle";
pub const FROM_GET_LIST: &str = "commits.get_list";

/// Like [`get_list`], also returning the provenance label of the serving path.
pub fn get_list_sourced(pr: &PullRequestDetails, name: &str) -> Option<(String, &'static str)> {
    if !enabled() || pr.head_sha.is_empty() || pr.base_sha.is_empty() {
        return None;
    }
    if name == "commits"
        && let Some(rows) = stashed_commits(pr)
    {
        return Some((rows, FROM_BUNDLE));
    }
    run_chd(&list_args("get-list", pr, name), READ_TIMEOUT)
        .filter(|rows| !rows.is_empty())
        .map(|rows| (rows, FROM_GET_LIST))
}

/// Request a background cache fill for this PR collection.
pub fn request_list_fill(pr: &PullRequestDetails, name: &str) {
    if enabled() && !pr.head_sha.is_empty() && !pr.base_sha.is_empty() {
        spawn_fill(list_args("fill-list", pr, name));
    }
}

fn list_args(op: &str, pr: &PullRequestDetails, name: &str) -> Vec<String> {
    let mut args = key_args(op, pr);
    // chd order: HOST OWNER REPO N NAME HEAD BASE_TIP
    let (head, base) = (args.remove(6), args.remove(6));
    args.extend([name.to_string(), head, base]);
    args
}

/// Cached patch for exactly this head + merge base, if chd has one.
pub fn get_diff(pr: &PullRequestDetails) -> Option<String> {
    if !enabled() || pr.head_sha.is_empty() || pr.base_sha.is_empty() {
        return None;
    }
    run_chd(&key_args("get-diff", pr), READ_TIMEOUT).filter(|patch| !patch.is_empty())
}

/// Request a background diff cache fill for this exact (head, merge base).
pub fn request_diff_fill(pr: &PullRequestDetails) {
    if enabled() && !pr.head_sha.is_empty() && !pr.base_sha.is_empty() {
        spawn_fill(key_args("fill-diff", pr));
    }
}

/// Immutable file content at a revision SHA, if chd has it.
pub fn get_blob(repo: &ForgeRepository, sha: &str, path: &str) -> Option<String> {
    if !enabled() || sha.is_empty() {
        return None;
    }
    run_chd(&blob_args("get-blob", repo, sha, path), READ_TIMEOUT)
}

/// Ask chd to fetch and store this revision file (raw bytes) in the background.
pub fn request_blob_fill(repo: &ForgeRepository, sha: &str, path: &str) {
    if enabled() && !sha.is_empty() {
        spawn_fill(blob_args("fill-blob", repo, sha, path));
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

/// Use the cached patch only if `pair` accepts it against live metadata.
/// Otherwise, fetch live and ask chd to fill the entry.
pub fn diff_with_cache<T>(
    pr: &PullRequestDetails,
    lookup: impl FnOnce(&PullRequestDetails) -> Option<String>,
    store: impl FnOnce(&PullRequestDetails),
    fetch: impl FnOnce() -> Result<String>,
    pair: impl Fn(&str) -> Result<T>,
) -> Result<T> {
    let cached = crate::profile::time_with(
        "pr.diff_cache_lookup",
        || lookup(pr),
        |hit| if hit.is_some() { "hit" } else { "miss" }.to_string(),
    );
    if let Some(cached) = cached {
        let paired = crate::profile::time_with(
            "pr.diff_cache_pair",
            || pair(&cached),
            |r| if r.is_ok() { "accepted" } else { "rejected" }.to_string(),
        );
        if let Ok(parsed) = paired {
            return Ok(parsed);
        }
    }
    let live = crate::profile::time("pr.diff_live_fetch", fetch)?;
    let parsed = pair(&live)?;
    store(pr);
    Ok(parsed)
}

#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn commit_list_source_distinguishes_bundle_from_standalone_cache() {
        use crate::forge::chd_cache::runner::install_fake_chd;
        let repo = pr().repository;
        install_fake_chd(Some(std::rc::Rc::new(|args| {
            if args.get(1).map(String::as_str) == Some("open") {
                Some(r#"{"head":"a","base":"b"}"#.to_string())
            } else {
                Some("[1]".to_string())
            }
        })));
        // No bundle commits: the standalone get-list serves and is labelled so.
        let mut p = pr();
        p.head_sha = "a".into();
        p.base_sha = "b".into();
        let _ = open_bundle(&repo, p.number, InfoSelection::default());
        assert_eq!(
            get_list_sourced(&p, "commits"),
            Some(("[1]".into(), FROM_GET_LIST))
        );
        install_fake_chd(Some(std::rc::Rc::new(|args| {
            Some(if args.get(1).map(String::as_str) == Some("open") {
                r#"{"head":"a","base":"b","commits":[2]}"#.to_string()
            } else {
                "[1]".to_string()
            })
        })));
        let _ = open_bundle(&repo, p.number, InfoSelection::default());
        assert_eq!(
            get_list_sourced(&p, "commits"),
            Some(("[2]".into(), FROM_BUNDLE))
        );
        install_fake_chd(None);
    }

    #[test]
    fn hit_skips_live_fetch() {
        let fetched = Cell::new(false);
        let out = diff_with_cache(
            &pr(),
            |_| Some("cached".into()),
            |_| panic!("no store on hit"),
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
            |_| {
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
    fn list_args_follow_chd_order() {
        assert_eq!(
            list_args("get-list", &pr(), "commits"),
            [
                "pr-cache",
                "get-list",
                "github.com",
                "o",
                "r",
                "1",
                "commits",
                "a",
                "b"
            ]
        );
    }

    #[test]
    fn miss_fetches_live() {
        let out = diff_with_cache(
            &pr(),
            |_| None,
            |_| {},
            || Ok("live".into()),
            |s| Ok(s.to_string()),
        )
        .unwrap();
        assert_eq!(out, "live");
    }
}
