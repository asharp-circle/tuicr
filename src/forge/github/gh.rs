use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Result, TuicrError};
use crate::forge::local_git::read_blob;
use crate::forge::local_merge_base;
use crate::forge::remote_comments::{RemoteReviewComment, RemoteReviewSummary, RemoteReviewThread};
use crate::forge::traits::{
    ForgeBackend, ForgeFileLinesRequest, ForgeRepository, GhCreateReviewResponse,
    PagedPullRequests, PullRequestCommit, PullRequestDetails, PullRequestInfo,
    PullRequestListQuery, PullRequestListScope, PullRequestTarget,
};
use crate::model::{DiffLine, FilePatch, FileStatus};
use crate::process::{
    CommandOutputError, CommandOutputErrorKind, run_command_output, run_command_output_with_stdin,
};
use crate::vcs::git::raw::{FileMetadata, pair_metadata_with_patch, run_git_diff};
use crate::vcs::slice_context_lines;

use super::models::{GhCompare, GhPrCommit, GhPullRequestFile, GhPullRequestSummary};
use super::review_summaries::{
    build_query as build_reviews_query, parse_graphql_page as parse_reviews_page,
};
use super::review_threads::{build_query, parse_graphql_page};
use super::submit::build_review_payload;
use crate::forge::traits::CreateReviewRequest;

const DEFAULT_GITHUB_HOST: &str = "github.com";
const PR_LIST_JSON_FIELDS: &str =
    "number,title,author,headRefName,baseRefName,updatedAt,url,state,isDraft";
