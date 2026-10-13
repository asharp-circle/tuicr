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
    parts: InfoParts,
    viewer: Option<String>,
    /// Per-resource provenance, e.g. `files=validated_cache(pages=2,304=2)`.
    provenance: String,
}

/// Validated REST payloads for live `gh pr view` fields (`None` if unvalidated).
#[derive(Debug, Clone, Default)]
pub struct InfoParts {
    pub check_runs: Option<String>,
    pub statuses: Option<String>,
    pub issue_comments: Option<String>,
    pub requested: Option<String>,
}

/// Which optional info sections the caller displays, so chd skips the rest.
#[derive(Debug, Clone, Copy)]
pub struct InfoSelection {
    pub checks: bool,
    pub comments: bool,
}

impl Default for InfoSelection {
    fn default() -> Self {
        Self {
            checks: true,
            comments: true,
        }
    }
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
pub fn open_bundle(
    repo: &ForgeRepository,
    number: u64,
    wanted: InfoSelection,
) -> Option<(String, String)> {
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
    .chain((!wanted.checks).then(|| "--no-checks".to_string()))
    .chain((!wanted.comments).then(|| "--no-comments".to_string()))
    .collect();
    let out = crate::profile::time("pr.chd_open_bundle", || run_chd(&args, OPEN_TIMEOUT))?;
    let mut bundle = parse_open_bundle(&out, repo, number)?;
    bundle.ticket = ticket;
    let identity = (bundle.head.clone(), bundle.base.clone());
    crate::profile::time_with(
        "pr.chd_bundle_provenance",
        || (),
        |_| bundle.provenance.clone(),
    );
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
        parts: InfoParts {
            check_runs: rows("check_runs"),
            statuses: rows("statuses"),
            issue_comments: rows("issue_comments"),
            requested: v
                .get("requested")
                .filter(|r| r.is_object())
                .map(|r| r.to_string()),
        },
        viewer: sha("viewer"),
        provenance: summarize_provenance(v.get("provenance")),
    })
}

/// Flattens chd's per-resource provenance into one sorted, human-readable line.
fn summarize_provenance(prov: Option<&serde_json::Value>) -> String {
    let Some(map) = prov.and_then(|p| p.as_object()) else {
        return "none".to_string();
    };
    let mut parts: Vec<String> = map
        .iter()
        .map(|(name, r)| {
            let outcome = r
                .get("outcome")
                .and_then(|o| o.as_str())
                .unwrap_or("unknown");
            let n = |k: &str| r.get(k).and_then(|x| x.as_u64());
            match (n("pages"), n("http_200"), n("http_304")) {
                (Some(p), Some(a), Some(b)) => {
                    format!("{name}={outcome}(pages={p},200={a},304={b})")
                }
                _ => format!("{name}={outcome}"),
            }
        })
        .collect();
    parts.sort();
    parts.join(" ")
}

/// Records that a consumer parsed and accepted a bundle part. Call it only
/// after acceptance, so a part that falls back to a live fetch is not counted.
pub fn note_consumed(part: &'static str) {
    crate::profile::time_with("pr.chd_bundle_consumed", || (), |_| part.to_string());
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

/// Consumes the validated info parts for exactly this head and base tip.
pub fn take_stashed_info_parts(pr: &PullRequestDetails) -> Option<InfoParts> {
    with_stash(|st| {
        let bundle = st.bundle.as_mut()?;
        (bundle.repo == repo_key(&pr.repository)
            && bundle.number == pr.number
            && bundle.head == pr.head_sha
            && bundle.base == pr.base_sha)
            .then(|| std::mem::take(&mut bundle.parts))
    })
}

/// Copies the info parts without consuming them, to decide early whether the
/// bundle can complete the PR info or a full live query must start now.
pub fn peek_info_parts(pr: &PullRequestDetails) -> Option<InfoParts> {
    stashed(&pr.repository, pr.number, &pr.head_sha, &pr.base_sha, |b| {
        Some(b.parts.clone())
    })
}

/// Consumes the credential-validated viewer login for exactly this PR identity.
pub fn take_stashed_viewer(pr: &PullRequestDetails) -> Option<String> {
    with_stash(|st| {
        let bundle = st.bundle.as_mut()?;
        (bundle.repo == repo_key(&pr.repository)
            && bundle.number == pr.number
            && bundle.head == pr.head_sha
            && bundle.base == pr.base_sha)
            .then(|| bundle.viewer.take())
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
    fn provenance_is_summarized_per_resource_and_tolerates_absence() {
        let repo = ForgeRepository::github("github.com", "o", "r");
        let j = r#"{"head":"h","base":"b","provenance":{"files":{"outcome":"validated_cache","pages":2,"http_200":0,"http_304":2},"commits":{"outcome":"miss"},"pr":{"outcome":"validated_cache"}}}"#;
        let b = parse_open_bundle(j, &repo, 1).unwrap();
        assert_eq!(
            b.provenance,
            "commits=miss files=validated_cache(pages=2,200=0,304=2) pr=validated_cache"
        );
        let old = parse_open_bundle(r#"{"head":"h","base":"b"}"#, &repo, 1).unwrap();
        assert_eq!(old.provenance, "none");
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
        assert_eq!(
            open_bundle(&repo, 1, InfoSelection::default()),
            Some(("a".into(), "b".into()))
        );
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
        assert!(open_bundle(&repo, 1, InfoSelection::default()).is_some());
        install_fake_chd(Some(std::rc::Rc::new(|_| None)));
        assert!(open_bundle(&repo, 1, InfoSelection::default()).is_none());
        assert!(stashed_files(&pr()).is_none());
        install_fake_chd(None);
    }

    #[test]
    fn unrequested_sections_are_passed_to_chd_as_flags() {
        let repo = ForgeRepository::github("github.com", "o", "r");
        install_fake_chd(Some(std::rc::Rc::new(|args| {
            assert_eq!(&args[6..], ["--no-checks"]);
            Some(r#"{"head":"a","base":"b"}"#.to_string())
        })));
        let wanted = InfoSelection {
            checks: false,
            comments: true,
        };
        assert!(open_bundle(&repo, 1, wanted).is_some());
        install_fake_chd(None);
    }

    #[test]
    fn absent_parts_are_none_so_callers_go_live() {
        let repo = ForgeRepository::github("github.com", "o", "r");
        install_fake_chd(Some(std::rc::Rc::new(|args| {
            assert_eq!(args, ["pr-cache", "open", "github.com", "o", "r", "1"]);
            Some(r#"{"head":"a","base":"b"}"#.to_string())
        })));
        assert!(open_bundle(&repo, 1, InfoSelection::default()).is_some());
        assert!(stashed_files(&pr()).is_none());
        assert!(take_stashed_reviews(&pr()).is_none());
        assert!(stashed_commits(&pr()).is_none());
        assert!(stashed_merge_base(&pr()).is_none());
        install_fake_chd(None);
    }
}
