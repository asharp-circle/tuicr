//! Validated PR data from a single `chd pr-cache open` call.

#[cfg(not(test))]
use std::sync::{Mutex, OnceLock};

use super::runner::{OPEN_TIMEOUT, enabled, run_chd};
use crate::forge::traits::{ForgeRepository, PullRequestDetails};

/// PR bundle data validated by `chd pr-cache open`.
#[derive(Debug, Clone, Default)]
struct OpenBundle {
    /// Open sequence number. Newer opens take precedence.
    ticket: u64,
    repo: String,
    number: u64,
    head: String,
    base: String,
    files: Option<String>,
    reviews: Option<String>,
    merge_base: Option<String>,
    commits: Option<String>,
}

/// Stashed bundle and sequence number for the most recent open.
#[derive(Default)]
struct StashState {
    latest: u64,
    bundle: Option<OpenBundle>,
}

#[cfg(not(test))]
fn with_stash<T>(f: impl FnOnce(&mut StashState) -> T) -> T {
    static STASH: OnceLock<Mutex<StashState>> = OnceLock::new();
    let mut guard = STASH
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

// Tests run in parallel, so each gets its own stash.
#[cfg(test)]
fn with_stash<T>(f: impl FnOnce(&mut StashState) -> T) -> T {
    thread_local! {
        static STASH: std::cell::RefCell<StashState> = std::cell::RefCell::new(StashState::default());
    }
    STASH.with(|s| f(&mut s.borrow_mut()))
}

#[cfg(test)]
pub(super) fn clear_for_test() {
    with_stash(|st| st.bundle = None);
}

fn repo_key(repo: &ForgeRepository) -> String {
    format!("{}/{}/{}", repo.host, repo.owner, repo.name)
}

/// Validates PR identity and caches preloaded bundle data via `chd pr-cache open`.
/// Returns `(head, base)` if valid. Unvalidated fields fall back to live fetches.
pub fn open_bundle(repo: &ForgeRepository, number: u64) -> Option<(String, String)> {
    // Invalidate prior state so late-finishing opens cannot overwrite newer data.
    let ticket = with_stash(|st| {
        st.latest += 1;
        st.bundle = None;
        st.latest
    });
    if !enabled() {
        return None;
    }
    let args: Vec<String> = [
        "pr-cache",
        "open",
        &repo.host,
        &repo.owner,
        &repo.name,
        &number.to_string(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let out = crate::profile::time("pr.chd_open_bundle", || run_chd(&args, OPEN_TIMEOUT))?;
    let mut bundle = parse_open_bundle(&out, repo, number)?;
    bundle.ticket = ticket;
    let identity = (bundle.head.clone(), bundle.base.clone());
    store_if_newer(bundle);
    Some(identity)
}

/// Stores the bundle only if it matches the latest open ticket.
fn store_if_newer(bundle: OpenBundle) {
    with_stash(|st| {
        if bundle.ticket == st.latest {
            st.bundle = Some(bundle);
        }
    });
}

fn parse_open_bundle(json: &str, repo: &ForgeRepository, number: u64) -> Option<OpenBundle> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let sha = |key: &str| {
        v.get(key)?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let rows = |key: &str| v.get(key).filter(|r| r.is_array()).map(|r| r.to_string());
    Some(OpenBundle {
        ticket: 0,
        repo: repo_key(repo),
        number,
        head: sha("head")?,
        base: sha("base")?,
        files: rows("files"),
        reviews: rows("reviews"),
        merge_base: sha("merge_base").filter(|m| m.len() == 40),
        commits: rows("commits"),
    })
}

/// Reads the stashed bundle if repository, number, head, and base match.
fn stashed<T>(
    repo: &ForgeRepository,
    number: u64,
    head: &str,
    base: &str,
    read: impl FnOnce(&OpenBundle) -> Option<T>,
) -> Option<T> {
    with_stash(|st| {
        let bundle = st.bundle.as_ref()?;
        (bundle.repo == repo_key(repo)
            && bundle.number == number
            && bundle.head == head
            && bundle.base == base)
            .then(|| read(bundle))
            .flatten()
    })
}

/// Validated PR `files` JSON array for this head and base tip.
pub fn stashed_files(pr: &PullRequestDetails) -> Option<String> {
    stashed(&pr.repository, pr.number, &pr.head_sha, &pr.base_sha, |b| {
        b.files.clone()
    })
}

/// Consumes the validated PR `reviews` JSON array for this head and base tip.
pub fn take_stashed_reviews(pr: &PullRequestDetails) -> Option<String> {
    // Match and consume atomically to prevent concurrent replacement.
    with_stash(|st| {
        let bundle = st.bundle.as_mut()?;
        (bundle.repo == repo_key(&pr.repository)
            && bundle.number == pr.number
            && bundle.head == pr.head_sha
            && bundle.base == pr.base_sha)
            .then(|| bundle.reviews.take())
            .flatten()
    })
}

/// Cached commit list (JSON array) for exactly this head and base tip.
pub fn stashed_commits(pr: &PullRequestDetails) -> Option<String> {
    stashed(&pr.repository, pr.number, &pr.head_sha, &pr.base_sha, |b| {
        b.commits.clone()
    })
}

/// Merge base for exactly this head and base tip, cached by those OIDs.
pub fn stashed_merge_base(pr: &PullRequestDetails) -> Option<String> {
    stashed(&pr.repository, pr.number, &pr.head_sha, &pr.base_sha, |b| {
        b.merge_base.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::chd_cache::{pr, runner::install_fake_chd};

    #[test]
    fn parses_open_bundle_and_ignores_unvalidated_parts() {
        let repo = ForgeRepository::github("github.com", "o", "r");
        let j = format!(
            r#"{{"head":"h1","base":"b1","files":[{{"a":1}}],"reviews":null,"merge_base":"{}","commits":null}}"#,
            "c".repeat(40)
        );
        let b = parse_open_bundle(&j, &repo, 1).unwrap();
        assert_eq!((b.head.as_str(), b.base.as_str()), ("h1", "b1"));
        assert!(b.files.is_some() && b.reviews.is_none() && b.commits.is_none());
        assert_eq!(b.merge_base.as_deref(), Some("c".repeat(40).as_str()));
        assert!(parse_open_bundle(r#"{"head":"","base":"b"}"#, &repo, 1).is_none());
        assert!(parse_open_bundle("nope", &repo, 1).is_none());
    }

    #[test]
    fn an_older_open_finishing_late_does_not_replace_a_newer_bundle() {
        let repo = ForgeRepository::github("github.com", "o", "r");
        let mk = |ticket| OpenBundle {
            ticket,
            repo: repo_key(&repo),
            number: 1,
            head: "a".into(),
            base: "b".into(),
            reviews: Some(format!("[{ticket}]")),
            ..Default::default()
        };
        with_stash(|st| {
            st.latest = 7; // a newer open started and cleared the stash
            st.bundle = None;
        });
        store_if_newer(mk(3)); // older open finishes late: dropped
        assert!(take_stashed_reviews(&pr()).is_none());
        store_if_newer(mk(7));
        assert_eq!(take_stashed_reviews(&pr()).as_deref(), Some("[7]"));
    }

    #[test]
    fn stash_serves_only_the_exact_identity() {
        let repo = ForgeRepository::github("github.com", "o", "r");
        install_fake_chd(Some(std::rc::Rc::new(|_| {
            Some(r#"{"head":"a","base":"b","files":[1],"reviews":[2]}"#.to_string())
        })));
        assert_eq!(open_bundle(&repo, 1), Some(("a".into(), "b".into())));
        assert_eq!(stashed_files(&pr()).as_deref(), Some("[1]"));
        let mut retargeted = pr();
        retargeted.base_sha = "z".into();
        assert!(take_stashed_reviews(&retargeted).is_none());
        assert_eq!(take_stashed_reviews(&pr()).as_deref(), Some("[2]"));
        assert!(take_stashed_reviews(&pr()).is_none());
        let mut moved = pr();
        moved.head_sha = "z".into();
        assert!(stashed_files(&moved).is_none());
        install_fake_chd(None);
    }

    #[test]
    fn a_failed_reopen_clears_the_previous_bundle() {
        let repo = ForgeRepository::github("github.com", "o", "r");
        install_fake_chd(Some(std::rc::Rc::new(|_| {
            Some(r#"{"head":"a","base":"b","files":[1]}"#.to_string())
        })));
        assert!(open_bundle(&repo, 1).is_some());
        install_fake_chd(Some(std::rc::Rc::new(|_| None)));
        assert!(open_bundle(&repo, 1).is_none());
        assert!(stashed_files(&pr()).is_none());
        install_fake_chd(None);
    }

    #[test]
    fn absent_parts_are_none_so_callers_go_live() {
        let repo = ForgeRepository::github("github.com", "o", "r");
        install_fake_chd(Some(std::rc::Rc::new(|args| {
            assert_eq!(args, ["pr-cache", "open", "github.com", "o", "r", "1"]);
            Some(r#"{"head":"a","base":"b"}"#.to_string())
        })));
        assert!(open_bundle(&repo, 1).is_some());
        assert!(stashed_files(&pr()).is_none());
        assert!(take_stashed_reviews(&pr()).is_none());
        assert!(stashed_commits(&pr()).is_none());
        assert!(stashed_merge_base(&pr()).is_none());
        install_fake_chd(None);
    }
}