const PR_VIEW_JSON_FIELDS: &str = concat!(
    "number,title,url,state,isDraft,author,headRefName,baseRefName,",
    "headRefOid,baseRefOid,body,updatedAt,closed,mergedAt"
);
const PR_INFO_JSON_FIELDS: &str = concat!(
    "number,title,url,state,isDraft,author,headRefName,baseRefName,",
    "headRefOid,baseRefOid,body,updatedAt,closed,mergedAt,",
    "reviewDecision,mergeable,mergeStateStatus,reviewRequests,latestReviews,",
    "statusCheckRollup,comments"
);
/// Fields without a validated REST equivalent. chd REST supplies `reviewRequests`, the
/// check rollup and comments; `latestReviews` stays live because REST cannot reproduce
/// its per-author selection (see docs/CHD_CACHE_COVERAGE.md).
const PR_INFO_JSON_FIELDS_RESIDUAL: &str = concat!(
    "number,title,url,state,isDraft,author,headRefName,baseRefName,",
    "headRefOid,baseRefOid,body,updatedAt,closed,mergedAt,",
    "reviewDecision,mergeable,mergeStateStatus,latestReviews"
);
const PR_INFO_JSON_FIELDS_WITHOUT_CHECKS: &str = concat!(
    "number,title,url,state,isDraft,author,headRefName,baseRefName,",
    "headRefOid,baseRefOid,body,updatedAt,closed,mergedAt,",
    "reviewDecision,mergeable,mergeStateStatus,reviewRequests,latestReviews,comments"
);
const PR_INFO_JSON_FIELDS_WITHOUT_COMMENTS: &str = concat!(
    "number,title,url,state,isDraft,author,headRefName,baseRefName,",
    "headRefOid,baseRefOid,body,updatedAt,closed,mergedAt,",
    "reviewDecision,mergeable,mergeStateStatus,reviewRequests,latestReviews,",
    "statusCheckRollup"
);
const PR_INFO_JSON_FIELDS_WITHOUT_CHECKS_OR_COMMENTS: &str = concat!(
    "number,title,url,state,isDraft,author,headRefName,baseRefName,",
    "headRefOid,baseRefOid,body,updatedAt,closed,mergedAt,",
    "reviewDecision,mergeable,mergeStateStatus,reviewRequests,latestReviews"
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhCommandError {
    MissingGh,
    Failed { status: Option<i32>, stderr: String },
}

pub type GhCommandResult<T> = std::result::Result<T, GhCommandError>;

pub trait GhCommandRunner: Sync {
    fn run(&self, args: &[String]) -> GhCommandResult<String>;

    /// Variant for `gh` invocations that take their payload on stdin (e.g.
    /// `gh api ... --input -`). The default panics; concrete runners that
    /// might be reached by code needing stdin must override.
    fn run_with_stdin(&self, _args: &[String], _stdin: &str) -> GhCommandResult<String> {
        panic!("run_with_stdin not implemented for this GhCommandRunner");
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGhRunner;

impl GhCommandRunner for SystemGhRunner {
    fn run(&self, args: &[String]) -> GhCommandResult<String> {
        run_command_output("gh", None, args.iter().map(|arg| OsStr::new(arg.as_str())))
            .map_err(GhCommandError::from)
    }

    fn run_with_stdin(&self, args: &[String], stdin: &str) -> GhCommandResult<String> {
        run_command_output_with_stdin(
            "gh",
            None,
            args.iter().map(|arg| OsStr::new(arg.as_str())),
            stdin,
        )
        .map_err(GhCommandError::from)
    }
}

impl From<CommandOutputError> for GhCommandError {
    fn from(error: CommandOutputError) -> Self {
        match error.kind {
            CommandOutputErrorKind::NotFound => Self::MissingGh,
            CommandOutputErrorKind::SpawnFailed | CommandOutputErrorKind::Unsuccessful => {
                Self::Failed {
                    status: error.status,
                    stderr: error.stderr,
                }
            }
        }
    }
}

/// Return `Some(diff)` when both `start_sha` and `end_sha` are present in
/// the local checkout at `repo_root`, by running `git diff <start>..<end>`.
/// Returns `None` when the checkout is missing either SHA or the command
/// fails — callers fall back to the forge in that case.
fn local_range_diff(repo_root: &Path, start_sha: &str, end_sha: &str) -> Option<Vec<FilePatch>> {
    for sha in [start_sha, end_sha] {
        let exists = run_command_output(
            "git",
            Some(repo_root),
            ["cat-file", "-e", sha].iter().map(|s| OsStr::new(*s)),
        );
        if exists.is_err() {
            return None;
        }
    }
    let range = format!("{start_sha}..{end_sha}");
    run_git_diff(repo_root, &[range.as_str()]).ok()
}

#[derive(Debug, Clone)]
pub struct GitHubGhBackend<R = SystemGhRunner> {
    default_repository: Option<ForgeRepository>,
    runner: R,
    /// Optional path to a local checkout. When present, the backend may
    /// satisfy `fetch_file_lines` from local git objects before falling back
    /// to `gh api`. It is **never** used as the source of truth for PR
    /// contents; the source of truth is always GitHub.
    local_checkout: Option<PathBuf>,
    /// Whether `get_pull_request_info` requests the `statusCheckRollup` response from GitHub.
    show_pr_checks: bool,
    /// Whether `get_pull_request_info` requests pull-request conversation
    /// comments from GitHub.
    show_pr_comments: bool,
}

impl GitHubGhBackend<SystemGhRunner> {
    pub fn new(default_repository: Option<ForgeRepository>) -> Self {
        Self {
            default_repository,
            runner: SystemGhRunner,
            local_checkout: None,
            show_pr_checks: false,
            show_pr_comments: true,
        }
    }

    pub fn with_local_checkout(mut self, checkout: Option<PathBuf>) -> Self {
        self.local_checkout = checkout;
        self
    }
}

impl<R> GitHubGhBackend<R>
where
    R: GhCommandRunner,
{
    pub fn with_runner(default_repository: Option<ForgeRepository>, runner: R) -> Self {
        Self {
            default_repository,
            runner,
            local_checkout: None,
            show_pr_checks: false,
            show_pr_comments: true,
        }
    }

    pub fn set_local_checkout(&mut self, checkout: Option<PathBuf>) {
        self.local_checkout = checkout;
    }

    pub fn with_pr_checks(mut self, show_pr_checks: bool) -> Self {
        self.show_pr_checks = show_pr_checks;
        self
    }

    pub fn with_pr_comments(mut self, show_pr_comments: bool) -> Self {
        self.show_pr_comments = show_pr_comments;
        self
    }

    pub fn local_checkout(&self) -> Option<&Path> {
        self.local_checkout.as_deref()
    }

    fn pull_request_info_with_fields(
        &self,
        target: PullRequestTarget,
        fields: &str,
    ) -> Result<PullRequestInfo> {
        let repository = self.resolve_repository(&target)?;
        let output = self.run_gh(
            vec![
                "pr".to_string(),
                "view".to_string(),
                target.number.to_string(),
                "--repo".to_string(),
                gh_repo_arg(&repository),
                "--json".to_string(),
                fields.to_string(),
            ],
            &repository.host,
        )?;
        super::pr_info::parse_pull_request_info(&output, &repository)
    }

    /// Residual-only info for the overlapped open: fields with no validated REST equivalent.
    fn residual_pull_request_info(&self, target: PullRequestTarget) -> Result<PullRequestInfo> {
        self.pull_request_info_with_fields(target, PR_INFO_JSON_FIELDS_RESIDUAL)
    }

    /// Maps the validated REST parts the panel needs; `None` if any is missing or malformed.
    fn map_info_parts(
        &self,
        p: &crate::forge::chd_cache::InfoParts,
    ) -> Option<(
        Vec<String>,
        Vec<crate::forge::traits::PullRequestCheckStatus>,
        Vec<crate::forge::traits::PullRequestIssueComment>,
    )> {
        use super::pr_info::{checks_from_rest, comments_from_rest, requested_from_rest};
        let requested = requested_from_rest(p.requested.as_deref()?)?;
        let checks = if self.show_pr_checks {
            checks_from_rest(p.check_runs.as_deref()?, p.statuses.as_deref()?)?
        } else {
            Vec::new()
        };
        let comments = if self.show_pr_comments {
            comments_from_rest(p.issue_comments.as_deref()?)?
        } else {
            Vec::new()
        };
        Some((requested, checks, comments))
    }

    /// Whether the stashed bundle for this identity can complete the PR info.
    fn info_parts_ready(&self, pr: &PullRequestDetails) -> bool {
        crate::forge::chd_cache::peek_info_parts(pr)
            .and_then(|parts| self.map_info_parts(&parts))
            .is_some()
    }

    /// Fills residual checks, comments and reviewers from the chd bundle if identity
    /// matches and every part validated; otherwise one full live query, never empty sections.
    fn complete_pull_request_info(
        &self,
        target: PullRequestTarget,
        mut info: PullRequestInfo,
    ) -> Result<PullRequestInfo> {
        let mapped = crate::forge::chd_cache::take_stashed_info_parts(&info.details)
            .and_then(|parts| self.map_info_parts(&parts));
        let Some((requested, checks, comments)) = mapped else {
            return self.get_pull_request_info(target);
        };
        crate::forge::chd_cache::note_consumed("info.requested");
        if self.show_pr_checks {
            crate::forge::chd_cache::note_consumed("info.checks");
        }
        if self.show_pr_comments {
            crate::forge::chd_cache::note_consumed("info.issue_comments");
        }
        info.requested_reviewers = super::pr_info::sorted_requested(requested);
        info.checks = super::pr_info::sorted_checks(checks);
        info.issue_comments = comments;
        Ok(info)
    }

    fn resolve_repository(&self, target: &PullRequestTarget) -> Result<ForgeRepository> {
        target
            .repository
            .clone()
            .or_else(|| self.default_repository.clone())
            .ok_or_else(|| {
                TuicrError::Forge(format!(
                    "GitHub pull request target `{}` does not include a repository",
                    target.original
                ))
            })
    }

    fn run_gh(&self, args: Vec<String>, host: &str) -> Result<String> {
        self.runner
            .run(&args)
            .map_err(|err| map_gh_error(err, host))
    }

    /// Cumulative diff for `pr`, paired with already-fetched file metadata.
    fn diff_with_metadata(
        &self,
        pr: &PullRequestDetails,
        metadata: Vec<FileMetadata>,
    ) -> Result<Vec<FilePatch>> {
        // We want the *cumulative* diff between base and head for the PR.
        // `gh pr diff --patch` returns mbox-style `git format-patch` output
        // — one patch per commit — so a 7-commit PR yields 7 separate
        // `diff --git` blocks per file, which our parser dutifully turns
        // into duplicate `DiffFile`s. Plain `gh pr diff` (no `--patch`)
        // returns the single cumulative diff. Hard-won lesson; see the
        // duplicate-files-in-list bug.
        crate::forge::chd_cache::diff_with_cache(
            pr,
            |pr| {
                crate::forge::chd_cache::get_diff(pr).filter(|p| cached_headers_match(&metadata, p))
            },
            crate::forge::chd_cache::request_diff_fill,
            || {
                self.run_gh(
                    vec![
                        "pr".to_string(),
                        "diff".to_string(),
                        pr.number.to_string(),
                        "--repo".to_string(),
                        gh_repo_arg(&pr.repository),
                        "--color".to_string(),
                        "never".to_string(),
                    ],
                    &pr.repository.host,
                )
            },
            |patch| pair_metadata_with_patch(metadata.clone(), patch.as_bytes()),
        )
    }

    fn pull_request_file_metadata(&self, pr: &PullRequestDetails) -> Result<Vec<FileMetadata>> {
        if let Some(rows) = crate::forge::chd_cache::stashed_files(pr)
            && let Ok(rows) = serde_json::from_str::<Vec<GhPullRequestFile>>(&rows)
            && let Ok(metadata) = rows
                .into_iter()
                .map(GhPullRequestFile::into_metadata)
                .collect::<Result<Vec<_>>>()
        {
            crate::forge::chd_cache::note_consumed("files");
            return Ok(metadata);
        }
        self.pull_request_file_metadata_live(pr)
    }

    fn review_metadata_from_validated_pages(
        &self,
        pr: &PullRequestDetails,
    ) -> Option<crate::forge::traits::PullRequestReviewMetadata> {
        let rows = crate::forge::chd_cache::take_stashed_reviews(pr)?;
        let reviews = parse_rest_reviews(&rows)?;
        let viewer_login = match crate::forge::chd_cache::take_stashed_viewer(pr) {
            Some(login) => {
                crate::forge::chd_cache::note_consumed("viewer");
                login
            }
            None => self.current_viewer(&pr.repository).ok().flatten()?,
        };
        crate::forge::chd_cache::note_consumed("reviews");
        Some(crate::forge::traits::PullRequestReviewMetadata {
            viewer_login: Some(viewer_login),
            reviews,
        })
    }

    fn list_pull_request_commits_live(
        &self,
        pr: &PullRequestDetails,
    ) -> Result<Vec<PullRequestCommit>> {
        let mut commits: Vec<PullRequestCommit> = Vec::new();
        for page in 1..=10 {
            let endpoint = format!(
                "repos/{}/{}/pulls/{}/commits?per_page=100&page={}",
                pr.repository.owner, pr.repository.name, pr.number, page,
            );
            let mut args = vec!["api".to_string()];
            if pr.repository.host != DEFAULT_GITHUB_HOST {
                args.push("--hostname".to_string());
                args.push(pr.repository.host.clone());
            }
            args.push(endpoint);
            let output = self.run_gh(args, &pr.repository.host)?;
            let rows: Vec<GhPrCommit> = serde_json::from_str(&output)?;
            let received = rows.len();
            commits.extend(rows.into_iter().map(GhPrCommit::into_pull_request_commit));
            if received < 100 {
                break;
            }
        }
        Ok(commits)
    }

    fn pull_request_file_metadata_live(
        &self,
        pr: &PullRequestDetails,
    ) -> Result<Vec<FileMetadata>> {
        let mut metadata = Vec::new();
        for page in 1..=30 {
            let endpoint = format!(
                "repos/{}/{}/pulls/{}/files?per_page=100&page={page}",
                pr.repository.owner, pr.repository.name, pr.number
            );
            let mut args = vec!["api".to_string()];
            if pr.repository.host != DEFAULT_GITHUB_HOST {
                args.extend(["--hostname".to_string(), pr.repository.host.clone()]);
            }
            args.push(endpoint);
            let output = self.run_gh(args, &pr.repository.host)?;
            let rows: Vec<GhPullRequestFile> = serde_json::from_str(&output)?;
            let received = rows.len();
            metadata.extend(
                rows.into_iter()
                    .map(GhPullRequestFile::into_metadata)
                    .collect::<Result<Vec<_>>>()?,
            );
            if received < 100 {
                return Ok(metadata);
            }
        }
        Err(TuicrError::Forge(
            "GitHub pull request exceeds the 3000-file REST API limit".into(),
        ))
    }
}

impl<R> ForgeBackend for GitHubGhBackend<R>
where
    R: GhCommandRunner,
{
    fn list_pull_requests(&self, query: PullRequestListQuery) -> Result<PagedPullRequests> {
        let page_size = query.page_size.max(1);
        let requested = query.already_loaded + page_size + 1;
        let mut args = vec![
            "pr".to_string(),
            "list".to_string(),
            "--repo".to_string(),
            gh_repo_arg(&query.repository),
            "--state".to_string(),
            "open".to_string(),
            "--limit".to_string(),
            requested.to_string(),
            "--json".to_string(),
            PR_LIST_JSON_FIELDS.to_string(),
        ];
        if query.scope == PullRequestListScope::ReviewRequested {
            args.push("--search".to_string());
            args.push("review-requested:@me".to_string());
        }
        let output = self.run_gh(args, &query.repository.host)?;
        let rows: Vec<GhPullRequestSummary> = serde_json::from_str(&output)?;
        let has_more = rows.len() > query.already_loaded + page_size;
        let pull_requests = rows
            .into_iter()
            .skip(query.already_loaded)
            .take(page_size)
            .map(|row| row.into_summary(&query.repository))
            .collect::<Vec<_>>();
        let total_loaded = query.already_loaded + pull_requests.len();

        Ok(PagedPullRequests {
            pull_requests,
            has_more,
            total_loaded,
        })
    }

    fn get_pull_request(&self, target: PullRequestTarget) -> Result<PullRequestDetails> {
        Ok(self.get_pull_request_info(target)?.details)
    }

    fn get_pull_request_info(&self, target: PullRequestTarget) -> Result<PullRequestInfo> {
        let fields = match (self.show_pr_checks, self.show_pr_comments) {
            (true, true) => PR_INFO_JSON_FIELDS,
            (false, true) => PR_INFO_JSON_FIELDS_WITHOUT_CHECKS,
            (true, false) => PR_INFO_JSON_FIELDS_WITHOUT_COMMENTS,
            (false, false) => PR_INFO_JSON_FIELDS_WITHOUT_CHECKS_OR_COMMENTS,
        };
        self.pull_request_info_with_fields(target, fields)
    }

    fn fetch_pr_data_overlapped(
        &self,
        target: &PullRequestTarget,
    ) -> Option<Result<crate::forge::pr_open::PrFetchData>> {
        if !crate::forge::chd_cache::is_enabled() {
            return None;
        }
        let repository = self.resolve_repository(target).ok()?;
        Some(overlapped_open(
            || {
                let (head_sha, base_sha) = crate::forge::chd_cache::open_bundle(
                    &repository,
                    target.number,
                    crate::forge::chd_cache::InfoSelection {
                        checks: self.show_pr_checks,
                        comments: self.show_pr_comments,
                    },
                )?;
                Some(identity_seed(
                    repository.clone(),
                    target.number,
                    head_sha,
                    base_sha,
                ))
            },
            || self.residual_pull_request_info(target.clone()),
            || self.get_pull_request_info(target.clone()),
            |seed| self.info_parts_ready(seed),
            |details| self.fetch_diff_commits_metadata(details),
            |info| self.complete_pull_request_info(target.clone(), info),
        ))
    }

    fn fetch_diff_commits_metadata(
        &self,
        mut details: PullRequestDetails,
    ) -> Result<(
        PullRequestDetails,
        Vec<FilePatch>,
        Vec<PullRequestCommit>,
        crate::forge::traits::PullRequestReviewMetadata,
    )> {
        // Commits, review metadata and the file list depend only on the PR
        // number, so they overlap with the merge-base lookup. The diff itself
        // still waits for the refined base: its cache identity is (head, base).
        let original = details.clone();
        let (patches, commits, review_metadata) = std::thread::scope(|scope| {
            let commits = scope.spawn(|| self.list_pull_request_commits(&original));
            let metadata = scope.spawn(|| self.list_pull_request_review_metadata(&original));
            let files = scope.spawn(|| self.pull_request_file_metadata(&original));
            if let Some(base_sha) = self.resolve_diff_base_sha(&details) {
                details.base_sha = base_sha;
            }
            let patches = files
                .join()
                .map_err(|_| TuicrError::Forge("file metadata worker panicked".into()))
                .and_then(|files| files)
                .and_then(|files| self.diff_with_metadata(&details, files));
            let commits = commits.join().ok().and_then(|r| r.ok()).unwrap_or_default();
            let metadata = metadata
                .join()
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            (patches, commits, metadata)
        });
        Ok((details, patches?, commits, review_metadata))
    }

    fn get_pull_request_diff(&self, pr: &PullRequestDetails) -> Result<Vec<FilePatch>> {
        let metadata = self.pull_request_file_metadata(pr)?;
        self.diff_with_metadata(pr, metadata)
    }

    fn local_checkout_path(&self) -> Option<PathBuf> {
        self.local_checkout.clone()
    }

    fn list_pull_request_commits(&self, pr: &PullRequestDetails) -> Result<Vec<PullRequestCommit>> {
        // GitHub paginates `pulls/<num>/commits` at 250 commits per page (max
        // per_page=100; PRs are capped at 250 commits via the API). Paginate
        // explicitly so multi-page PRs work; bound the loop defensively.
        if let Some((rows, source)) = crate::forge::chd_cache::get_list_sourced(pr, "commits")
            .and_then(|(json, source)| {
                serde_json::from_str::<Vec<GhPrCommit>>(&json)
                    .ok()
                    .map(|rows| (rows, source))
            })
        {
            crate::forge::chd_cache::note_consumed(source);
            return Ok(rows
                .into_iter()
                .map(GhPrCommit::into_pull_request_commit)
                .collect());
        }
        let commits = self.list_pull_request_commits_live(pr)?;
        // GitHub truncates PR commit lists at 250; chd can never verify those.
        if commits_cacheable(commits.len()) {
            crate::forge::chd_cache::request_list_fill(pr, "commits");
        }
        Ok(commits)
    }

    fn get_pull_request_commit_range_diff(
        &self,
        pr: &PullRequestDetails,
        start_sha: &str,
        end_sha: &str,
    ) -> Result<Vec<FilePatch>> {
        // Fast path: when both SHAs live in the local checkout, `git diff`
        // gives us the cumulative diff in O(local-IO) without round-tripping
        // through GitHub. The PR diff text is the source of truth, but the
        // forge produces equivalent output for the same two SHAs, so this
        // is a safe optimization.
        if let Some(root) = self.local_checkout.as_deref()
            && let Some(diff) = local_range_diff(root, start_sha, end_sha)
        {
            return Ok(diff);
        }

        // The JSON response provides authoritative file metadata; a second
        // request asks for the full patch. Their shared source order lets us
        // pair them without inspecting display-oriented path headers.
        let endpoint = format!(
            "repos/{}/{}/compare/{}...{}",
            pr.repository.owner, pr.repository.name, start_sha, end_sha,
        );
        let mut metadata_args = vec!["api".to_string()];
        if pr.repository.host != DEFAULT_GITHUB_HOST {
            metadata_args.extend(["--hostname".to_string(), pr.repository.host.clone()]);
        }
        metadata_args.push(endpoint.clone());
        let metadata_output = self.run_gh(metadata_args, &pr.repository.host)?;
        let compare: GhCompare = serde_json::from_str(&metadata_output)?;
        let metadata = compare
            .files
            .into_iter()
            .map(GhPullRequestFile::into_metadata)
            .collect::<Result<Vec<_>>>()?;

        let mut args = vec![
            "api".to_string(),
            "-H".to_string(),
            "Accept: application/vnd.github.diff".to_string(),
        ];
        if pr.repository.host != DEFAULT_GITHUB_HOST {
            args.push("--hostname".to_string());
            args.push(pr.repository.host.clone());
        }
        args.push(endpoint);
        let patch = self.run_gh(args, &pr.repository.host)?;
        pair_metadata_with_patch(metadata, patch.as_bytes())
    }

    fn resolve_diff_base_sha(&self, pr: &PullRequestDetails) -> Option<String> {
        // GraphQL's `baseRefOid` usually equals the merge base, but it tracks
        // the base branch tip once the base moves under a long-lived PR. The
        // PR diff stays three-dot, so the old side would then be read at the
        // wrong revision.
        if let Some(root) = self.local_checkout.as_deref()
            && let Some(sha) = local_merge_base(root, &pr.base_sha, &pr.head_sha)
        {
            return Some(sha);
        }

        if let Some(sha) = crate::forge::chd_cache::stashed_merge_base(pr) {
            crate::forge::chd_cache::note_consumed("merge_base");
            return Some(sha);
        }

        // `per_page=1` trims the `commits` array; `files` is returned either
        // way, so this stays one modest request rather than a cheap one.
        let endpoint = format!(
            "repos/{}/{}/compare/{}...{}?per_page=1",
            pr.repository.owner, pr.repository.name, pr.base_sha, pr.head_sha,
        );
        let mut args = vec!["api".to_string()];
        if pr.repository.host != DEFAULT_GITHUB_HOST {
            args.push("--hostname".to_string());
            args.push(pr.repository.host.clone());
        }
        args.push(endpoint);
        let output = self.run_gh(args, &pr.repository.host).ok()?;
        let comparison: GhCompare = serde_json::from_str(&output).ok()?;
        comparison
            .merge_base_commit
            .map(|commit| commit.sha)
            .filter(|sha| !sha.is_empty())
    }

    fn current_viewer(&self, repo: &ForgeRepository) -> Result<Option<String>> {
        let mut args = vec!["api".to_string(), "user".to_string()];
        if repo.host != DEFAULT_GITHUB_HOST {
            args.extend(["--hostname".to_string(), repo.host.clone()]);
        }
        let output = self.run_gh(args, &repo.host)?;
        let user: serde_json::Value = serde_json::from_str(&output)?;
        Ok(user
            .get("login")
            .and_then(|login| login.as_str())
            .map(str::to_string))
    }

    fn toggle_comment_reaction(
        &self,
        repo: &ForgeRepository,
        comment_id: &str,
        content: &str,
        remove: bool,
    ) -> Result<()> {
        let (field, query) = if remove {
            (
                "removeReaction",
                "mutation($id: ID!, $content: ReactionContent!) { removeReaction(input: {subjectId: $id, content: $content}) { subject { id } } }",
            )
        } else {
            (
                "addReaction",
                "mutation($id: ID!, $content: ReactionContent!) { addReaction(input: {subjectId: $id, content: $content}) { subject { id } } }",
            )
        };
        let mut args = vec![
            "api".into(),
            "graphql".into(),
            "-f".into(),
            format!("query={query}"),
            "-f".into(),
            format!("id={comment_id}"),
            "-f".into(),
            format!("content={content}"),
        ];
        if repo.host != DEFAULT_GITHUB_HOST {
            args.extend(["--hostname".into(), repo.host.clone()]);
        }
        let output = self.run_gh(args, &repo.host)?;
        let response: serde_json::Value = serde_json::from_str(&output)?;
        if let Some(errors) = response.get("errors") {
            return Err(TuicrError::Forge(format!(
                "GitHub reaction failed: {errors}"
            )));
        }
        if response
            .pointer(&format!("/data/{field}/subject/id"))
            .and_then(|v| v.as_str())
            != Some(comment_id)
        {
            return Err(TuicrError::Forge(
                "GitHub did not confirm reaction update".into(),
            ));
        }
        Ok(())
    }

    fn set_review_thread_resolved(
        &self,
        repo: &ForgeRepository,
        id: &str,
        resolved: bool,
    ) -> Result<()> {
        let (mutation, field) = if resolved {
            ("resolveReviewThread", "resolveReviewThread")
        } else {
            ("unresolveReviewThread", "unresolveReviewThread")
        };
        let mut args = vec![
            "api".to_string(),
            "graphql".to_string(),
            "-f".to_string(),
            format!(
                "query=mutation($id: ID!) {{ {mutation}(input: {{threadId: $id}}) {{ thread {{ id isResolved }} }} }}"
            ),
            "-f".to_string(),
            format!("id={id}"),
        ];
        if repo.host != DEFAULT_GITHUB_HOST {
            args.extend(["--hostname".to_string(), repo.host.clone()]);
        }
        let output = self.run_gh(args, &repo.host)?;
        let response: serde_json::Value = serde_json::from_str(&output)?;
        if let Some(errors) = response.get("errors") {
            return Err(TuicrError::Forge(format!(
                "GitHub thread resolution failed: {errors}"
            )));
        }
        let thread = response.pointer(&format!("/data/{field}/thread"));
        if thread.and_then(|t| t.get("id")).and_then(|v| v.as_str()) != Some(id)
            || thread
                .and_then(|t| t.get("isResolved"))
                .and_then(|v| v.as_bool())
                != Some(resolved)
        {
            return Err(TuicrError::Forge(
                "GitHub did not confirm thread resolution".into(),
            ));
        }
        Ok(())
    }

    fn reply_to_review_thread(
        &self,
        repo: &ForgeRepository,
        thread_id: &str,
        body: &str,
    ) -> Result<RemoteReviewComment> {
        let query = "mutation($threadId: ID!, $body: String!) { addPullRequestReviewThreadReply(input: {pullRequestReviewThreadId: $threadId, body: $body}) { comment { id body createdAt url author { login } replyTo { id } pullRequestReview { id fullDatabaseId } } } }";
        let mut args = vec![
            "api".to_string(),
            "graphql".to_string(),
            "-f".to_string(),
            format!("query={query}"),
            "-f".to_string(),
            format!("threadId={thread_id}"),
            "-f".to_string(),
            format!("body={body}"),
        ];
        if repo.host != DEFAULT_GITHUB_HOST {
            args.extend(["--hostname".to_string(), repo.host.clone()]);
        }
        let output = self.run_gh(args, &repo.host)?;
        let response: serde_json::Value = serde_json::from_str(&output)?;
        if let Some(errors) = response.get("errors") {
            return Err(TuicrError::Forge(format!(
                "GitHub thread reply failed: {errors}"
            )));
        }
        let comment = response
            .pointer("/data/addPullRequestReviewThreadReply/comment")
            .ok_or_else(|| TuicrError::Forge("GitHub did not confirm thread reply".into()))?;
        let id = comment
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
            .ok_or_else(|| TuicrError::Forge("GitHub thread reply missing comment ID".into()))?;
        Ok(RemoteReviewComment {
            id: id.to_string(),
            author: comment
                .pointer("/author/login")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            body: comment
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or(body)
                .to_string(),
            created_at: comment
                .get("createdAt")
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc)),
            in_reply_to: comment
                .pointer("/replyTo/id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            url: comment
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            review_id: comment
                .pointer("/pullRequestReview/id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            review_database_id: comment
                .pointer("/pullRequestReview/fullDatabaseId")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            reactions: Vec::new(),
        })
    }

    fn update_review_comment(&self, repo: &ForgeRepository, id: &str, body: &str) -> Result<()> {
        let mut args = vec!["api".to_string(), "graphql".to_string()];
        let payload = serde_json::json!({"query": "mutation($id: ID!, $body: String!) { updatePullRequestReviewComment(input: {pullRequestReviewCommentId: $id, body: $body}) { pullRequestReviewComment { id body } } }", "variables": {"id": id, "body": body}});
        if repo.host != DEFAULT_GITHUB_HOST {
            args.extend(["--hostname".to_string(), repo.host.clone()]);
        }
        args.extend(["--input".to_string(), "-".to_string()]);
        let output = self
            .runner
            .run_with_stdin(&args, &payload.to_string())
            .map_err(|err| map_gh_error(err, &repo.host))?;
        let response: serde_json::Value = serde_json::from_str(&output)?;
        if let Some(errors) = response.get("errors") {
            return Err(TuicrError::Forge(format!(
                "GitHub comment update failed: {errors}"
            )));
        }
        if response
            .pointer("/data/updatePullRequestReviewComment/pullRequestReviewComment/id")
            .and_then(|v| v.as_str())
            != Some(id)
            || response
                .pointer("/data/updatePullRequestReviewComment/pullRequestReviewComment/body")
                .and_then(|v| v.as_str())
                != Some(body)
        {
            return Err(TuicrError::Forge(
                "GitHub did not confirm comment update".into(),
            ));
        }
        Ok(())
    }

    fn delete_review_comment(&self, repo: &ForgeRepository, id: &str) -> Result<()> {
        let mut args = vec!["api".to_string(), "graphql".to_string(),
            "-f".to_string(), "query=mutation($id: ID!) { deletePullRequestReviewComment(input: {id: $id}) { clientMutationId } }".to_string(),
            "-f".to_string(), format!("id={id}")];
        if repo.host != DEFAULT_GITHUB_HOST {
            args.extend(["--hostname".to_string(), repo.host.clone()]);
        }
        let output = self.run_gh(args, &repo.host)?;
        let response: serde_json::Value = serde_json::from_str(&output)?;
        if let Some(errors) = response.get("errors") {
            return Err(TuicrError::Forge(format!(
                "GitHub comment deletion failed: {errors}"
            )));
        }
        if response
            .pointer("/data/deletePullRequestReviewComment")
            .is_none()
        {
            return Err(TuicrError::Forge(
                "GitHub did not confirm comment deletion".into(),
            ));
        }
        Ok(())
    }

    fn delete_review(&self, repo: &ForgeRepository, id: &str) -> Result<()> {
        let mut args = vec!["api".to_string(), "graphql".to_string(),
            "-f".to_string(), "query=mutation($id: ID!) { updatePullRequestReview(input: {pullRequestReviewId: $id, body: \"\"}) { clientMutationId } }".to_string(),
            "-f".to_string(), format!("id={id}")];
        if repo.host != DEFAULT_GITHUB_HOST {
            args.extend(["--hostname".to_string(), repo.host.clone()]);
        }
        let output = self.run_gh(args, &repo.host)?;
        let response: serde_json::Value = serde_json::from_str(&output)?;
        if let Some(errors) = response.get("errors") {
            return Err(TuicrError::Forge(format!(
                "GitHub review deletion failed: {errors}"
            )));
        }
        if response.pointer("/data/updatePullRequestReview").is_none() {
            return Err(TuicrError::Forge(
                "GitHub did not confirm review summary deletion".into(),
            ));
        }
        Ok(())
    }

    fn list_review_threads(&self, pr: &PullRequestDetails) -> Result<Vec<RemoteReviewThread>> {
        let mut all: Vec<RemoteReviewThread> = Vec::new();
        let mut cursor: Option<String> = None;
        // Bound the pagination loop so a buggy/cyclic server can't hang us.
        // 100 threads * 100 pages = 10k threads; well beyond realistic PRs.
        for _ in 0..100 {
            let args = self.build_review_threads_args(pr, cursor.as_deref());
            let output = self.run_gh(args, &pr.repository.host)?;
            let parsed = parse_graphql_page(&output)?;
            all.extend(parsed.threads);
            let Some(page_info) = parsed.page_info else {
                break;
            };
            if !page_info.has_next_page {
                break;
            }
            let Some(end_cursor) = page_info.end_cursor else {
                break;
            };
            cursor = Some(end_cursor);
        }
        Ok(all)
    }

    fn list_review_summaries(&self, pr: &PullRequestDetails) -> Result<Vec<RemoteReviewSummary>> {
        let mut all: Vec<RemoteReviewSummary> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..100 {
            let args = self.build_review_summaries_args(pr, cursor.as_deref());
            let output = self.run_gh(args, &pr.repository.host)?;
            let parsed = parse_reviews_page(&output)?;
            all.extend(parsed.summaries);
            let Some(page_info) = parsed.page_info else {
                break;
            };
            if !page_info.has_next_page {
                break;
            }
            let Some(end_cursor) = page_info.end_cursor else {
                break;
            };
            cursor = Some(end_cursor);
        }
        Ok(all)
    }

    fn list_pull_request_review_metadata(
        &self,
        pr: &PullRequestDetails,
    ) -> Result<crate::forge::traits::PullRequestReviewMetadata> {
        if let Some(rest) = self.review_metadata_from_validated_pages(pr) {
            return Ok(rest);
        }
        let mut all = crate::forge::traits::PullRequestReviewMetadata::default();
        let mut cursor: Option<String> = None;
        for _ in 0..100 {
            let args = self.build_review_metadata_args(pr, cursor.as_deref());
            let output = self.run_gh(args, &pr.repository.host)?;
            let parsed = super::review_metadata::parse_graphql_page(&output)?;
            if all.viewer_login.is_none() {
                all.viewer_login = parsed.metadata.viewer_login;
            }
            all.reviews.extend(parsed.metadata.reviews);
            let Some(page_info) = parsed.page_info else {
                break;
            };
            if !page_info.has_next_page {
                break;
            }
            let Some(end_cursor) = page_info.end_cursor else {
                break;
            };
            cursor = Some(end_cursor);
        }
        Ok(all)
    }

    fn fetch_file_lines(&self, request: ForgeFileLinesRequest) -> Result<Vec<DiffLine>> {
        if request.start_line == 0 || request.start_line > request.end_line {
            return Ok(Vec::new());
        }
        let (start_line, end_line) = (request.start_line, request.end_line);
        let content = self.fetch_file_content(request)?;
        Ok(slice_context_lines(&content, start_line, end_line))
    }

    /// Local blob when the checkout has the PR's SHA, REST otherwise. The PR's
    /// exact SHAs may or may not be present locally; we silently fall back.
    fn fetch_file_content(&self, request: ForgeFileLinesRequest) -> Result<String> {
        match self
            .local_checkout
            .as_deref()
            .and_then(|root| read_blob(root, request.sha(), request.path.as_path()))
        {
            Some(content) => Ok(content),
            None => self.fetch_file_via_api(&request),
        }
    }

    fn file_line_count(&self, request: ForgeFileLinesRequest) -> Result<u32> {
        let content = self.fetch_file_content(request)?;
        Ok(content.lines().count() as u32)
    }

    fn create_review(
        &self,
        pr: &PullRequestDetails,
        request: CreateReviewRequest<'_>,
    ) -> Result<GhCreateReviewResponse> {
        let payload = build_review_payload(
            request.commit_id,
            request.body,
            request.event,
            request.comments,
        );
        let payload_json = serde_json::to_string(&payload)?;

        let endpoint = format!(
            "repos/{}/{}/pulls/{}/reviews",
            pr.repository.owner, pr.repository.name, pr.number,
        );
        let mut args = vec![
            "api".to_string(),
            endpoint,
            "--method".to_string(),
            "POST".to_string(),
            "--input".to_string(),
            "-".to_string(),
        ];
        if pr.repository.host != DEFAULT_GITHUB_HOST {
            args.push("--hostname".to_string());
            args.push(pr.repository.host.clone());
        }

        let output = self
            .runner
            .run_with_stdin(&args, &payload_json)
            .map_err(|err| map_create_review_error(err, &pr.repository.host))?;

        parse_create_review_response(&output)
    }

    fn has_pending_review(&self, repository: &ForgeRepository, number: u64) -> Result<bool> {
        let endpoint = format!(
            "repos/{}/{}/pulls/{number}/reviews",
            repository.owner, repository.name
        );
        self.find_pending_review(repository, &endpoint)
            .map(|id| id.is_some())
    }

    fn submit_pending_review(
        &self,
        repository: &ForgeRepository,
        number: u64,
        event: crate::forge::submit::SubmitEvent,
        commit_id: &str,
        body: &str,
        comments: &[crate::forge::submit::InlineComment],
    ) -> Result<Option<GhCreateReviewResponse>> {
        let endpoint = format!(
            "repos/{}/{}/pulls/{number}/reviews",
            repository.owner, repository.name
        );
        let Some((id, rev_commit)) = self.find_pending_review(repository, &endpoint)? else {
            return Ok(None);
        };
        if !rev_commit.is_empty()
            && !commit_id.is_empty()
            && rev_commit != commit_id
            && !comments.is_empty()
        {
            return Err(TuicrError::Forge(format!(
                "Existing pending review is on commit {rev_commit}, but comments are on {commit_id}"
            )));
        }
        let existing = self.list_pending_review_comments(repository, &endpoint, id)?;
        for comment in comments {
            let path_str = comment.path.to_string_lossy();
            let comment_side = comment.side.as_str();
            let comment_start = comment.start_line.map(|l| l as u64);
            if existing.iter().any(|c| {
                c.path == path_str.as_ref()
                    && c.line == comment.line as u64
                    && c.side.eq_ignore_ascii_case(comment_side)
                    && c.start_line == comment_start
                    && c.body == comment.body
            }) {
                continue;
            }
            self.post_existing_pending_comment(repository, number, id, comment)?;
        }
        if event == crate::forge::submit::SubmitEvent::Draft {
            if !body.is_empty() {
                let mut args = vec![
                    "api".to_string(),
                    format!("{endpoint}/{id}"),
                    "--method".into(),
                    "PUT".into(),
                    "--input".into(),
                    "-".into(),
                ];
                if repository.host != DEFAULT_GITHUB_HOST {
                    args.extend(["--hostname".into(), repository.host.clone()]);
                }
                let payload = serde_json::json!({ "body": body });
                let output = self
                    .runner
                    .run_with_stdin(&args, &payload.to_string())
                    .map_err(|e| map_gh_error(e, &repository.host))?;
                return Ok(Some(parse_create_review_response(&output)?));
            }
            return Ok(Some(GhCreateReviewResponse {
                id,
                html_url: String::new(),
                state: "PENDING".into(),
            }));
        }
        let mut args = vec![
            "api".to_string(),
            format!("{endpoint}/{id}/events"),
            "--method".into(),
            "POST".into(),
            "--input".into(),
            "-".into(),
        ];
        if repository.host != DEFAULT_GITHUB_HOST {
            args.extend(["--hostname".into(), repository.host.clone()]);
        }
        let payload = serde_json::json!({ "event": event.github_event(), "body": body });
        let output = self
            .runner
            .run_with_stdin(&args, &payload.to_string())
            .map_err(|e| map_gh_error(e, &repository.host))?;
        Ok(Some(parse_create_review_response(&output)?))
    }

    fn add_pending_comment(
        &self,
        repository: &ForgeRepository,
        number: u64,
        commit_id: &str,
        comment: &crate::forge::submit::InlineComment,
    ) -> Result<u64> {
        let endpoint = format!(
            "repos/{}/{}/pulls/{number}/reviews",
            repository.owner, repository.name
        );
        let review_info = self.find_pending_review(repository, &endpoint)?;
        if let Some((id, pending_commit)) = review_info {
            if !pending_commit.is_empty() && !commit_id.is_empty() && pending_commit != commit_id {
                return Err(TuicrError::Forge(format!(
                    "Existing pending review is on commit {pending_commit}, but comment is on {commit_id}"
                )));
            }
            self.post_existing_pending_comment(repository, number, id, comment)?;
            Ok(id)
        } else {
            let payload = build_review_payload(
                commit_id,
                "",
                crate::forge::submit::SubmitEvent::Draft,
                std::slice::from_ref(comment),
            );
            let mut args = vec![
                "api".to_string(),
                endpoint,
                "--method".into(),
                "POST".into(),
                "--input".into(),
                "-".into(),
            ];
            if repository.host != DEFAULT_GITHUB_HOST {
                args.extend(["--hostname".to_string(), repository.host.clone()]);
            }
            let output = self
                .runner
                .run_with_stdin(&args, &payload.to_string())
                .map_err(|e| map_create_review_error(e, &repository.host))?;
            Ok(parse_create_review_response(&output)?.id)
        }
    }
}

struct PendingReviewComment {
    path: String,
    line: u64,
    side: String,
    start_line: Option<u64>,
    body: String,
}

impl<R> GitHubGhBackend<R>
where
    R: GhCommandRunner,
{
    fn list_pending_review_comments(
        &self,
        repository: &ForgeRepository,
        endpoint: &str,
        id: u64,
    ) -> Result<Vec<PendingReviewComment>> {
        let mut results = Vec::new();
        for page in 1..=100 {
            let mut args = vec![
                "api".to_string(),
                format!("{endpoint}/{id}/comments?per_page=100&page={page}"),
            ];
            if repository.host != DEFAULT_GITHUB_HOST {
                args.extend(["--hostname".into(), repository.host.clone()]);
            }
            let output = self.run_gh(args, &repository.host)?;
            let rows: Vec<serde_json::Value> = serde_json::from_str(&output)?;
            let len = rows.len();
            for row in rows {
                let Some(path) = row["path"].as_str().map(str::to_string) else {
                    continue;
                };
                let Some(line) = row["line"]
                    .as_u64()
                    .or_else(|| row["original_line"].as_u64())
                else {
                    continue;
                };
                let side = row["side"].as_str().unwrap_or("RIGHT").to_string();
                let start_line = row["start_line"].as_u64();
                let Some(body) = row["body"].as_str().map(str::to_string) else {
                    continue;
                };
                results.push(PendingReviewComment {
                    path,
                    line,
                    side,
                    start_line,
                    body,
                });
            }
            if len < 100 {
                return Ok(results);
            }
        }
        Ok(results)
    }

    fn find_pending_review(
        &self,
        repository: &ForgeRepository,
        endpoint: &str,
    ) -> Result<Option<(u64, String)>> {
        let viewer = self.current_viewer(repository)?.ok_or_else(|| {
            TuicrError::Forge("GitHub did not return the current viewer login".into())
        })?;
        for page in 1..=100 {
            let mut args = vec![
                "api".to_string(),
                format!("{endpoint}?per_page=100&page={page}"),
            ];
            if repository.host != DEFAULT_GITHUB_HOST {
                args.extend(["--hostname".into(), repository.host.clone()]);
            }
            let output = self.run_gh(args, &repository.host)?;
            let rows: Vec<serde_json::Value> = serde_json::from_str(&output)?;
            let len = rows.len();
            if let Some((id, commit_id)) = rows
                .iter()
                .find(|row| row["state"] == "PENDING" && row["user"]["login"] == viewer)
                .and_then(|row| {
                    let id = row["id"].as_u64()?;
                    let commit_id = row["commit_id"].as_str().unwrap_or("").to_string();
                    Some((id, commit_id))
                })
            {
                return Ok(Some((id, commit_id)));
            }
            if len < 100 {
                return Ok(None);
            }
        }
        Err(TuicrError::Forge(
            "GitHub review list exceeded pagination limit".into(),
        ))
    }

    fn post_existing_pending_comment(
        &self,
        repository: &ForgeRepository,
        number: u64,
        id: u64,
        comment: &crate::forge::submit::InlineComment,
    ) -> Result<()> {
        let review_node = self.review_node_id(repository, number, id)?;
        let mut input = serde_json::json!({
            "pullRequestReviewId": review_node,
            "path": comment.path.to_string_lossy(),
            "body": comment.body,
            "line": comment.line,
            "side": comment.side.as_str(),
        });
        if let Some(start) = comment.start_line {
            input["startLine"] = start.into();
            input["startSide"] = comment.start_side.unwrap_or(comment.side).as_str().into();
        }
        let query = "mutation($input: AddPullRequestReviewThreadInput!) { addPullRequestReviewThread(input: $input) { thread { id } } }";
        let mut args = vec![
            "api".to_string(),
            "graphql".into(),
            "--input".into(),
            "-".into(),
        ];
        if repository.host != DEFAULT_GITHUB_HOST {
            args.extend(["--hostname".into(), repository.host.clone()]);
        }
        let payload = serde_json::json!({ "query": query, "variables": { "input": input } });
        let output = self
            .runner
            .run_with_stdin(&args, &payload.to_string())
            .map_err(|error| map_gh_error(error, &repository.host))?;
        let response: serde_json::Value = serde_json::from_str(&output)?;
        if let Some(errors) = response.get("errors") {
            return Err(TuicrError::Forge(format!(
                "GitHub pending comment failed: {errors}"
            )));
        }
        if response
            .pointer("/data/addPullRequestReviewThread/thread/id")
            .and_then(|id| id.as_str())
            .is_none()
        {
            return Err(TuicrError::Forge(
                "GitHub did not confirm pending comment".into(),
            ));
        }
        Ok(())
    }

    fn review_node_id(&self, repository: &ForgeRepository, number: u64, id: u64) -> Result<String> {
        let query = "query($owner: String!, $name: String!, $number: Int!, $cursor: String) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { reviews(first: 100, after: $cursor, states: PENDING) { nodes { id databaseId fullDatabaseId } pageInfo { hasNextPage endCursor } } } } }";
        let mut cursor: Option<String> = None;
        loop {
            let mut args = vec![
                "api".to_string(),
                "graphql".into(),
                "-f".into(),
                format!("query={query}"),
                "-f".into(),
                format!("owner={}", repository.owner),
                "-f".into(),
                format!("name={}", repository.name),
                "-F".into(),
                format!("number={number}"),
            ];
            if let Some(value) = &cursor {
                args.extend(["-f".into(), format!("cursor={value}")]);
            }
            if repository.host != DEFAULT_GITHUB_HOST {
                args.extend(["--hostname".into(), repository.host.clone()]);
            }
            let output = self.run_gh(args, &repository.host)?;
            let response: serde_json::Value = serde_json::from_str(&output)?;
            if let Some(errors) = response.get("errors") {
                return Err(TuicrError::Forge(format!(
                    "GitHub pending review lookup failed: {errors}"
                )));
            }
            let reviews = &response["data"]["repository"]["pullRequest"]["reviews"];
            if let Some(node) = reviews["nodes"].as_array().and_then(|nodes| {
                nodes.iter().find(|node| {
                    let matches_full = node["fullDatabaseId"]
                        .as_str()
                        .and_then(|value| value.parse::<u64>().ok())
                        == Some(id);
                    let matches_db = node["databaseId"].as_u64() == Some(id);
                    matches_full || matches_db
                })
            }) {
                return node["id"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| TuicrError::Forge("GitHub review node missing ID".into()));
            }
            if reviews["pageInfo"]["hasNextPage"] != true {
                break;
            }
            cursor = reviews["pageInfo"]["endCursor"]
                .as_str()
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Err(TuicrError::Forge(
            "GitHub pending review node ID unavailable".into(),
        ))
    }
}

impl<R> GitHubGhBackend<R>
where
    R: GhCommandRunner,
{
    fn build_review_threads_args(
        &self,
        pr: &PullRequestDetails,
        cursor: Option<&str>,
    ) -> Vec<String> {
        self.build_graphql_args(pr, &build_query(cursor), cursor)
    }

    fn build_review_summaries_args(
        &self,
        pr: &PullRequestDetails,
        cursor: Option<&str>,
    ) -> Vec<String> {
        self.build_graphql_args(pr, &build_reviews_query(cursor), cursor)
    }

    fn build_review_metadata_args(
        &self,
        pr: &PullRequestDetails,
        cursor: Option<&str>,
    ) -> Vec<String> {
        self.build_graphql_args(pr, &super::review_metadata::build_query(cursor), cursor)
    }

    fn build_graphql_args(
        &self,
        pr: &PullRequestDetails,
        query: &str,
        cursor: Option<&str>,
    ) -> Vec<String> {
        let mut args = vec![
            "api".to_string(),
            "graphql".to_string(),
            "-f".to_string(),
            format!("query={query}"),
            "-F".to_string(),
            format!("owner={}", pr.repository.owner),
            "-F".to_string(),
            format!("name={}", pr.repository.name),
            "-F".to_string(),
            format!("number={}", pr.number),
        ];
        if let Some(c) = cursor {
            args.push("-F".to_string());
            args.push(format!("after={c}"));
        }
        // Enterprise host routing — `gh api graphql --hostname` picks the
        // correct endpoint when the repo lives on a non-default host.
        if pr.repository.host != "github.com" {
            args.push("--hostname".to_string());
            args.push(pr.repository.host.clone());
        }
        args
    }

    fn fetch_file_via_api(&self, request: &ForgeFileLinesRequest) -> Result<String> {
        // `gh api repos/<owner>/<repo>/contents/<path>?ref=<sha>` returns a
        // JSON object with base64-encoded `content` for text files. The
        // `Accept: application/vnd.github.raw` header skips JSON wrapping
        // and returns raw bytes, which we use here to keep parsing simple
        // and binary-safe (callers already gate binary files out).
        let path_str = request.path.to_string_lossy().replace('\\', "/");
        // Revision content is immutable, so a chd hit needs no revalidation.
        if let Some(hit) =
            crate::forge::chd_cache::get_blob(&request.repository, request.sha(), &path_str)
        {
            return Ok(hit);
        }
        let endpoint = format!(
            "repos/{}/{}/contents/{}?ref={}",
            request.repository.owner,
            request.repository.name,
            path_str,
            request.sha(),
        );
        let mut args = vec![
            "api".to_string(),
            "-H".to_string(),
            "Accept: application/vnd.github.raw".to_string(),
        ];
        if request.repository.host != "github.com" {
            args.push("--hostname".to_string());
            args.push(request.repository.host.clone());
        }
        args.push(endpoint);
        let content = self.run_gh(args, &request.repository.host)?;
        crate::forge::chd_cache::request_blob_fill(&request.repository, request.sha(), &path_str);
        Ok(content)
    }
}

const COMMITS_API_CAP: usize = 250;

/// Maps REST `pulls/N/reviews` rows to GraphQL review records.
fn parse_rest_reviews(json: &str) -> Option<Vec<crate::forge::traits::PullRequestReviewRecord>> {
    #[derive(serde::Deserialize)]
    struct Row {
        user: Option<RowUser>,
        submitted_at: Option<chrono::DateTime<chrono::Utc>>,
        commit_id: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct RowUser {
        login: Option<String>,
    }
    let rows: Vec<Row> = serde_json::from_str(json).ok()?;
    Some(
        rows.into_iter()
            .map(|r| crate::forge::traits::PullRequestReviewRecord {
                author: r.user.and_then(|u| u.login),
                submitted_at: r.submitted_at,
                commit_oid: r.commit_id,
            })
            .collect(),
    )
}

/// Returns false if the commit list reached GitHub's cap and cannot be verified.
fn commits_cacheable(len: usize) -> bool {
    len < COMMITS_API_CAP
}

type DiffBundle = (
    PullRequestDetails,
    Vec<FilePatch>,
    Vec<PullRequestCommit>,
    crate::forge::traits::PullRequestReviewMetadata,
);

/// Details carrying only validated identity; every other field is empty and
/// is always replaced by the live PR info.
fn identity_seed(
    repository: ForgeRepository,
    number: u64,
    head_sha: String,
    base_sha: String,
) -> PullRequestDetails {
    PullRequestDetails {
        repository,
        number,
        title: String::new(),
        url: String::new(),
        state: String::new(),
        is_draft: false,
        author: None,
        head_ref_name: String::new(),
        base_ref_name: String::new(),
        head_sha,
        base_sha,
        body: String::new(),
        updated_at: None,
        closed: false,
        merged_at: None,
        diff_start_sha: None,
    }
}

/// Run the live PR info fetch while the validated identity is obtained and
/// the diff/commit/metadata fetches are seeded from it. If identity is
/// unavailable, the seeded fetches fail, or the info disagrees, the bundle is
/// refetched serially against the info's identity.
fn overlapped_open(
    identity: impl FnOnce() -> Option<PullRequestDetails>,
    info: impl FnOnce() -> Result<crate::forge::traits::PullRequestInfo> + Send,
    full_info: impl FnOnce() -> Result<crate::forge::traits::PullRequestInfo> + Send,
    parts_ready: impl FnOnce(&PullRequestDetails) -> bool,
    fetch: impl Fn(PullRequestDetails) -> Result<DiffBundle> + Sync,
    complete: impl FnOnce(
        crate::forge::traits::PullRequestInfo,
    ) -> Result<crate::forge::traits::PullRequestInfo>,
) -> Result<crate::forge::pr_open::PrFetchData> {
    let (info, seed, early, full) = std::thread::scope(|scope| {
        let info = scope.spawn(|| crate::profile::time("pr.fetch_info", info));
        let seed = identity();
        // If the bundle cannot complete the info, start the full query now, not serially.
        let full = seed
            .as_ref()
            .is_none_or(|seed| !parts_ready(seed))
            .then(|| scope.spawn(|| crate::profile::time("pr.fetch_info_full", full_info)));
        let early = seed.as_ref().map(|seed| {
            crate::profile::time("pr.fetch_diff_commits_metadata_early", || {
                fetch(seed.clone())
            })
        });
        (info.join(), seed, early, full.map(|f| f.join()))
    });
    let info = match full {
        Some(full) => full.map_err(|_| TuicrError::Forge("PR info worker panicked".into()))??,
        None => {
            let info = info.map_err(|_| TuicrError::Forge("PR info worker panicked".into()))??;
            crate::profile::time("pr.complete_info", || complete(info))?
        }
    };
    reconcile_overlap(seed.as_ref(), info, early, fetch)
}

/// Combine overlapped fetches with the live PR info. The early bundle is
/// used only when the info reports the identical head and base the fetches
/// were seeded with; otherwise (or if they failed) `serial` refetches against
/// the info's identity. Info fields always come from the live info, and only
/// the refined diff base is taken from the bundle.
fn reconcile_overlap(
    seed: Option<&PullRequestDetails>,
    mut info: crate::forge::traits::PullRequestInfo,
    early: Option<Result<DiffBundle>>,
    serial: impl FnOnce(PullRequestDetails) -> Result<DiffBundle>,
) -> Result<crate::forge::pr_open::PrFetchData> {
    let same = seed.is_some_and(|seed| {
        info.details.head_sha == seed.head_sha && info.details.base_sha == seed.base_sha
    });
    let bundle = match early {
        Some(Ok(bundle)) if same => bundle,
        _ => crate::profile::time("pr.fetch_diff_commits_metadata", || {
            serial(info.details.clone())
        })?,
    };
    let (refined, patches, commits, review_metadata) = bundle;
    info.details.base_sha = refined.base_sha;
    let details = info.details.clone();
    Ok((details, patches, commits, review_metadata, info))
}

/// Identity check for a cached patch against live file metadata. Each block's
/// `diff --git` header must equal the one built from its record's paths, and its
/// extended header lines must agree with the record's status (new/deleted/rename/
/// copy). A force-push that swaps files or changes a file's status then falls
/// back to the live fetch. Paths are never parsed from patch text.
fn cached_headers_match(metadata: &[FileMetadata], patch: &str) -> bool {
    let mut blocks: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut in_hunk = false;
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            blocks.push((line, Vec::new()));
            in_hunk = false;
        } else if line.starts_with("@@") {
            in_hunk = true;
        } else if !in_hunk && let Some((_, ext)) = blocks.last_mut() {
            ext.push(line);
        }
    }
    blocks.len() == metadata.len()
        && blocks.iter().zip(metadata).all(|((header, ext), meta)| {
            let (Some(old), Some(new)) = (
                meta.old_path.as_ref().or(meta.new_path.as_ref()),
                meta.new_path.as_ref().or(meta.old_path.as_ref()),
            ) else {
                return false;
            };
            let has = |prefix: &str| ext.iter().any(|l| l.starts_with(prefix));
            let status_ok = match meta.status {
                FileStatus::Added => has("new file mode") && !has("deleted file mode"),
                FileStatus::Deleted => has("deleted file mode") && !has("new file mode"),
                FileStatus::Renamed => has("rename from"),
                FileStatus::Copied => has("copy from"),
                FileStatus::Modified => {
                    !has("new file mode")
                        && !has("deleted file mode")
                        && !has("rename from")
                        && !has("copy from")
                }
            };
            status_ok
                && **header
                    == format!(
                        "diff --git a/{} b/{}",
                        old.to_string_lossy(),
                        new.to_string_lossy()
                    )
        })
}

pub fn parse_pull_request_target(input: &str) -> Result<PullRequestTarget> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return malformed_target(input);
    }

    if let Some(target) = parse_numeric_target(trimmed) {
        return Ok(target);
    }
    if let Some(target) = parse_url_target(trimmed) {
        return Ok(target);
    }
    if let Some(target) = parse_repo_hash_target(trimmed) {
        return Ok(target);
    }

    malformed_target(input)
}

pub fn parse_github_remote_url(remote_url: &str) -> Option<ForgeRepository> {
    let trimmed = trim_url_suffix(remote_url.trim());
    if trimmed.is_empty() {
        return None;
    }

    if let Some((host, path)) = parse_scp_like_remote(trimmed) {
        let resolved = resolve_ssh_hostname(host);
        return repository_from_path(&resolved, path);
    }

    let without_scheme = strip_scheme(trimmed).unwrap_or(trimmed);
    let without_user = without_scheme
        .rsplit_once('@')
        .map(|(_, rest)| rest)
        .unwrap_or(without_scheme);
    let (host, path) = without_user.split_once('/')?;
    repository_from_path(strip_port(host), path)
}

fn parse_numeric_target(target: &str) -> Option<PullRequestTarget> {
    if !target.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let number = target.parse::<u64>().ok()?;
    if number == 0 {
        return None;
    }
    Some(PullRequestTarget::number(number, target))
}

fn parse_url_target(target: &str) -> Option<PullRequestTarget> {
    let without_scheme = strip_scheme(target)?;
    let trimmed = trim_url_suffix(without_scheme);
    let mut parts = trimmed.split('/').filter(|part| !part.is_empty());
    let host = parts.next()?;
    let owner = parts.next()?;
    let repo = parts.next()?;
    let segment = parts.next()?;
    let (number, repository) = if segment == "pull" {
        // GitHub: /<owner>/<repo>/pull/<n>
        (
            parts.next()?.parse::<u64>().ok()?,
            ForgeRepository::github(host, owner, strip_git_suffix(repo)),
        )
    } else if segment == "-" {
        // GitLab: /<owner>/<repo>/-/merge_requests/<n>
        if parts.next()? != "merge_requests" {
            return None;
        }
        (
            parts.next()?.parse::<u64>().ok()?,
            ForgeRepository::gitlab(host, owner, strip_git_suffix(repo)),
        )
    } else {
        return None;
    };
    if number == 0 {
        return None;
    }

    Some(PullRequestTarget::with_repository(
        repository, number, target,
    ))
}

fn parse_repo_hash_target(target: &str) -> Option<PullRequestTarget> {
    let (repo_part, number_part) = target.split_once('#')?;
    let number = number_part.parse::<u64>().ok()?;
    if number == 0 {
        return None;
    }
    let parts = repo_part
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let repository = match parts.as_slice() {
        [owner, repo] => {
            ForgeRepository::github(DEFAULT_GITHUB_HOST, *owner, strip_git_suffix(repo))
        }
        [host, owner, repo] => forge_repo_from_host(host, owner, strip_git_suffix(repo)),
        _ => return None,
    };

    Some(PullRequestTarget::with_repository(
        repository, number, target,
    ))
}

/// Build a `ForgeRepository` from host/owner/repo, picking the forge kind
/// based on the host name.
fn forge_repo_from_host(host: &str, owner: &str, repo: &str) -> ForgeRepository {
    if host.contains("gitlab") {
        ForgeRepository::gitlab(host, owner, repo)
    } else if crate::forge::gitea::tea::is_gitea_host(host) {
        ForgeRepository::gitea(host, owner, repo)
    } else {
        ForgeRepository::github(host, owner, repo)
    }
}

/// Resolve an SSH host alias to its real `HostName` via `~/.ssh/config`.
/// Returns the alias unchanged when the config is missing, unreadable, or
/// contains no matching block.
///
/// Limitations: only exact `Host` patterns are matched. Wildcard (`*`),
/// negation (`!`), `Match`, and `Include` directives are not supported;
/// a `Host`/`HostName` pair that depends on any of those falls back to
/// the alias unchanged.
fn resolve_ssh_hostname(alias: &str) -> String {
    let resolved = read_ssh_hostname(alias);
    normalize_ssh_transport_host(&resolved).to_string()
}

fn read_ssh_hostname(alias: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return alias.to_string();
    };
    let path = PathBuf::from(home).join(".ssh/config");
    let Ok(content) = fs::read_to_string(path) else {
        return alias.to_string();
    };
    resolve_ssh_hostname_from_config(alias, &content)
}

/// Map GitHub's documented SSH-over-HTTPS transport host back to its API host.
///
/// Users behind networks that block port 22 follow GitHub's [SSH-over-443
/// workaround](https://docs.github.com/en/authentication/troubleshooting-ssh/using-ssh-over-the-https-port)
/// and set `Hostname ssh.github.com` for `github.com` in `~/.ssh/config`. The
/// resolved hostname is correct for git transport but `ssh.github.com` is not
/// a real DNS name for API calls — only `github.com`/`api.github.com` is.
fn normalize_ssh_transport_host(host: &str) -> &str {
    if host.eq_ignore_ascii_case("ssh.github.com") {
        "github.com"
    } else {
        host
    }
}

fn resolve_ssh_hostname_from_config(alias: &str, config: &str) -> String {
    let mut in_block = false;
    for raw in config.lines() {
        // Strip inline comments and surrounding whitespace.
        let line = raw.split_once('#').map_or(raw, |(before, _)| before).trim();
        if line.is_empty() {
            continue;
        }
        // SSH config separates keyword from value by whitespace or '='.
        let (key, value) = line
            .split_once(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or((line, ""));
        let value = value
            .trim_start_matches(|c: char| c.is_whitespace() || c == '=')
            .trim();

        if key.eq_ignore_ascii_case("Host") {
            // Exact match only; wildcards and negation are intentionally unsupported.
            in_block = value.split_whitespace().any(|pat| pat == alias);
        } else if key.eq_ignore_ascii_case("Match") {
            // Match blocks aren't supported; exit any Host block we were in.
            in_block = false;
        } else if in_block && key.eq_ignore_ascii_case("HostName") {
            return value.to_string();
        }
    }
    alias.to_string()
}

fn parse_scp_like_remote(remote_url: &str) -> Option<(&str, &str)> {
    if remote_url.contains("://") {
        return None;
    }

    let (host_part, path) = remote_url.split_once(':')?;
    if host_part.contains('/') || path.is_empty() {
        return None;
    }
    let host = host_part
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(host_part);
    Some((host, path))
}

fn repository_from_path(host: &str, path: &str) -> Option<ForgeRepository> {
    let mut parts = path.split('/').filter(|part| !part.is_empty());
    let owner = parts.next()?;
    let repo = parts.next()?;
    Some(ForgeRepository::github(
        host,
        owner,
        strip_git_suffix(trim_url_suffix(repo)),
    ))
}

fn strip_scheme(value: &str) -> Option<&str> {
    value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .or_else(|| value.strip_prefix("ssh://"))
}

fn trim_url_suffix(value: &str) -> &str {
    value
        .split(['?', '#'])
        .next()
        .unwrap_or(value)
        .trim_end_matches('/')
}

/// Strip a trailing `:<port>` from a host, e.g. `example.com:2222` ->
/// `example.com`. `ssh://` remotes commonly carry a non-default SSH port
/// (GitHub Enterprise instances behind a custom port); that port is
/// meaningless for the HTTPS API host used to build `--repo` arguments, and
/// left in place it turns into a broken URL.
fn strip_port(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    }
}

fn strip_git_suffix(value: &str) -> &str {
    value.strip_suffix(".git").unwrap_or(value)
}

fn gh_repo_arg(repository: &ForgeRepository) -> String {
    if repository.host == DEFAULT_GITHUB_HOST {
        repository.slug()
    } else {
        format!("{}/{}", repository.host, repository.slug())
    }
}

fn map_gh_error(error: GhCommandError, host: &str) -> TuicrError {
    match error {
        GhCommandError::MissingGh => TuicrError::Forge(
            "GitHub integration requires `gh`.\nInstall GitHub CLI and run `gh auth login`."
                .to_string(),
        ),
        GhCommandError::Failed { stderr, .. } if looks_like_auth_failure(&stderr) => {
            TuicrError::Forge(format!(
                "GitHub authentication failed.\nRun `gh auth login` for {host}."
            ))
        }
        GhCommandError::Failed { stderr, status } => {
            let detail = if stderr.is_empty() {
                status
                    .map(|code| format!("gh exited with status {code}"))
                    .unwrap_or_else(|| "gh command failed".to_string())
            } else {
                stderr
            };
            TuicrError::Forge(format!("GitHub command failed: {detail}"))
        }
    }
}

fn looks_like_auth_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("gh auth login")
        || lower.contains("not logged in")
        || lower.contains("not logged into")
        || lower.contains("authentication failed")
        || lower.contains("requires authentication")
}

fn looks_like_permission_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("resource not accessible by integration")
        || lower.contains("must have pull request write")
        || lower.contains("http 403")
        || lower.contains("status: 403")
        || lower.contains("403 forbidden")
}

fn looks_like_pending_review_conflict(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("only have one pending review per pull request")
}

fn looks_like_unknown_commit(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("commitoid is not part of the pull request")
        || lower.contains("commit_id is not part of the pull request")
}

/// Error mapping specific to create-review. Permission failures get their
/// own message per the spec; pending-review conflicts and unknown-commit
/// 422s get actionable messages too. Other failures fall through to the
/// standard missing-gh / auth / generic mapping.
fn map_create_review_error(error: GhCommandError, host: &str) -> TuicrError {
    if let GhCommandError::Failed { stderr, .. } = &error {
        if looks_like_permission_failure(stderr) {
            return TuicrError::Forge(
                "Cannot submit review: GitHub token lacks pull request write permission."
                    .to_string(),
            );
        }
        if looks_like_pending_review_conflict(stderr) {
            return TuicrError::Forge(
                "You already have a pending review on this PR. Finish or discard it on GitHub, then try again."
                    .to_string(),
            );
        }
        if looks_like_unknown_commit(stderr) {
            return TuicrError::Forge(
                "GitHub rejected the review: the selected commit is not part of this PR (it may have been removed by a force-push). Reload with :e and try again."
                    .to_string(),
            );
        }
    }
    map_gh_error(error, host)
}

/// Parse the GitHub `pulls/<n>/reviews` response into our minimal shape.
/// GitHub returns more fields; we only extract what the App needs.
fn parse_create_review_response(output: &str) -> Result<GhCreateReviewResponse> {
    let value: serde_json::Value = serde_json::from_str(output)?;
    let id = value
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| TuicrError::Forge("Create-review response missing `id`".to_string()))?;
    let html_url = value
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let state = value
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(GhCreateReviewResponse {
        id,
        html_url,
        state,
    })
}

fn malformed_target<T>(input: &str) -> Result<T> {
    Err(TuicrError::Forge(format!(
        "Malformed GitHub pull request target: `{input}`"
    )))
}

#[cfg(test)]
pub(crate) mod tests_fixture {
    pub const SIMPLE_PATCH: &str = r##"diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
 pub fn answer() -> u32 {
-    41
+    42
 }
"##;
}

#[cfg(test)]
mod tests {
    fn seed_details(head: &str, base: &str) -> PullRequestDetails {
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
            head_sha: head.into(),
            base_sha: base.into(),
            body: String::new(),
            updated_at: None,
            closed: false,
            merged_at: None,
            diff_start_sha: None,
        }
    }

    fn bundle(base: &str, tag: &str) -> DiffBundle {
        let mut d = seed_details("h", base);
        d.title = tag.into();
        (d, Vec::new(), Vec::new(), Default::default())
    }

    fn info(head: &str, base: &str) -> crate::forge::traits::PullRequestInfo {
        crate::forge::traits::PullRequestInfo::from_details(seed_details(head, base))
    }

    #[test]
    fn overlap_kept_when_info_matches_seed_and_refines_base_only() {
        let seed = seed_details("h", "tip");
        let out = reconcile_overlap(
            Some(&seed),
            info("h", "tip"),
            Some(Ok(bundle("mb", "early"))),
            |_| panic!("no serial refetch on match"),
        )
        .unwrap();
        assert_eq!(out.0.base_sha, "mb");
        assert_eq!(out.0.title, ""); // live info fields, not the seed's
    }

    #[test]
    fn overlap_discarded_on_force_push_or_retarget() {
        let seed = seed_details("h", "tip");
        for (head, base) in [("h2", "tip"), ("h", "tip2")] {
            let out = reconcile_overlap(
                Some(&seed),
                info(head, base),
                Some(Ok(bundle("old", "early"))),
                |d| {
                    assert_eq!((d.head_sha.as_str(), d.base_sha.as_str()), (head, base));
                    Ok(bundle("fresh", "serial"))
                },
            )
            .unwrap();
            assert_eq!(out.0.base_sha, "fresh");
        }
    }

    #[test]
    fn overlapped_open_without_identity_fetches_once_serially() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let out = overlapped_open(
            || None,
            || Ok(info("h", "tip")),
            || Ok(info("h", "tip")),
            |_| true,
            |_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(bundle("mb", "serial"))
            },
            Ok,
        )
        .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(out.0.base_sha, "mb");
    }

    #[test]
    fn overlapped_open_uses_early_bundle_only_on_identity_match() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let fetch = |_| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(bundle("mb", "x"))
        };
        overlapped_open(
            || Some(seed_details("h", "tip")),
            || Ok(info("h", "tip")),
            || Ok(info("h", "tip")),
            |_| true,
            fetch,
            Ok,
        )
        .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        overlapped_open(
            || Some(seed_details("h", "tip")),
            || Ok(info("h2", "tip")),
            || Ok(info("h2", "tip")),
            |_| true,
            fetch,
            Ok,
        )
        .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[test]
    fn overlapped_open_propagates_info_error() {
        let err = overlapped_open(
            || Some(seed_details("h", "tip")),
            || Err(TuicrError::Forge("boom".into())),
            || Err(TuicrError::Forge("boom".into())),
            |_| true,
            |_| Ok(bundle("mb", "x")),
            Ok,
        );
        assert!(err.is_err());
    }

    #[test]
    fn overlap_failure_falls_back_to_serial() {
        let seed = seed_details("h", "tip");
        let out = reconcile_overlap(
            Some(&seed),
            info("h", "tip"),
            Some(Err(TuicrError::Forge("x".into()))),
            |_| Ok(bundle("fresh", "serial")),
        )
        .unwrap();
        assert_eq!(out.0.base_sha, "fresh");
    }

    /// `RefCell`-shaped, but `Sync`, so fakes satisfy `GhCommandRunner`.
    #[derive(Default)]
    pub(crate) struct TestCell<T>(std::sync::Mutex<T>);

    impl<T> TestCell<T> {
        pub(crate) fn new(value: T) -> Self {
            Self(std::sync::Mutex::new(value))
        }
        pub(crate) fn borrow(&self) -> std::sync::MutexGuard<'_, T> {
            self.0.lock().unwrap()
        }
        pub(crate) fn borrow_mut(&self) -> std::sync::MutexGuard<'_, T> {
            self.0.lock().unwrap()
        }
    }

    use super::*;
    use crate::forge::traits::ForgeBackend;

    const PR_LIST_JSON: &str = r##"
[
  {
    "number": 148,
    "title": "Add forge-backed PR review",
    "author": { "login": "alice" },
    "headRefName": "forge-review",
    "baseRefName": "main",
    "updatedAt": "2026-05-12T18:30:00Z",
    "url": "https://github.com/agavra/tuicr/pull/148",
    "state": "OPEN",
    "isDraft": false
  },
  {
    "number": 125,
    "title": "Support fetching and pushing reviews",
    "author": { "login": "YPares" },
    "headRefName": "reviews",
    "baseRefName": "main",
    "updatedAt": "2026-05-08T10:00:00Z",
    "url": "https://github.com/agavra/tuicr/pull/125",
    "state": "OPEN",
    "isDraft": true
  }
]
"##;

    const PR_VIEW_JSON: &str = r##"
{
  "number": 125,
  "title": "Support fetching and pushing reviews",
  "url": "https://github.com/agavra/tuicr/pull/125",
  "state": "OPEN",
  "isDraft": false,
  "author": { "login": "alice" },
  "headRefName": "reviews",
  "baseRefName": "main",
  "headRefOid": "abcdef1234567890",
  "baseRefOid": "1234567890abcdef",
  "body": "Review workflow",
  "updatedAt": "2026-05-12T18:30:00Z",
  "closed": false,
  "mergedAt": null
}
"##;

    const PR_PATCH: &str = r##"
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
 pub fn answer() -> u32 {
-    41
+    42
 }
"##;

    #[derive(Default)]
    struct FakeGhRunner {
        calls: TestCell<Vec<Vec<String>>>,
        /// Captured stdin payloads, paired in order with `calls` entries that
        /// went through `run_with_stdin`. Empty for plain `run` invocations.
        stdin_calls: TestCell<Vec<(Vec<String>, String)>>,
        /// When set, `run_with_stdin` returns this error instead of a stub
        /// success response. Lets tests exercise error mapping.
        stdin_error: TestCell<Option<GhCommandError>>,
        /// When set, `run_with_stdin` returns this body as the success output.
        /// Defaults to `CREATE_REVIEW_RESPONSE_JSON` when None.
        stdin_response: TestCell<Option<String>>,
    }

    impl GhCommandRunner for FakeGhRunner {
        fn run(&self, args: &[String]) -> GhCommandResult<String> {
            self.calls.borrow_mut().push(args.to_vec());
            match args.first().map(String::as_str) {
                // gh pr list/view/diff — second arg is the subcommand.
                Some("pr") => match args.get(1).map(String::as_str) {
                    Some("list") => Ok(PR_LIST_JSON.to_string()),
                    Some("view") => Ok(PR_VIEW_JSON.to_string()),
                    Some("diff") => Ok(PR_PATCH.to_string()),
                    _ => Err(GhCommandError::Failed {
                        status: Some(1),
                        stderr: "unexpected pr command".to_string(),
                    }),
                },
                // gh api graphql — dispatch by the query body so the
                // mock returns shape-appropriate fixtures for both the
                // reviewThreads and reviews queries.
                Some("api") if args.get(1).map(String::as_str) == Some("graphql") => {
                    let query = args
                        .iter()
                        .find(|a| a.starts_with("query="))
                        .map(String::as_str)
                        .unwrap_or("");
                    if query.contains("reviewThreads(") {
                        Ok(REVIEW_THREADS_JSON.to_string())
                    } else if query.contains("viewer { login }") && query.contains("commit { oid }")
                    {
                        Ok(REVIEW_METADATA_JSON.to_string())
                    } else if query.contains("reviews(") {
                        Ok(REVIEW_SUMMARIES_JSON.to_string())
                    } else {
                        Err(GhCommandError::Failed {
                            status: Some(1),
                            stderr: "unexpected graphql query".to_string(),
                        })
                    }
                }
                Some("api") if args.get(1).map(String::as_str) == Some("user") => {
                    Ok(r#"{"login":"reviewer"}"#.to_string())
                }
                Some("api")
                    if args.get(1).is_some_and(|path| {
                        path.contains("/reviews/") && path.contains("/comments")
                    }) =>
                {
                    Ok("[]".to_string())
                }
                Some("api") if args.get(1).is_some_and(|path| path.contains("/reviews?")) => {
                    Ok("[]".to_string())
                }
                // gh api repos/.../pulls/<n>/commits (commit list).
                Some("api")
                    if args
                        .iter()
                        .any(|a| a.contains("/pulls/") && a.contains("/files")) =>
                {
                    Ok(PR_FILES_JSON.to_string())
                }
                Some("api")
                    if args
                        .iter()
                        .any(|a| a.contains("/pulls/") && a.contains("/commits")) =>
                {
                    Ok(PR_COMMITS_JSON.to_string())
                }
                // gh api repos/.../compare/<base>...<head>. The range diff
                // asks for `Accept: ...diff` and gets patch text; without
                // that header the same endpoint is the merge-base lookup and
                // returns JSON.
                Some("api") if args.iter().any(|a| a.contains("/compare/")) => {
                    if args
                        .iter()
                        .any(|a| a == "Accept: application/vnd.github.diff")
                    {
                        Ok(COMPARE_DIFF.to_string())
                    } else {
                        Ok(COMPARE_JSON.to_string())
                    }
                }
                _ => Err(GhCommandError::Failed {
                    status: Some(1),
                    stderr: "unexpected command".to_string(),
                }),
            }
        }

        fn run_with_stdin(&self, args: &[String], stdin: &str) -> GhCommandResult<String> {
            self.calls.borrow_mut().push(args.to_vec());
            self.stdin_calls
                .borrow_mut()
                .push((args.to_vec(), stdin.to_string()));
            if let Some(err) = self.stdin_error.borrow().clone() {
                return Err(err);
            }
            Ok(self
                .stdin_response
                .borrow()
                .clone()
                .unwrap_or_else(|| CREATE_REVIEW_RESPONSE_JSON.to_string()))
        }
    }

    const CREATE_REVIEW_RESPONSE_JSON: &str = r##"{
        "id": 123456,
        "html_url": "https://github.com/agavra/tuicr/pull/125#pullrequestreview-123456",
        "state": "COMMENTED"
    }"##;

    const PR_COMMITS_JSON: &str = r##"[
        {
            "sha": "aaaaaaa1111111111111111111111111111aaaa",
            "commit": {
                "message": "First commit\n\nbody text",
                "author": { "name": "Alice", "email": "a@x", "date": "2026-05-10T10:00:00Z" }
            }
        },
        {
            "sha": "bbbbbbb2222222222222222222222222222bbbb",
            "commit": {
                "message": "Second commit",
                "author": { "name": "Bob", "email": "b@x", "date": "2026-05-11T10:00:00Z" }
            }
        }
    ]"##;

    const PR_FILES_JSON: &str = r##"[{"filename":"src/lib.rs","status":"modified"}]"##;

    /// `compare` without the diff Accept header. Carries both readers of this
    /// endpoint: the file metadata the range diff pairs with the patch, and
    /// the merge base. `merge_base_commit.sha` deliberately differs from the
    /// `baseaaa` tip the mock reports as `baseRefOid` — that drift is exactly
    /// what `resolve_diff_base_sha` exists to correct.
    const COMPARE_JSON: &str = r##"{
        "status": "diverged",
        "merge_base_commit": { "sha": "mergebase111" },
        "commits": [],
        "files": [{"filename":"src/lib.rs","status":"modified"}]
    }"##;

    const COMPARE_DIFF: &str = r##"diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
 pub fn answer() -> u32 {
-    41
+    42
 }
"##;

    const REVIEW_THREADS_JSON: &str = r##"{
        "data": {
            "repository": {
                "pullRequest": {
                    "reviewThreads": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [
                            {
                                "id": "PRRT_1",
                                "isResolved": false,
                                "isOutdated": false,
                                "path": "src/lib.rs",
                                "line": 42,
                                "diffSide": "RIGHT",
                                "comments": {
                                    "nodes": [
                                        {
                                            "id": "PRRC_1",
                                            "body": "remote one",
                                            "author": { "login": "alice" },
                                            "url": "https://example.com/1"
                                        }
                                    ]
                                }
                            }
                        ]
                    }
                }
            }
        }
    }"##;

    const REVIEW_SUMMARIES_JSON: &str = r##"{
        "data": {
            "repository": {
                "pullRequest": {
                    "reviews": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [
                            {
                                "id": "PRR_1",
                                "state": "COMMENTED",
                                "body": "Overall LGTM",
                                "author": { "login": "alice" },
                                "submittedAt": "2026-05-12T18:30:00Z",
                                "url": "https://example.com/r/1"
                            }
                        ]
                    }
                }
            }
        }
    }"##;

    const REVIEW_METADATA_JSON: &str = r##"{
        "data": {
            "viewer": { "login": "ronen-hoffer" },
            "repository": {
                "pullRequest": {
                    "reviews": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [
                            {
                                "author": { "login": "alice" },
                                "submittedAt": "2026-05-12T18:30:00Z",
                                "commit": { "oid": "aaaaaaa1111111111111111111111111111aaaa" }
                            },
                            {
                                "author": { "login": "ronen-hoffer" },
                                "submittedAt": "2026-05-13T09:00:00Z",
                                "commit": { "oid": "bbbbbbb2222222222222222222222222222bbbb" }
                            }
                        ]
                    }
                }
            }
        }
    }"##;

    fn repo() -> ForgeRepository {
        ForgeRepository::github("github.com", "agavra", "tuicr")
    }

    #[test]
    fn replies_to_review_thread_with_graphql_variables_and_validates_result() {
        struct ReplyRunner(TestCell<Vec<Vec<String>>>, &'static str);
        impl GhCommandRunner for ReplyRunner {
            fn run(&self, args: &[String]) -> GhCommandResult<String> {
                self.0.borrow_mut().push(args.to_vec());
                Ok(self.1.into())
            }
        }
        let success = r#"{"data":{"addPullRequestReviewThreadReply":{"comment":{"id":"PRRC_2","body":"hello","author":{"login":"me"},"replyTo":{"id":"PRRC_1"},"url":"https://example.com/reply","pullRequestReview":{"id":"PRR_1","fullDatabaseId":"123"}}}}}"#;
        let backend = GitHubGhBackend::with_runner(None, ReplyRunner(Default::default(), success));
        let reply = backend
            .reply_to_review_thread(
                &ForgeRepository::github("github.example.com", "owner", "repo"),
                "PRRT_1",
                "hello\nworld",
            )
            .unwrap();
        assert_eq!(reply.id, "PRRC_2");
        assert_eq!(reply.in_reply_to.as_deref(), Some("PRRC_1"));
        assert_eq!(reply.review_database_id.as_deref(), Some("123"));
        let args = &backend.runner.0.borrow()[0];
        assert!(
            args.iter()
                .any(|arg| arg.contains("addPullRequestReviewThreadReply"))
        );
        assert!(args.contains(&"threadId=PRRT_1".to_string()));
        assert!(args.contains(&"body=hello\nworld".to_string()));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--hostname", "github.example.com"])
        );

        for response in [
            r#"{"errors":[{"message":"forbidden"}]}"#,
            r#"{"data":{"addPullRequestReviewThreadReply":{"comment":null}}}"#,
        ] {
            let backend =
                GitHubGhBackend::with_runner(None, ReplyRunner(Default::default(), response));
            assert!(
                backend
                    .reply_to_review_thread(&repo(), "PRRT_1", "body")
                    .is_err()
            );
        }
    }

    #[test]
    fn updates_review_comment_with_stdin_variables_and_checks_confirmation() {
        struct Runner(TestCell<Vec<(Vec<String>, String)>>, &'static str);
        impl GhCommandRunner for Runner {
            fn run(&self, _: &[String]) -> GhCommandResult<String> {
                panic!("must use stdin")
            }
            fn run_with_stdin(&self, args: &[String], stdin: &str) -> GhCommandResult<String> {
                self.0.borrow_mut().push((args.to_vec(), stdin.to_string()));
                Ok(self.1.to_string())
            }
        }
        let repo = ForgeRepository::github("github.example.com", "owner", "repo");
        let runner = Runner(
            Default::default(),
            r#"{"data":{"updatePullRequestReviewComment":{"pullRequestReviewComment":{"id":"PRRC_1","body":"updated\nbody"}}}}"#,
        );
        let backend = GitHubGhBackend::with_runner(None, runner);
        backend
            .update_review_comment(&repo, "PRRC_1", "updated\nbody")
            .unwrap();
        let calls = backend.runner.0.borrow();
        let (args, stdin) = &calls[0];
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--hostname", "github.example.com"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--input", "-"]));
        assert!(!args.iter().any(|arg| arg.contains("updated")));
        let payload: serde_json::Value = serde_json::from_str(stdin).unwrap();
        assert_eq!(payload["variables"]["id"], "PRRC_1");
        assert_eq!(payload["variables"]["body"], "updated\nbody");
        drop(calls);
        let backend = GitHubGhBackend::with_runner(
            None,
            Runner(Default::default(), r#"{"errors":[{"message":"denied"}]}"#),
        );
        assert!(
            backend
                .update_review_comment(&repo, "PRRC_1", "updated")
                .is_err()
        );
        let backend = GitHubGhBackend::with_runner(
            None,
            Runner(
                Default::default(),
                r#"{"data":{"updatePullRequestReviewComment":null}}"#,
            ),
        );
        assert!(
            backend
                .update_review_comment(&repo, "PRRC_1", "updated")
                .is_err()
        );
    }

    #[test]
    fn deletes_owned_comment_and_clears_pending_review_body_via_graphql_with_host() {
        struct DeleteRunner(TestCell<Vec<Vec<String>>>);
        impl GhCommandRunner for DeleteRunner {
            fn run(&self, args: &[String]) -> GhCommandResult<String> {
                self.0.borrow_mut().push(args.to_vec());
                if args
                    .iter()
                    .any(|a| a.contains("deletePullRequestReviewComment"))
                {
                    Ok(
                        r#"{"data":{"deletePullRequestReviewComment":{"clientMutationId":null}}}"#
                            .into(),
                    )
                } else {
                    Ok(r#"{"data":{"updatePullRequestReview":{"clientMutationId":null}}}"#.into())
                }
            }
        }
        let backend = GitHubGhBackend::with_runner(None, DeleteRunner(Default::default()));
        let repo = ForgeRepository::github("github.example.com", "owner", "repo");
        backend.delete_review_comment(&repo, "PRRC_123").unwrap();
        backend.delete_review(&repo, "PRR_456").unwrap();
        let calls = backend.runner.0.borrow();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].contains(&"id=PRRC_123".to_string()));
        assert!(calls[1].contains(&"id=PRR_456".to_string()));
        assert!(
            calls[1]
                .iter()
                .any(|arg| arg.contains("updatePullRequestReview") && arg.contains("body: \"\""))
        );
        assert!(calls.iter().all(|args| {
            args.windows(2)
                .any(|pair| pair == ["--hostname", "github.example.com"])
        }));
    }

    #[test]
    fn toggles_reaction_with_graphql_and_validates_subject() {
        struct ReactionRunner(TestCell<Vec<Vec<String>>>);
        impl GhCommandRunner for ReactionRunner {
            fn run(&self, args: &[String]) -> GhCommandResult<String> {
                self.0.borrow_mut().push(args.to_vec());
                let field = if args.iter().any(|arg| arg.contains("removeReaction(input")) {
                    "removeReaction"
                } else {
                    "addReaction"
                };
                Ok(
                    serde_json::json!({ "data": { field: { "subject": { "id": "PRRC_1" } } } })
                        .to_string(),
                )
            }
        }
        let backend = GitHubGhBackend::with_runner(None, ReactionRunner(Default::default()));
        let repo = ForgeRepository::github("github.example.com", "owner", "repo");
        backend
            .toggle_comment_reaction(&repo, "PRRC_1", "HEART", false)
            .unwrap();
        backend
            .toggle_comment_reaction(&repo, "PRRC_1", "HEART", true)
            .unwrap();
        let calls = backend.runner.0.borrow();
        assert_eq!(calls.len(), 2);
        for call in calls.iter() {
            assert!(call.contains(&"id=PRRC_1".to_string()));
            assert!(call.contains(&"content=HEART".to_string()));
            assert!(
                call.windows(2)
                    .any(|pair| pair == ["--hostname", "github.example.com"])
            );
        }
        struct ErrorRunner;
        impl GhCommandRunner for ErrorRunner {
            fn run(&self, _: &[String]) -> GhCommandResult<String> {
                Ok(r#"{"data":{"addReaction":{"subject":{"id":"wrong"}}}}"#.into())
            }
        }
        assert!(
            GitHubGhBackend::with_runner(None, ErrorRunner)
                .toggle_comment_reaction(&repo, "PRRC_1", "HEART", false)
                .is_err()
        );
        struct GraphQlErrorsRunner;
        impl GhCommandRunner for GraphQlErrorsRunner {
            fn run(&self, _: &[String]) -> GhCommandResult<String> {
                Ok(
                    r#"{"errors":[{"message":"Could not resolve to a node with the global id"}]}"#
                        .into(),
                )
            }
        }
        let err = GitHubGhBackend::with_runner(None, GraphQlErrorsRunner)
            .toggle_comment_reaction(&repo, "PRRC_1", "HEART", false)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Could not resolve to a node with the global id")
        );
    }

    #[test]
    fn toggles_thread_resolution_with_validated_graphql_response_and_enterprise_host() {
        struct ThreadRunner(TestCell<Vec<Vec<String>>>);
        impl GhCommandRunner for ThreadRunner {
            fn run(&self, args: &[String]) -> GhCommandResult<String> {
                self.0.borrow_mut().push(args.to_vec());
                let resolved = args
                    .iter()
                    .any(|arg| arg.contains("{ resolveReviewThread("));
                Ok(serde_json::json!({
                    "data": { (if resolved { "resolveReviewThread" } else { "unresolveReviewThread" }): {
                        "thread": { "id": "PRRT_1", "isResolved": resolved }
                    }}
                }).to_string())
            }
        }
        let backend = GitHubGhBackend::with_runner(None, ThreadRunner(Default::default()));
        let repo = ForgeRepository::github("github.example.com", "owner", "repo");
        backend
            .set_review_thread_resolved(&repo, "PRRT_1", true)
            .unwrap();
        backend
            .set_review_thread_resolved(&repo, "PRRT_1", false)
            .unwrap();
        let calls = backend.runner.0.borrow();
        assert_eq!(calls.len(), 2);
        for args in calls.iter() {
            assert!(args.contains(&"id=PRRT_1".to_string()));
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--hostname", "github.example.com"])
            );
            assert!(args.iter().any(|arg| arg.contains("threadId: $id")));
        }
    }

    #[test]
    fn thread_resolution_rejects_errors_and_unconfirmed_results() {
        struct ErrorRunner(&'static str);
        impl GhCommandRunner for ErrorRunner {
            fn run(&self, _: &[String]) -> GhCommandResult<String> {
                Ok(self.0.into())
            }
        }
        for response in [
            r#"{"errors":[{"message":"forbidden"}],"data":null}"#,
            r#"{"data":{"resolveReviewThread":null}}"#,
            r#"{"data":{"resolveReviewThread":{"thread":{"id":"wrong","isResolved":true}}}}"#,
            r#"{"data":{"resolveReviewThread":{"thread":{"id":"PRRT_1","isResolved":false}}}}"#,
        ] {
            let backend = GitHubGhBackend::with_runner(None, ErrorRunner(response));
            assert!(
                backend
                    .set_review_thread_resolved(&repo(), "PRRT_1", true)
                    .is_err()
            );
        }
    }

    #[test]
    fn deletion_rejects_graphql_errors() {
        struct ErrorRunner;
        impl GhCommandRunner for ErrorRunner {
            fn run(&self, _: &[String]) -> GhCommandResult<String> {
                Ok(r#"{"errors":[{"message":"forbidden"}],"data":null}"#.into())
            }
        }
        let backend = GitHubGhBackend::with_runner(None, ErrorRunner);
        assert!(backend.delete_review_comment(&repo(), "id").is_err());
        assert!(backend.delete_review(&repo(), "id").is_err());
    }

    #[test]
    fn repository_display_name_omits_github_com() {
        assert_eq!(repo().display_name(), "agavra/tuicr");
        assert_eq!(
            ForgeRepository::github("github.example.com", "agavra", "tuicr").display_name(),
            "github.example.com/agavra/tuicr"
        );
    }

    #[test]
    fn constructs_system_runner_backend() {
        let _backend = GitHubGhBackend::new(Some(repo()));
    }

    #[test]
    fn parses_numeric_pull_request_target() {
        let target = parse_pull_request_target("125").unwrap();
        assert_eq!(target.number, 125);
        assert_eq!(target.repository, None);
    }

    #[test]
    fn parses_owner_repo_pull_request_target() {
        let target = parse_pull_request_target("agavra/tuicr#125").unwrap();
        assert_eq!(target.number, 125);
        assert_eq!(target.repository.unwrap(), repo());
    }

    #[test]
    fn parses_enterprise_owner_repo_pull_request_target() {
        let target = parse_pull_request_target("github.example.com/agavra/tuicr#125").unwrap();
        let repository = target.repository.unwrap();
        assert_eq!(repository.host, "github.example.com");
        assert_eq!(repository.slug(), "agavra/tuicr");
    }

    #[test]
    fn parses_full_pull_request_url() {
        let target = parse_pull_request_target("https://github.com/agavra/tuicr/pull/125").unwrap();
        assert_eq!(target.number, 125);
        assert_eq!(target.repository.unwrap(), repo());
    }

    #[test]
    fn parses_enterprise_pull_url_as_github_even_when_host_contains_gitlab() {
        let target =
            parse_pull_request_target("https://gitlab.ghe.company.com/agavra/tuicr/pull/125")
                .unwrap();
        let repository = target.repository.unwrap();
        assert_eq!(target.number, 125);
        assert_eq!(repository.kind, crate::forge::traits::ForgeKind::GitHub);
        assert_eq!(repository.host, "gitlab.ghe.company.com");
        assert_eq!(repository.slug(), "agavra/tuicr");
    }

    #[test]
    fn rejects_malformed_pull_request_target() {
        let err = parse_pull_request_target("agavra/tuicr/pulls/125").unwrap_err();
        assert!(
            err.to_string()
                .contains("Malformed GitHub pull request target")
        );
    }

    #[test]
    fn parses_https_remote_url() {
        let repository = parse_github_remote_url("https://github.com/agavra/tuicr.git").unwrap();
        assert_eq!(repository, repo());
    }

    #[test]
    fn parses_scp_like_ssh_remote_url() {
        let repository = parse_github_remote_url("git@github.com:agavra/tuicr.git").unwrap();
        assert_eq!(repository, repo());
    }

    #[test]
    fn parses_ssh_remote_url() {
        let repository =
            parse_github_remote_url("ssh://git@github.example.com/agavra/tuicr.git").unwrap();
        assert_eq!(repository.host, "github.example.com");
        assert_eq!(repository.slug(), "agavra/tuicr");
    }

    #[test]
    fn parses_ssh_remote_url_with_custom_port() {
        // GitHub Enterprise instances behind a non-default SSH port must not
        // leak that port into the ForgeRepository host; it breaks `--repo`
        // URL construction against the HTTPS API.
        let repository =
            parse_github_remote_url("ssh://git@github.example.com:2222/agavra/tuicr.git").unwrap();
        assert_eq!(repository.host, "github.example.com");
        assert_eq!(repository.slug(), "agavra/tuicr");
    }

    #[test]
    fn resolve_ssh_hostname_resolves_alias_to_configured_hostname() {
        // (case name, alias, config, expected hostname)
        let cases: &[(&str, &str, &str, &str)] = &[
            (
                "basic alias",
                "github-work",
                "Host github-work\n    HostName github.com\n",
                "github.com",
            ),
            (
                "tab separator",
                "github-work",
                "Host\tgithub-work\n\tHostName\tgithub.com\n",
                "github.com",
            ),
            (
                "equals separator",
                "github-work",
                "Host=github-work\n    HostName=github.com\n",
                "github.com",
            ),
            (
                "case-insensitive keywords",
                "github-work",
                "host github-work\n    hostname github.com\n",
                "github.com",
            ),
            (
                "trailing comment stripped",
                "github-work",
                "Host github-work\n    HostName github.com # inline comment\n",
                "github.com",
            ),
            (
                "blank and comment lines ignored",
                "github-work",
                "\
# leading comment

Host github-work
    # nested comment
    HostName github.com
",
                "github.com",
            ),
            (
                "multi-pattern block: second pattern",
                "github-work",
                "\
Host github.com github-personal
    HostName github.com

Host github-work
    HostName github.mycompany.com
",
                "github.mycompany.com",
            ),
            (
                "multi-pattern block: first pattern",
                "github-personal",
                "\
Host github.com github-personal
    HostName github.com

Host github-work
    HostName github.mycompany.com
",
                "github.com",
            ),
            (
                "duplicate Host blocks: first wins",
                "github-work",
                "\
Host github-work
    HostName first.example.com

Host github-work
    HostName second.example.com
",
                "first.example.com",
            ),
        ];

        for (name, alias, config, expected) in cases {
            assert_eq!(
                resolve_ssh_hostname_from_config(alias, config),
                *expected,
                "case: {name}",
            );
        }
    }

    #[test]
    fn resolve_ssh_hostname_falls_back_to_alias_when_unresolved() {
        // (case name, alias, config). All cases expect the alias back unchanged.
        let cases: &[(&str, &str, &str)] = &[
            (
                "host not in config",
                "github-work",
                "Host github.com\n    HostName github.com\n",
            ),
            (
                "block has no HostName",
                "github-work",
                "Host github-work\n    User git\n    IdentitiesOnly yes\n",
            ),
            (
                "wildcard patterns unsupported",
                "foo.github.com",
                "\
Host *.github.com
    HostName fallback.example.com

Host *
    HostName catchall.example.com
",
            ),
            (
                "Match directive terminates Host block",
                "github-work",
                "\
Host github-work
Match host github-work
    HostName should-not-leak.example.com
",
            ),
        ];

        for (name, alias, config) in cases {
            assert_eq!(
                resolve_ssh_hostname_from_config(alias, config),
                *alias,
                "case: {name}",
            );
        }
    }

    #[test]
    fn normalizes_github_ssh_over_443_transport_host() {
        // `ssh.github.com` is GitHub's documented SSH-over-HTTPS-port transport
        // host. After resolving ~/.ssh/config it must map back to github.com,
        // otherwise downstream API calls hit the nonexistent api.ssh.github.com.
        assert_eq!(normalize_ssh_transport_host("ssh.github.com"), "github.com");
        assert_eq!(normalize_ssh_transport_host("SSH.GitHub.com"), "github.com");
        assert_eq!(normalize_ssh_transport_host("github.com"), "github.com");
        assert_eq!(
            normalize_ssh_transport_host("github.example.com"),
            "github.example.com"
        );
    }

    #[test]
    fn parses_scp_url_with_ssh_over_443_hostname_rewrite() {
        // ~/.ssh/config rewrites github.com -> ssh.github.com:443 for transport.
        // The forge repository must still report github.com so API calls work.
        let config = "Host github.com\n    HostName ssh.github.com\n    Port 443\n";
        let url = "git@github.com:agavra/tuicr.git";
        let trimmed = trim_url_suffix(url.trim());
        let (host, path) = parse_scp_like_remote(trimmed).unwrap();
        let resolved =
            normalize_ssh_transport_host(&resolve_ssh_hostname_from_config(host, config))
                .to_string();
        let repo = repository_from_path(&resolved, path).unwrap();
        assert_eq!(repo.host, "github.com");
        assert_eq!(repo.slug(), "agavra/tuicr");
    }

    #[test]
    fn parse_scp_url_with_ssh_alias_resolves_via_config() {
        // End-to-end check on the SCP-style path: alias in the URL gets
        // swapped for the configured HostName before repository construction.
        let config = "Host github-work\n    HostName github.com\n";
        let url = "git@github-work:example-org/example-repo.git";
        let trimmed = trim_url_suffix(url.trim());
        let (host, path) = parse_scp_like_remote(trimmed).unwrap();
        let resolved = resolve_ssh_hostname_from_config(host, config);
        let repo = repository_from_path(&resolved, path).unwrap();
        assert_eq!(repo.host, "github.com");
        assert_eq!(repo.slug(), "example-org/example-repo");
    }

    #[test]
    fn list_pull_requests_uses_gh_json_output() {
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(None, runner);
        let result = backend
            .list_pull_requests(PullRequestListQuery::first_page(repo(), 1))
            .unwrap();

        assert_eq!(result.pull_requests.len(), 1);
        assert!(result.has_more);
        assert_eq!(result.total_loaded, 1);
        assert_eq!(result.pull_requests[0].number, 148);
        assert_eq!(result.pull_requests[0].author.as_deref(), Some("alice"));

        let calls = backend.runner.calls.borrow();
        assert_eq!(
            calls[0],
            vec![
                "pr",
                "list",
                "--repo",
                "agavra/tuicr",
                "--state",
                "open",
                "--limit",
                "2",
                "--json",
                PR_LIST_JSON_FIELDS,
            ]
        );
    }

    #[test]
    fn list_pull_requests_can_filter_to_review_requested() {
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(None, runner);
        backend
            .list_pull_requests(PullRequestListQuery::first_page_with_scope(
                repo(),
                1,
                PullRequestListScope::ReviewRequested,
            ))
            .unwrap();

        let calls = backend.runner.calls.borrow();
        assert_eq!(
            calls[0],
            vec![
                "pr",
                "list",
                "--repo",
                "agavra/tuicr",
                "--state",
                "open",
                "--limit",
                "2",
                "--json",
                PR_LIST_JSON_FIELDS,
                "--search",
                "review-requested:@me",
            ]
        );
    }

    #[test]
    fn list_pull_requests_can_load_next_slice() {
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(None, runner);
        let result = backend
            .list_pull_requests(PullRequestListQuery {
                repository: repo(),
                already_loaded: 1,
                page_size: 1,
                scope: PullRequestListScope::Open,
            })
            .unwrap();

        assert_eq!(result.pull_requests.len(), 1);
        assert!(!result.has_more);
        assert_eq!(result.total_loaded, 2);
        assert_eq!(result.pull_requests[0].number, 125);

        let calls = backend.runner.calls.borrow();
        assert_eq!(
            calls[0],
            vec![
                "pr",
                "list",
                "--repo",
                "agavra/tuicr",
                "--state",
                "open",
                "--limit",
                "3",
                "--json",
                PR_LIST_JSON_FIELDS,
            ]
        );
    }

    #[test]
    fn list_pull_requests_uses_host_qualified_repo_for_enterprise() {
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(None, runner);
        let repository = ForgeRepository::github("github.example.com", "agavra", "tuicr");
        backend
            .list_pull_requests(PullRequestListQuery::first_page(repository, 1))
            .unwrap();

        let calls = backend.runner.calls.borrow();
        assert_eq!(calls[0][3], "github.example.com/agavra/tuicr");
    }

    #[test]
    fn get_pull_request_uses_default_repository_for_numeric_target() {
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();

        assert_eq!(details.number, 125);
        assert_eq!(details.head_sha, "abcdef1234567890");
        assert!(!details.is_read_only());
    }

    fn stash_info_bundle(open_json: &'static str) {
        use crate::forge::chd_cache::{install_fake_chd, open_bundle};
        install_fake_chd(Some(std::rc::Rc::new(move |_| Some(open_json.to_string()))));
        assert!(
            open_bundle(
                &ForgeRepository::github("github.com", "o", "r"),
                1,
                crate::forge::chd_cache::InfoSelection::default()
            )
            .is_some()
        );
        // The fake stays installed (installing resets the stash); each test thread owns it.
    }

    const FULL_OPEN: &str = r#"{"head":"h","base":"tip","viewer":"me",
        "check_runs":[{"name":"build","status":"completed","conclusion":"success","details_url":"u"}],
        "statuses":[{"context":"ci","state":"pending","target_url":""}],
        "issue_comments":[{"user":{"login":"bot[bot]"},"body":"hello"}],
        "requested":{"users":[{"login":"alice"}],"teams":[]}}"#;

    #[test]
    fn complete_info_uses_validated_rest_parts_without_a_live_query() {
        stash_info_bundle(FULL_OPEN);
        let backend = GitHubGhBackend::with_runner(Some(repo()), FakeGhRunner::default())
            .with_pr_checks(true);
        let out = backend
            .complete_pull_request_info(parse_pull_request_target("1").unwrap(), info("h", "tip"))
            .unwrap();
        assert!(backend.runner.calls.borrow().is_empty());
        assert_eq!(out.requested_reviewers, ["alice"]);
        assert_eq!(out.checks.len(), 2);
        assert_eq!(out.issue_comments[0].author.as_deref(), Some("bot"));
    }

    #[test]
    fn complete_info_goes_live_when_a_part_is_missing_or_identity_differs() {
        let missing = r#"{"head":"h","base":"tip","requested":{"users":[],"teams":[]},
            "issue_comments":[],"statuses":[]}"#;
        for (open, head) in [(missing, "h"), (FULL_OPEN, "moved")] {
            stash_info_bundle(open);
            let backend = GitHubGhBackend::with_runner(Some(repo()), FakeGhRunner::default())
                .with_pr_checks(true);
            backend
                .complete_pull_request_info(
                    parse_pull_request_target("1").unwrap(),
                    info(head, "tip"),
                )
                .unwrap();
            let calls = backend.runner.calls.borrow();
            assert_eq!(calls.len(), 1, "one full live info query");
            assert_eq!(
                calls[0].last().map(String::as_str),
                Some(PR_INFO_JSON_FIELDS)
            );
        }
    }

    #[test]
    fn complete_info_never_reuses_parts_across_operations() {
        stash_info_bundle(FULL_OPEN);
        let backend = GitHubGhBackend::with_runner(Some(repo()), FakeGhRunner::default())
            .with_pr_checks(true);
        let target = || parse_pull_request_target("1").unwrap();
        backend
            .complete_pull_request_info(target(), info("h", "tip"))
            .unwrap();
        // Parts are consumed once; a second completion must fetch live.
        backend
            .complete_pull_request_info(target(), info("h", "tip"))
            .unwrap();
        assert_eq!(backend.runner.calls.borrow().len(), 1);
    }

    #[test]
    fn disabled_check_rollup_does_not_force_a_live_query_for_missing_checks() {
        stash_info_bundle(
            r#"{"head":"h","base":"tip","issue_comments":[],
            "requested":{"users":[],"teams":[]}}"#,
        );
        let backend = GitHubGhBackend::with_runner(Some(repo()), FakeGhRunner::default());
        assert!(!backend.show_pr_checks);
        assert!(backend.info_parts_ready(&seed_details("h", "tip")));
        backend
            .complete_pull_request_info(parse_pull_request_target("1").unwrap(), info("h", "tip"))
            .unwrap();
        assert!(backend.runner.calls.borrow().is_empty());
    }

    #[test]
    fn stashed_viewer_skips_the_live_user_lookup() {
        stash_info_bundle(r#"{"head":"h","base":"tip","viewer":"me","reviews":[]}"#);
        let backend = GitHubGhBackend::with_runner(Some(repo()), FakeGhRunner::default());
        let meta = backend
            .review_metadata_from_validated_pages(&seed_details("h", "tip"))
            .expect("validated reviews and viewer");
        assert_eq!(meta.viewer_login.as_deref(), Some("me"));
        assert!(backend.runner.calls.borrow().is_empty());
    }

    #[test]
    fn unready_bundle_starts_the_full_info_query_without_waiting_for_the_residual() {
        let full_calls = std::sync::atomic::AtomicUsize::new(0);
        let out = overlapped_open(
            || Some(seed_details("h", "tip")),
            || Err(TuicrError::Forge("residual not needed".into())),
            || {
                full_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(info("h", "tip"))
            },
            |_| false,
            |_| Ok(bundle("mb", "x")),
            |_| panic!("complete must not run when the full query supplied the info"),
        )
        .unwrap();
        assert_eq!(full_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(out.0.base_sha, "mb");
    }

    #[test]
    fn can_skip_pull_request_check_rollup() {
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner).with_pr_checks(false);

        let info = backend
            .get_pull_request_info(parse_pull_request_target("125").unwrap())
            .unwrap();

        assert!(info.checks.is_empty());
        let calls = backend.runner.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].last().map(String::as_str),
            Some(PR_INFO_JSON_FIELDS_WITHOUT_CHECKS)
        );
        assert!(
            !calls[0]
                .last()
                .expect("expected --json field list")
                .contains("statusCheckRollup")
        );
    }

    #[test]
    fn can_skip_pull_request_conversation_comments() {
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner)
            .with_pr_checks(true)
            .with_pr_comments(false);

        let info = backend
            .get_pull_request_info(parse_pull_request_target("125").unwrap())
            .unwrap();

        assert!(info.issue_comments.is_empty());
        let calls = backend.runner.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].last().map(String::as_str),
            Some(PR_INFO_JSON_FIELDS_WITHOUT_COMMENTS)
        );
        assert!(
            !calls[0]
                .last()
                .expect("expected --json field list")
                .split(',')
                .any(|field| field == "comments")
        );
    }

    #[test]
    fn get_pull_request_requires_repository() {
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(None, runner);
        let err = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("does not include a repository"));
    }

    #[test]
    fn get_pull_request_diff_pairs_metadata_with_patch_text() {
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        let patches = backend.get_pull_request_diff(&details).unwrap();

        assert_eq!(patches.len(), 1);
        assert_eq!(
            patches[0].new_path.as_deref(),
            Some(Path::new("src/lib.rs"))
        );
        assert_eq!(patches[0].patch, PR_PATCH.trim_start());
    }

    #[test]
    fn parallel_fetch_returns_same_parts_as_serial_calls() {
        let backend = GitHubGhBackend::with_runner(Some(repo()), FakeGhRunner::default());
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        let (out, patches, commits, meta) = backend
            .fetch_diff_commits_metadata(details.clone())
            .unwrap();

        let mut serial = details.clone();
        if let Some(base) = backend.resolve_diff_base_sha(&serial) {
            serial.base_sha = base;
        }
        assert_eq!(out.head_sha, serial.head_sha);
        assert_eq!(out.base_sha, serial.base_sha);
        assert_eq!(patches, backend.get_pull_request_diff(&serial).unwrap());
        assert_eq!(
            commits,
            backend
                .list_pull_request_commits(&serial)
                .unwrap_or_default()
        );
        assert_eq!(
            meta.viewer_login,
            backend
                .list_pull_request_review_metadata(&serial)
                .unwrap_or_default()
                .viewer_login
        );
    }

    #[test]
    fn cached_patch_with_swapped_files_is_rejected() {
        let meta = |p: &str| FileMetadata {
            old_path: Some(PathBuf::from(p)),
            new_path: Some(PathBuf::from(p)),
            status: crate::model::FileStatus::Modified,
        };
        let patch =
            "diff --git a/old.rs b/old.rs\n--- a/old.rs\n+++ b/old.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(cached_headers_match(&[meta("old.rs")], patch));
        assert!(!cached_headers_match(&[meta("new.rs")], patch));
        let deleted = "diff --git a/old.rs b/old.rs\ndeleted file mode 100644\n@@ -1 +0,0 @@\n-a\n";
        assert!(!cached_headers_match(&[meta("old.rs")], deleted));
        let mut gone = meta("old.rs");
        gone.status = crate::model::FileStatus::Deleted;
        assert!(cached_headers_match(&[gone.clone()], deleted));
        assert!(!cached_headers_match(&[gone], patch));
        let nested = "diff --git a/src/foo.rs b/src/foo.rs\n";
        assert!(!cached_headers_match(&[meta("foo.rs")], nested));
        assert!(!cached_headers_match(
            &[meta("old.rs"), meta("x.rs")],
            patch
        ));
    }

    #[test]
    fn should_not_pass_patch_flag_to_gh_pr_diff() {
        // Regression: `gh pr diff --patch` returns mbox-style per-commit
        // patches concatenated, which the diff parser turns into duplicate
        // DiffFile entries (one per commit-touching-the-same-file). We
        // want the cumulative diff. Lock the argv so this can't silently
        // regress.
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        let _ = backend.get_pull_request_diff(&details).unwrap();

        let calls = backend.runner.calls.borrow();
        let diff_call = calls
            .iter()
            .find(|args| {
                args.first().map(String::as_str) == Some("pr")
                    && args.get(1).map(String::as_str) == Some("diff")
            })
            .expect("expected a `gh pr diff` call");
        assert!(
            !diff_call.iter().any(|a| a == "--patch"),
            "`gh pr diff` must NOT pass --patch (mbox output duplicates files); got {diff_call:?}"
        );
    }

    #[test]
    fn should_list_review_threads_via_graphql_api_call() {
        // given
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        // when
        let threads = backend.list_review_threads(&details).unwrap();
        // then
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, "PRRT_1");
        assert_eq!(threads[0].path, "src/lib.rs");
        // and — the API call was placed against `gh api graphql` with the
        // owner/name/number parameters.
        let calls = backend.runner.calls.borrow();
        let graphql_call = calls
            .iter()
            .find(|args| args.first().map(String::as_str) == Some("api"))
            .expect("expected a gh api call");
        assert_eq!(graphql_call[1], "graphql");
        assert!(graphql_call.iter().any(|a| a == "owner=agavra"));
        assert!(graphql_call.iter().any(|a| a == "name=tuicr"));
        assert!(graphql_call.iter().any(|a| a == "number=125"));
    }

    #[test]
    fn should_list_review_summaries_via_graphql_api_call() {
        // given
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        // when
        let summaries = backend.list_review_summaries(&details).unwrap();
        // then — the COMMENTED review with body is surfaced.
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "PRR_1");
        assert_eq!(summaries[0].author.as_deref(), Some("alice"));
        assert_eq!(summaries[0].body, "Overall LGTM");
        // and — the call routes the `reviews(` query, not `reviewThreads(`.
        let calls = backend.runner.calls.borrow();
        let summaries_call = calls
            .iter()
            .find(|args| {
                args.first().map(String::as_str) == Some("api")
                    && args.iter().any(|a| a.contains("reviews(first:"))
            })
            .expect("expected a reviews graphql call");
        assert!(summaries_call.iter().any(|a| a == "owner=agavra"));
        assert!(summaries_call.iter().any(|a| a == "number=125"));
    }

    type ChdCalls = std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>>;

    fn fake_chd(answer: impl Fn(&[String]) -> Option<String> + 'static) -> ChdCalls {
        let calls: ChdCalls = Default::default();
        let log = calls.clone();
        crate::forge::chd_cache::install_fake_chd(Some(std::rc::Rc::new(move |args| {
            log.borrow_mut().push(args.to_vec());
            answer(args)
        })));
        calls
    }

    fn backend_and_details() -> (GitHubGhBackend<FakeGhRunner>, PullRequestDetails) {
        let backend = GitHubGhBackend::with_runner(Some(repo()), FakeGhRunner::default());
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        backend.runner.calls.borrow_mut().clear();
        (backend, details)
    }

    fn ran_gh(backend: &GitHubGhBackend<FakeGhRunner>, needle: &str) -> bool {
        backend
            .runner
            .calls
            .borrow()
            .iter()
            .any(|a| a.iter().any(|x| x.contains(needle)))
    }

    #[test]
    fn commits_use_cached_list_and_live_miss_requests_fill() {
        let calls = fake_chd(|args| {
            (args.get(1).map(String::as_str) == Some("get-list"))
                .then(|| PR_COMMITS_JSON.to_string())
        });
        let (backend, details) = backend_and_details();
        let hit = backend.list_pull_request_commits(&details).unwrap();
        assert!(!hit.is_empty());
        assert!(!ran_gh(&backend, "/commits"));
        crate::forge::chd_cache::install_fake_chd(None);
        drop(calls);

        let calls = fake_chd(|_| None);
        let live = backend.list_pull_request_commits(&details).unwrap();
        crate::forge::chd_cache::install_fake_chd(None);
        assert_eq!(live.len(), hit.len());
        assert!(ran_gh(&backend, "/commits"));
        assert!(
            calls
                .borrow()
                .iter()
                .any(|a| a.get(1).map(String::as_str) == Some("fill-list"))
        );
    }

    #[test]
    fn oversized_commit_lists_are_not_cacheable() {
        assert!(commits_cacheable(249));
        assert!(!commits_cacheable(250));
    }

    fn bundle_json(extra: &str) -> String {
        format!(
            r#"{{"head":"{}","base":"{}"{extra}}}"#,
            "a".repeat(40),
            "b".repeat(40)
        )
    }

    fn open_with(extra: String) -> (GitHubGhBackend<FakeGhRunner>, PullRequestDetails) {
        fake_chd(move |args| {
            (args.get(1).map(String::as_str) == Some("open")).then(|| bundle_json(&extra))
        });
        crate::forge::chd_cache::open_bundle(
            &repo(),
            125,
            crate::forge::chd_cache::InfoSelection::default(),
        )
        .expect("bundle");
        let (backend, mut details) = backend_and_details();
        details.head_sha = "a".repeat(40);
        details.base_sha = "b".repeat(40);
        (backend, details)
    }

    #[test]
    fn files_come_from_validated_bundle_without_a_live_call() {
        let (backend, details) = open_with(format!(r#","files":{}"#, PR_FILES_JSON));
        let metadata = backend.pull_request_file_metadata(&details).unwrap();
        crate::forge::chd_cache::install_fake_chd(None);
        assert!(!metadata.is_empty());
        assert!(!ran_gh(&backend, "/files"));
    }

    #[test]
    fn files_go_live_when_bundle_lacks_them_or_identity_moved() {
        let (backend, details) = open_with(String::new());
        assert!(
            !backend
                .pull_request_file_metadata(&details)
                .unwrap()
                .is_empty()
        );
        assert!(ran_gh(&backend, "/files"));
        backend.runner.calls.borrow_mut().clear();
        let (backend2, mut moved) = open_with(format!(r#","files":{}"#, PR_FILES_JSON));
        moved.head_sha = "f".repeat(40);
        assert!(
            !backend2
                .pull_request_file_metadata(&moved)
                .unwrap()
                .is_empty()
        );
        assert!(ran_gh(&backend2, "/files"));
        crate::forge::chd_cache::install_fake_chd(None);
    }

    #[test]
    fn review_metadata_uses_validated_bundle_reviews_and_rest_viewer() {
        let (backend, details) = open_with(
            r#","reviews":[{"user":{"login":"alice"},"submitted_at":"2026-01-02T03:04:05Z","commit_id":"abc"},{"user":null,"submitted_at":null,"commit_id":null}]"#
                .to_string(),
        );
        let metadata = backend.list_pull_request_review_metadata(&details).unwrap();
        crate::forge::chd_cache::install_fake_chd(None);
        assert_eq!(metadata.viewer_login.as_deref(), Some("reviewer"));
        assert_eq!(metadata.reviews.len(), 2);
        assert_eq!(metadata.reviews[0].author.as_deref(), Some("alice"));
        assert_eq!(metadata.reviews[0].commit_oid.as_deref(), Some("abc"));
        assert!(metadata.reviews[1].author.is_none());
        assert!(!ran_gh(&backend, "graphql"));
    }

    #[test]
    fn review_metadata_falls_back_to_graphql_without_validated_reviews() {
        let (backend, details) = open_with(String::new());
        let metadata = backend.list_pull_request_review_metadata(&details).unwrap();
        crate::forge::chd_cache::install_fake_chd(None);
        assert_eq!(metadata.viewer_login.as_deref(), Some("ronen-hoffer"));
        assert!(ran_gh(&backend, "graphql"));
    }

    #[test]
    fn merge_base_and_commits_come_from_the_bundle_by_oids() {
        let want = "c".repeat(40);
        let (backend, details) = open_with(format!(
            r#","merge_base":"{want}","commits":{}"#,
            PR_COMMITS_JSON
        ));
        let got = backend.resolve_diff_base_sha(&details);
        let commits = backend.list_pull_request_commits(&details).unwrap();
        crate::forge::chd_cache::install_fake_chd(None);
        assert_eq!(got, Some(want));
        assert!(!commits.is_empty());
        assert!(!ran_gh(&backend, "/compare/"));
        assert!(!ran_gh(&backend, "/commits"));
    }

    #[test]
    fn should_list_pull_request_review_metadata_via_graphql_api_call() {
        // given
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        // when
        let metadata = backend.list_pull_request_review_metadata(&details).unwrap();
        // then
        assert_eq!(metadata.viewer_login.as_deref(), Some("ronen-hoffer"));
        assert_eq!(metadata.reviews.len(), 2);
        assert_eq!(metadata.reviews[1].author.as_deref(), Some("ronen-hoffer"));
        assert_eq!(
            metadata.reviews[1].commit_oid.as_deref(),
            Some("bbbbbbb2222222222222222222222222222bbbb")
        );
        // and — the query requests viewer login and review commit oids.
        let calls = backend.runner.calls.borrow();
        let metadata_call = calls
            .iter()
            .find(|args| {
                args.first().map(String::as_str) == Some("api")
                    && args.iter().any(|a| a.contains("viewer { login }"))
                    && args.iter().any(|a| a.contains("commit { oid }"))
            })
            .expect("expected review metadata graphql call");
        assert!(metadata_call.iter().any(|a| a == "owner=agavra"));
        assert!(metadata_call.iter().any(|a| a == "number=125"));
    }

    #[test]
    fn should_list_pull_request_commits_via_api_pagination_endpoint() {
        // given
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        // when
        let commits = backend.list_pull_request_commits(&details).unwrap();
        // then — both commits parse with summary, short sha, and author.
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].oid, "aaaaaaa1111111111111111111111111111aaaa");
        assert_eq!(commits[0].short_oid, "aaaaaaa");
        assert_eq!(commits[0].summary, "First commit");
        assert_eq!(commits[0].author, "Alice");
        assert!(commits[0].timestamp.is_some());
        assert_eq!(commits[1].summary, "Second commit");
        // and — the call targeted the pulls/<n>/commits endpoint.
        let calls = backend.runner.calls.borrow();
        let commits_call = calls
            .iter()
            .find(|args| {
                args.iter()
                    .any(|a| a.contains("/pulls/125/commits") && a.contains("per_page=100"))
            })
            .expect("expected a pulls commits api call");
        assert_eq!(commits_call[0], "api");
    }

    #[test]
    fn should_request_compare_endpoint_for_range_diff_when_no_local_checkout() {
        // given
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        // when
        let diff = backend
            .get_pull_request_commit_range_diff(&details, "baseaaa", "headbbb")
            .unwrap();
        // then
        assert_eq!(diff.len(), 1);
        assert!(diff[0].patch.contains("diff --git a/src/lib.rs"));
        // and — the call hit the compare endpoint with the Accept diff header.
        let calls = backend.runner.calls.borrow();
        let compare_call = calls
            .iter()
            .find(|args| {
                args.iter()
                    .any(|a| a.contains("/compare/baseaaa...headbbb"))
                    && args
                        .iter()
                        .any(|a| a == "Accept: application/vnd.github.diff")
            })
            .expect("expected a compare api call");
        assert!(compare_call.contains(&"Accept: application/vnd.github.diff".to_string()));
    }

    #[test]
    fn should_resolve_diff_base_sha_to_compare_merge_base_not_base_ref_oid() {
        // given — the mock reports baseRefOid 1234567890abcdef while compare
        // reports a different merge base, i.e. the base branch has moved.
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        assert_eq!(details.base_sha, "1234567890abcdef");
        // when
        let resolved = backend.resolve_diff_base_sha(&details);
        // then — the three-dot merge base wins over the branch tip.
        assert_eq!(resolved.as_deref(), Some("mergebase111"));
        // and — it hit compare between the reported base and head, without
        // the diff Accept header that would return patch text instead.
        let calls = backend.runner.calls.borrow();
        let compare_call = calls
            .iter()
            .find(|args| {
                args.iter()
                    .any(|a| a.contains("/compare/1234567890abcdef...abcdef1234567890"))
            })
            .expect("expected a compare api call");
        assert!(!compare_call.contains(&"Accept: application/vnd.github.diff".to_string()));
        assert!(compare_call.iter().any(|a| a.contains("per_page=1")));
    }

    #[test]
    fn should_leave_diff_base_sha_unrefined_when_compare_call_fails() {
        // given — a runner that fails every call.
        struct FailingGhRunner;
        impl GhCommandRunner for FailingGhRunner {
            fn run(&self, _args: &[String]) -> GhCommandResult<String> {
                Err(GhCommandError::Failed {
                    status: Some(1),
                    stderr: "network down".to_string(),
                })
            }
        }
        let details = GitHubGhBackend::with_runner(Some(repo()), FakeGhRunner::default())
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        let backend = GitHubGhBackend::with_runner(Some(repo()), FailingGhRunner);
        // when
        let resolved = backend.resolve_diff_base_sha(&details);
        // then — best-effort: callers keep the unrefined base_sha.
        assert_eq!(resolved, None);
    }

    /// Run `git` in `root`, returning trimmed stdout. Panics on failure so
    /// setup problems surface as test errors rather than silent `None`s.
    fn git(root: &Path, args: &[&str]) -> String {
        let out = run_command_output("git", Some(root), args.iter().map(|a| OsStr::new(*a)))
            .unwrap_or_else(|e| panic!("git {args:?} failed: {e:?}"));
        out.trim().to_string()
    }

    fn commit(root: &Path, name: &str) -> String {
        fs::write(root.join(name), name).unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", name, "--no-gpg-sign"]);
        git(root, &["rev-parse", "HEAD"])
    }

    #[test]
    fn should_resolve_diff_base_sha_locally_when_base_branch_moved_past_the_branch_point() {
        // given — a repo where the base tip has advanced past the commit the
        // PR branch forked from. This is exactly the drift that makes
        // baseRefOid the wrong revision to read the diff's old side at.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "--quiet"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "commit.gpgsign", "false"]);

        let fork_point = commit(root, "a");
        let pr_head = commit(root, "b");
        // Detach back to the fork point and advance the base independently.
        git(root, &["checkout", "--quiet", &fork_point]);
        let base_tip = commit(root, "c");
        assert_ne!(base_tip, fork_point);

        // when — the backend has a local checkout containing both commits.
        let mut backend = GitHubGhBackend::with_runner(Some(repo()), FakeGhRunner::default());
        backend.set_local_checkout(Some(root.to_path_buf()));
        let mut details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        details.base_sha = base_tip.clone();
        details.head_sha = pr_head;
        let resolved = backend.resolve_diff_base_sha(&details);

        // then — the fork point, not the base tip the forge reported.
        assert_eq!(resolved.as_deref(), Some(fork_point.as_str()));
        // and — no compare API call was needed.
        assert!(
            !backend
                .runner
                .calls
                .borrow()
                .iter()
                .any(|args| args.iter().any(|a| a.contains("/compare/"))),
            "local merge-base must not fall through to the compare API"
        );
    }

    #[test]
    fn should_fall_back_to_compare_api_when_local_checkout_lacks_the_commits() {
        // given — a valid repo that doesn't contain the PR's commits.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "--quiet"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        commit(root, "a");

        let mut backend = GitHubGhBackend::with_runner(Some(repo()), FakeGhRunner::default());
        backend.set_local_checkout(Some(root.to_path_buf()));
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        // when — base/head SHAs are absent from the checkout.
        let resolved = backend.resolve_diff_base_sha(&details);
        // then — the forge answers instead of a wrong-or-missing local one.
        assert_eq!(resolved.as_deref(), Some("mergebase111"));
    }

    #[test]
    fn maps_missing_gh_to_install_message() {
        let err = map_gh_error(GhCommandError::MissingGh, "github.com");
        assert!(err.to_string().contains("GitHub integration requires `gh`"));
    }

    #[test]
    fn maps_auth_failure_to_login_message() {
        let err = map_gh_error(
            GhCommandError::Failed {
                status: Some(4),
                stderr: "run `gh auth login` to authenticate".to_string(),
            },
            "github.example.com",
        );
        assert!(err.to_string().contains("GitHub authentication failed"));
        assert!(err.to_string().contains("github.example.com"));
    }

    // create_review tests

    use crate::forge::submit::{GhSide, InlineComment, SubmitEvent};
    use crate::forge::traits::CreateReviewRequest;
    use std::path::PathBuf;

    fn inline(line: u32, body: &str) -> InlineComment {
        InlineComment {
            path: PathBuf::from("src/lib.rs"),
            line,
            side: GhSide::Right,
            counterpart_line: None,
            start_line: None,
            start_side: None,
            range_anchors: None,
            old_path: None,
            body: body.to_string(),
            comment_id: format!("cid-{line}"),
        }
    }

    #[test]
    fn should_create_pending_review_for_first_comment() {
        let backend = GitHubGhBackend::with_runner(Some(repo()), FakeGhRunner::default());
        let id = backend
            .add_pending_comment(&repo(), 125, "head-sha", &inline(42, "text"))
            .unwrap();
        assert_eq!(id, 123456);
        let calls = backend.runner.stdin_calls.borrow();
        assert_eq!(calls.len(), 1);
        let payload: serde_json::Value = serde_json::from_str(&calls[0].1).unwrap();
        assert!(payload.get("event").is_none());
        assert_eq!(payload["commit_id"], "head-sha");
        assert_eq!(payload["comments"][0]["line"], 42);
    }

    #[test]
    fn should_attach_comment_to_existing_pending_review() {
        struct PendingRunner(TestCell<Vec<(Vec<String>, String)>>);
        impl GhCommandRunner for PendingRunner {
            fn run(&self, args: &[String]) -> GhCommandResult<String> {
                if args.get(1).map(String::as_str) == Some("user") {
                    Ok(r#"{"login":"reviewer"}"#.into())
                } else if args.get(1).map(String::as_str) == Some("graphql")
                    && args.iter().any(|arg| arg.contains("reviews(first:"))
                {
                    Ok(r#"{"data":{"repository":{"pullRequest":{"reviews":{"nodes":[{"id":"PRR_88","fullDatabaseId":"88"}],"pageInfo":{"hasNextPage":false}}}}}}"#.into())
                } else if args.get(1).map(String::as_str) == Some("graphql") {
                    Ok(
                        r#"{"data":{"addPullRequestReviewThread":{"thread":{"id":"thread-99"}}}}"#
                            .into(),
                    )
                } else {
                    Ok(r#"[{"id":77,"state":"PENDING","user":{"login":"other"}},{"id":88,"state":"PENDING","user":{"login":"reviewer"}}]"#.into())
                }
            }
            fn run_with_stdin(&self, args: &[String], stdin: &str) -> GhCommandResult<String> {
                self.0.borrow_mut().push((args.to_vec(), stdin.to_string()));
                Ok(
                    r#"{"data":{"addPullRequestReviewThread":{"thread":{"id":"thread-99"}}}}"#
                        .into(),
                )
            }
        }
        let backend = GitHubGhBackend::with_runner(Some(repo()), PendingRunner(Default::default()));
        let id = backend
            .add_pending_comment(&repo(), 125, "head-sha", &inline(42, "text"))
            .unwrap();
        assert_eq!(id, 88);
        let calls = backend.runner.0.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0[1], "graphql");
        let payload: serde_json::Value = serde_json::from_str(&calls[0].1).unwrap();
        assert_eq!(
            payload["variables"]["input"]["pullRequestReviewId"],
            "PRR_88"
        );
        assert_eq!(payload["variables"]["input"]["line"], 42);
        assert_eq!(payload["variables"]["input"]["body"], "text");
    }

    #[test]
    fn should_submit_existing_pending_review_instead_of_creating_another() {
        struct PendingRunner(TestCell<Vec<(Vec<String>, String)>>);
        impl GhCommandRunner for PendingRunner {
            fn run(&self, args: &[String]) -> GhCommandResult<String> {
                if args.get(1).map(String::as_str) == Some("user") {
                    Ok(r#"{"login":"reviewer"}"#.into())
                } else {
                    Ok(r#"[{"id":88,"state":"PENDING","user":{"login":"reviewer"}}]"#.into())
                }
            }
            fn run_with_stdin(&self, args: &[String], stdin: &str) -> GhCommandResult<String> {
                self.0.borrow_mut().push((args.to_vec(), stdin.to_string()));
                Ok(r#"{"id":88,"state":"APPROVED"}"#.into())
            }
        }
        let backend = GitHubGhBackend::with_runner(Some(repo()), PendingRunner(Default::default()));
        let response = backend
            .submit_pending_review(&repo(), 125, SubmitEvent::Approve, "head-sha", "LGTM", &[])
            .unwrap()
            .unwrap();
        assert_eq!(response.id, 88);
        let calls = backend.runner.0.borrow();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].0[1].ends_with("/reviews/88/events"));
        let payload: serde_json::Value = serde_json::from_str(&calls[0].1).unwrap();
        assert_eq!(payload["event"], "APPROVE");
    }

    #[test]
    fn should_skip_duplicate_comment_on_submit_pending_review_retry() {
        struct ExistingCommentsRunner(TestCell<Vec<(Vec<String>, String)>>);
        impl GhCommandRunner for ExistingCommentsRunner {
            fn run(&self, args: &[String]) -> GhCommandResult<String> {
                if args.get(1).map(String::as_str) == Some("user") {
                    Ok(r#"{"login":"reviewer"}"#.into())
                } else if args.get(1).is_some_and(|p| p.contains("/comments?")) {
                    Ok(r#"[{"path":"src/lib.rs","line":42,"side":"RIGHT","body":"already posted"}]"#.into())
                } else {
                    Ok(r#"[{"id":88,"state":"PENDING","user":{"login":"reviewer"}}]"#.into())
                }
            }
            fn run_with_stdin(&self, args: &[String], stdin: &str) -> GhCommandResult<String> {
                self.0.borrow_mut().push((args.to_vec(), stdin.to_string()));
                Ok(r#"{"id":88,"state":"COMMENTED"}"#.into())
            }
        }
        let backend =
            GitHubGhBackend::with_runner(Some(repo()), ExistingCommentsRunner(Default::default()));
        let comment = inline(42, "already posted");
        let response = backend
            .submit_pending_review(
                &repo(),
                125,
                SubmitEvent::Comment,
                "head-sha",
                "summary",
                &[comment],
            )
            .unwrap()
            .unwrap();
        assert_eq!(response.id, 88);
        let calls = backend.runner.0.borrow();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].0[1].ends_with("/reviews/88/events"));
    }

    #[test]
    fn should_reject_attaching_pending_comment_if_commit_mismatches() {
        struct MismatchedRunner;
        impl GhCommandRunner for MismatchedRunner {
            fn run(&self, args: &[String]) -> GhCommandResult<String> {
                if args.get(1).map(String::as_str) == Some("user") {
                    Ok(r#"{"login":"reviewer"}"#.into())
                } else {
                    Ok(r#"[{"id":88,"state":"PENDING","commit_id":"commit-aaa","user":{"login":"reviewer"}}]"#.into())
                }
            }
        }
        let backend = GitHubGhBackend::with_runner(Some(repo()), MismatchedRunner);
        let err = backend
            .add_pending_comment(&repo(), 125, "commit-bbb", &inline(42, "text"))
            .unwrap_err();
        assert!(err.to_string().contains("commit-aaa"));
        assert!(err.to_string().contains("commit-bbb"));
    }

    #[test]
    fn should_match_fallback_database_id_in_review_node_id() {
        struct DatabaseIdRunner;
        impl GhCommandRunner for DatabaseIdRunner {
            fn run(&self, args: &[String]) -> GhCommandResult<String> {
                if args.get(1).map(String::as_str) == Some("graphql") {
                    Ok(r#"{"data":{"repository":{"pullRequest":{"reviews":{"nodes":[{"id":"PRR_legacy","databaseId":88}],"pageInfo":{"hasNextPage":false}}}}}}"#.into())
                } else {
                    Ok("{}".into())
                }
            }
        }
        let backend = GitHubGhBackend::with_runner(Some(repo()), DatabaseIdRunner);
        let node_id = backend.review_node_id(&repo(), 125, 88).unwrap();
        assert_eq!(node_id, "PRR_legacy");
    }

    #[test]
    fn should_update_pending_review_body_on_draft_submit() {
        struct UpdateBodyRunner(TestCell<Vec<(Vec<String>, String)>>);
        impl GhCommandRunner for UpdateBodyRunner {
            fn run(&self, args: &[String]) -> GhCommandResult<String> {
                if args.get(1).map(String::as_str) == Some("user") {
                    Ok(r#"{"login":"reviewer"}"#.into())
                } else if args.get(1).is_some_and(|p| p.contains("/comments?")) {
                    Ok("[]".into())
                } else {
                    Ok(r#"[{"id":88,"state":"PENDING","user":{"login":"reviewer"}}]"#.into())
                }
            }
            fn run_with_stdin(&self, args: &[String], stdin: &str) -> GhCommandResult<String> {
                self.0.borrow_mut().push((args.to_vec(), stdin.to_string()));
                Ok(r#"{"id":88,"state":"PENDING"}"#.into())
            }
        }
        let backend =
            GitHubGhBackend::with_runner(Some(repo()), UpdateBodyRunner(Default::default()));
        let response = backend
            .submit_pending_review(
                &repo(),
                125,
                SubmitEvent::Draft,
                "head-sha",
                "draft summary",
                &[],
            )
            .unwrap()
            .unwrap();
        assert_eq!(response.id, 88);
        let calls = backend.runner.0.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0[1], "repos/agavra/tuicr/pulls/125/reviews/88");
        assert_eq!(calls[0].0[3], "PUT");
        let payload: serde_json::Value = serde_json::from_str(&calls[0].1).unwrap();
        assert_eq!(payload["body"], "draft summary");
    }

    #[test]
    fn should_post_create_review_to_reviews_endpoint_with_stdin_payload() {
        // given
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        let comments = vec![inline(42, "[ISSUE] boom")];
        let request = CreateReviewRequest {
            event: SubmitEvent::Comment,
            commit_id: "abcdef1234567890",
            body: "review body",
            comments: &comments,
        };
        // when
        let response = backend.create_review(&details, request).unwrap();
        // then
        assert_eq!(response.id, 123456);
        assert_eq!(response.state, "COMMENTED");
        assert!(response.html_url.contains("pullrequestreview-123456"));

        // and — the call hit the expected endpoint with stdin
        let stdin_calls = backend.runner.stdin_calls.borrow();
        assert_eq!(stdin_calls.len(), 1);
        let (args, stdin) = &stdin_calls[0];
        assert_eq!(args[0], "api");
        assert_eq!(args[1], "repos/agavra/tuicr/pulls/125/reviews");
        assert!(args.iter().any(|a| a == "--method"));
        assert!(args.iter().any(|a| a == "POST"));
        assert!(args.iter().any(|a| a == "--input"));
        assert!(args.iter().any(|a| a == "-"));
        // and — the stdin carries the JSON payload with the expected fields
        let payload: serde_json::Value = serde_json::from_str(stdin).unwrap();
        assert_eq!(payload["commit_id"], "abcdef1234567890");
        assert_eq!(payload["body"], "review body");
        assert_eq!(payload["event"], "COMMENT");
        assert_eq!(payload["comments"][0]["line"], 42);
        assert_eq!(payload["comments"][0]["side"], "RIGHT");
        // and — comment_id stays out of the payload (it's internal state)
        assert!(
            payload["comments"][0].get("comment_id").is_none(),
            "comment_id should not leak into the JSON payload"
        );
    }

    #[test]
    fn should_omit_event_field_for_draft_submission() {
        // given
        let runner = FakeGhRunner::default();
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        // when — draft submission
        let _ = backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::Draft,
                    commit_id: "sha",
                    body: "",
                    comments: &[],
                },
            )
            .unwrap();
        // then — the payload omits `event`
        let stdin_calls = backend.runner.stdin_calls.borrow();
        let (_, stdin) = &stdin_calls[0];
        let payload: serde_json::Value = serde_json::from_str(stdin).unwrap();
        assert!(payload.get("event").is_none());
    }

    #[test]
    fn should_send_hostname_argument_for_enterprise_host() {
        // given — enterprise host
        let runner = FakeGhRunner::default();
        let enterprise = ForgeRepository::github("github.example.com", "agavra", "tuicr");
        let backend = GitHubGhBackend::with_runner(Some(enterprise.clone()), runner);
        let mut details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        details.repository = enterprise;
        // when
        let _ = backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::Comment,
                    commit_id: "sha",
                    body: "",
                    comments: &[],
                },
            )
            .unwrap();
        // then
        let stdin_calls = backend.runner.stdin_calls.borrow();
        let (args, _) = &stdin_calls[0];
        assert!(args.iter().any(|a| a == "--hostname"));
        assert!(args.iter().any(|a| a == "github.example.com"));
    }

    #[test]
    fn should_map_permission_failure_to_pull_request_write_message() {
        // given — a runner that fails with a 403-style stderr
        let runner = FakeGhRunner::default();
        *runner.stdin_error.borrow_mut() = Some(GhCommandError::Failed {
            status: Some(1),
            stderr: "HTTP 403: Resource not accessible by integration".to_string(),
        });
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        // when
        let err = backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::Comment,
                    commit_id: "sha",
                    body: "",
                    comments: &[],
                },
            )
            .unwrap_err();
        // then
        let msg = err.to_string();
        assert!(
            msg.contains("pull request write permission"),
            "got: {msg:?}"
        );
    }

    #[test]
    fn should_map_auth_failure_during_create_review() {
        // given — a runner that fails with auth-failure stderr
        let runner = FakeGhRunner::default();
        *runner.stdin_error.borrow_mut() = Some(GhCommandError::Failed {
            status: Some(4),
            stderr: "Run `gh auth login` to authenticate".to_string(),
        });
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        // when
        let err = backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::Comment,
                    commit_id: "sha",
                    body: "",
                    comments: &[],
                },
            )
            .unwrap_err();
        // then
        assert!(err.to_string().contains("GitHub authentication failed"));
    }

    #[test]
    fn should_map_missing_gh_during_create_review() {
        // given — gh is not installed
        let runner = FakeGhRunner::default();
        *runner.stdin_error.borrow_mut() = Some(GhCommandError::MissingGh);
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        // when
        let err = backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::Comment,
                    commit_id: "sha",
                    body: "",
                    comments: &[],
                },
            )
            .unwrap_err();
        // then
        assert!(err.to_string().contains("GitHub integration requires `gh`"));
    }

    #[test]
    fn should_error_when_create_review_response_is_missing_id() {
        // given — malformed response from gh
        let runner = FakeGhRunner::default();
        *runner.stdin_response.borrow_mut() =
            Some(r#"{"html_url": "x", "state": "COMMENTED"}"#.to_string());
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        // when
        let err = backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::Comment,
                    commit_id: "sha",
                    body: "",
                    comments: &[],
                },
            )
            .unwrap_err();
        // then
        assert!(err.to_string().contains("Create-review response missing"));
    }

    #[test]
    fn should_recognize_must_have_pull_request_write_as_permission_failure() {
        // given
        let runner = FakeGhRunner::default();
        *runner.stdin_error.borrow_mut() = Some(GhCommandError::Failed {
            status: Some(1),
            stderr: "must have pull request write permission".to_string(),
        });
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        // when
        let err = backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::Comment,
                    commit_id: "sha",
                    body: "",
                    comments: &[],
                },
            )
            .unwrap_err();
        // then
        assert!(err.to_string().contains("pull request write permission"));
    }

    #[test]
    fn should_map_pending_review_conflict_to_finish_or_discard_message() {
        // 422 from GitHub when a pending review already exists for this user
        // on this PR. We surface a clear actionable message.
        let runner = FakeGhRunner::default();
        *runner.stdin_error.borrow_mut() = Some(GhCommandError::Failed {
            status: Some(22),
            stderr: "gh: Unprocessable Entity (HTTP 422)\n\
                {\"message\":\"Unprocessable Entity\",\"errors\":\
                [\"User can only have one pending review per pull request\"]}"
                .to_string(),
        });
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        let err = backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::Comment,
                    commit_id: "sha",
                    body: "",
                    comments: &[],
                },
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("pending review on this PR"),
            "expected pending-review hint, got: {msg:?}"
        );
        assert!(msg.contains("Finish or discard"), "got: {msg:?}");
    }

    #[test]
    fn should_map_unknown_commit_to_force_push_hint() {
        // 422 from GitHub when commit_id isn't part of the PR (e.g., the PR
        // was force-pushed since the session loaded).
        let runner = FakeGhRunner::default();
        *runner.stdin_error.borrow_mut() = Some(GhCommandError::Failed {
            status: Some(22),
            stderr: "gh: Unprocessable Entity (HTTP 422)\n\
                {\"message\":\"Unprocessable Entity\",\"errors\":\
                [\"The commitOID is not part of the pull request\"]}"
                .to_string(),
        });
        let backend = GitHubGhBackend::with_runner(Some(repo()), runner);
        let details = backend
            .get_pull_request(parse_pull_request_target("125").unwrap())
            .unwrap();
        let err = backend
            .create_review(
                &details,
                CreateReviewRequest {
                    event: SubmitEvent::Comment,
                    commit_id: "sha",
                    body: "",
                    comments: &[],
                },
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not part of this PR"),
            "expected unknown-commit hint, got: {msg:?}"
        );
        assert!(msg.contains(":e"), "got: {msg:?}");
    }
}
