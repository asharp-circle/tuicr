//! Parse extended `gh pr view --json` fields for the PR description panel.
//!
//! `gh` flattens some GraphQL connections to bare arrays in JSON export
//! (`reviewRequests`, `latestReviews`, `statusCheckRollup`), but older paths
//! and partial responses can still expose nested `{ nodes: [...] }` shapes.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::error::{Result, TuicrError};
use crate::forge::github::models::GhPullRequestDetails;
use crate::forge::traits::{
    ForgeRepository, PullRequestCheckStatus, PullRequestInfo, PullRequestIssueComment,
    PullRequestReviewStatus,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GhPullRequestInfoResponse {
    #[serde(flatten)]
    pub details: GhPullRequestDetails,
    #[serde(default)]
    pub review_decision: Option<String>,
    #[serde(default)]
    pub mergeable: Option<String>,
    #[serde(default)]
    pub merge_state_status: Option<String>,
    #[serde(default, deserialize_with = "deserialize_review_requests")]
    review_requests: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_latest_reviews")]
    latest_reviews: Vec<PullRequestReviewStatus>,
    #[serde(default, deserialize_with = "deserialize_status_checks")]
    status_check_rollup: Vec<PullRequestCheckStatus>,
    #[serde(
        default,
        deserialize_with = "deserialize_issue_comments",
        rename = "comments"
    )]
    issue_comments: Vec<PullRequestIssueComment>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhLatestReview {
    #[serde(default)]
    author: Option<GhAuthor>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    submitted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct GhAuthor {
    #[serde(default)]
    login: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhReviewRequestFlat {
    #[serde(default, rename = "__typename")]
    type_name: Option<String>,
    #[serde(default)]
    login: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    slug: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GhReviewRequestWrappedNode {
    #[serde(default, rename = "requestedReviewer")]
    requested_reviewer: GhReviewRequestFlat,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GhReviewRequestsInput {
    Flat(Vec<GhReviewRequestFlat>),
    Wrapped {
        nodes: Vec<GhReviewRequestWrappedNode>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GhLatestReviewsInput {
    Flat(Vec<GhLatestReview>),
    Wrapped { nodes: Vec<GhLatestReview> },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum GhStatusCheck {
    CheckRun {
        #[serde(default)]
        name: String,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        conclusion: Option<String>,
        #[serde(default, rename = "detailsUrl")]
        details_url: Option<String>,
    },
    StatusContext {
        #[serde(default)]
        context: String,
        #[serde(default)]
        state: Option<String>,
        #[serde(default, rename = "targetUrl")]
        target_url: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct GhStatusCheckRollupNested {
    #[serde(default)]
    nodes: Vec<GhStatusCheckRollupNode>,
}

#[derive(Debug, Default, Deserialize)]
struct GhStatusCheckRollupNode {
    #[serde(default)]
    commit: GhStatusCheckRollupCommit,
}

#[derive(Debug, Default, Deserialize)]
struct GhStatusCheckRollupCommit {
    #[serde(default, rename = "statusCheckRollup")]
    status_check_rollup: GhStatusCheckContexts,
}

#[derive(Debug, Default, Deserialize)]
struct GhStatusCheckContexts {
    #[serde(default)]
    contexts: GhStatusCheckContextNodes,
}

#[derive(Debug, Default, Deserialize)]
struct GhStatusCheckContextNodes {
    #[serde(default)]
    nodes: Vec<GhStatusCheck>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GhStatusCheckRollupInput {
    Flat(Vec<GhStatusCheck>),
    Nested(GhStatusCheckRollupNested),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhIssueComment {
    #[serde(default)]
    author: Option<GhAuthor>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GhIssueCommentsInput {
    Flat(Vec<GhIssueComment>),
    Wrapped { nodes: Vec<GhIssueComment> },
}

pub(crate) fn parse_pull_request_info(
    json: &str,
    repository: &ForgeRepository,
) -> Result<PullRequestInfo> {
    match serde_json::from_str::<GhPullRequestInfoResponse>(json) {
        Ok(response) => response.into_info(repository),
        Err(e) => {
            let details: GhPullRequestDetails = serde_json::from_str(json).map_err(|_| {
                TuicrError::Forge(format!("Failed to parse GitHub PR info response: {e}"))
            })?;
            Ok(PullRequestInfo::from_details(
                details.into_details(repository)?,
            ))
        }
    }
}

impl GhPullRequestInfoResponse {
    fn into_info(self, repository: &ForgeRepository) -> Result<PullRequestInfo> {
        let details = self.details.into_details(repository)?;

        Ok(PullRequestInfo {
            details,
            review_decision: non_empty(self.review_decision),
            mergeable: non_empty(self.mergeable),
            merge_state: non_empty(self.merge_state_status),
            requested_reviewers: sorted_requested(self.review_requests),
            latest_reviews: self.latest_reviews,
            checks: sorted_checks(self.status_check_rollup),
            issue_comments: self.issue_comments,
        })
    }
}

fn deserialize_review_requests<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<GhReviewRequestsInput>::deserialize(deserializer)?;
    Ok(match value {
        None => Vec::new(),
        Some(GhReviewRequestsInput::Flat(nodes)) => nodes
            .into_iter()
            .filter_map(review_request_flat_label)
            .collect(),
        Some(GhReviewRequestsInput::Wrapped { nodes }) => nodes
            .into_iter()
            .filter_map(|node| review_request_flat_label(node.requested_reviewer))
            .collect(),
    })
}

fn deserialize_latest_reviews<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<PullRequestReviewStatus>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<GhLatestReviewsInput>::deserialize(deserializer)?;
    Ok(match value {
        None => Vec::new(),
        Some(
            GhLatestReviewsInput::Flat(reviews) | GhLatestReviewsInput::Wrapped { nodes: reviews },
        ) => reviews
            .into_iter()
            .filter_map(latest_review_status)
            .collect(),
    })
}

fn deserialize_status_checks<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<PullRequestCheckStatus>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<GhStatusCheckRollupInput>::deserialize(deserializer)?;
    Ok(match value {
        None => Vec::new(),
        Some(GhStatusCheckRollupInput::Flat(checks)) => {
            checks.into_iter().filter_map(status_check_label).collect()
        }
        Some(GhStatusCheckRollupInput::Nested(nested)) => nested
            .nodes
            .into_iter()
            .flat_map(|node| node.commit.status_check_rollup.contexts.nodes)
            .filter_map(status_check_label)
            .collect(),
    })
}

fn deserialize_issue_comments<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<PullRequestIssueComment>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<GhIssueCommentsInput>::deserialize(deserializer)?;
    Ok(match value {
        None => Vec::new(),
        Some(
            GhIssueCommentsInput::Flat(comments)
            | GhIssueCommentsInput::Wrapped { nodes: comments },
        ) => comments
            .into_iter()
            .filter_map(issue_comment_label)
            .collect(),
    })
}

fn issue_comment_label(comment: GhIssueComment) -> Option<PullRequestIssueComment> {
    let body = comment.body.trim();
    if body.is_empty() {
        return None;
    }
    Some(PullRequestIssueComment {
        author: comment.author.and_then(|a| a.login),
        body: comment.body,
        url: non_empty(comment.url),
        created_at: comment.created_at,
    })
}

fn review_request_flat_label(node: GhReviewRequestFlat) -> Option<String> {
    match node.type_name.as_deref() {
        Some("Team") => node
            .name
            .filter(|s| !s.is_empty())
            .or(node.slug.filter(|s| !s.is_empty())),
        _ => node.login.filter(|s| !s.is_empty()),
    }
}

fn latest_review_status(review: GhLatestReview) -> Option<PullRequestReviewStatus> {
    let state = review.state.filter(|s| !s.is_empty())?;
    Some(PullRequestReviewStatus {
        author: review.author.and_then(|a| a.login),
        state,
        submitted_at: review.submitted_at,
    })
}

fn status_check_label(check: GhStatusCheck) -> Option<PullRequestCheckStatus> {
    match check {
        GhStatusCheck::CheckRun {
            name,
            status,
            conclusion,
            details_url,
        } => {
            if name.is_empty() {
                None
            } else {
                Some(PullRequestCheckStatus {
                    name,
                    status: non_empty(status),
                    conclusion: non_empty(conclusion),
                    url: non_empty(details_url),
                })
            }
        }
        GhStatusCheck::StatusContext {
            context,
            state,
            target_url,
        } => {
            if context.is_empty() {
                None
            } else {
                Some(PullRequestCheckStatus {
                    name: context,
                    status: None,
                    conclusion: non_empty(state),
                    url: non_empty(target_url),
                })
            }
        }
        GhStatusCheck::Unknown => None,
    }
}

/// Stable display order shared by the live and the REST-built paths, since
/// GraphQL and REST order checks and requested reviewers differently.
pub(crate) fn sorted_checks(
    mut checks: Vec<PullRequestCheckStatus>,
) -> Vec<PullRequestCheckStatus> {
    checks.sort_by(|a, b| {
        (&a.name, &a.status, &a.conclusion).cmp(&(&b.name, &b.status, &b.conclusion))
    });
    checks
}

pub(crate) fn sorted_requested(mut labels: Vec<String>) -> Vec<String> {
    labels.sort();
    labels
}

/// GraphQL spells bot logins without the REST `[bot]` suffix.
fn graphql_login(login: &str) -> String {
    login.strip_suffix("[bot]").unwrap_or(login).to_string()
}

fn text(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn upper(value: Option<String>) -> Option<String> {
    value.map(|s| s.to_ascii_uppercase())
}

/// Maps validated REST check runs and combined statuses to the same rows the
/// GraphQL `statusCheckRollup` yields: enum values upper-cased, nameless rows
/// dropped, check runs before legacy status contexts. `None` on bad JSON.
pub(crate) fn checks_from_rest(
    check_runs: &str,
    statuses: &str,
) -> Option<Vec<PullRequestCheckStatus>> {
    let runs: Vec<serde_json::Value> = serde_json::from_str(check_runs).ok()?;
    let contexts: Vec<serde_json::Value> = serde_json::from_str(statuses).ok()?;
    let runs = runs.iter().filter_map(|r| {
        Some(PullRequestCheckStatus {
            name: text(r, "name")?,
            status: upper(text(r, "status")),
            conclusion: upper(text(r, "conclusion")),
            url: text(r, "details_url"),
        })
    });
    let contexts = contexts.iter().filter_map(|c| {
        Some(PullRequestCheckStatus {
            name: text(c, "context")?,
            status: None,
            conclusion: upper(text(c, "state")),
            url: text(c, "target_url"),
        })
    });
    Some(runs.chain(contexts).collect())
}

/// Maps validated REST issue comments to the panel's conversation comments.
pub(crate) fn comments_from_rest(json: &str) -> Option<Vec<PullRequestIssueComment>> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
    Some(
        rows.iter()
            .filter_map(|c| {
                let body = text(c, "body").filter(|b| !b.trim().is_empty())?;
                Some(PullRequestIssueComment {
                    author: c
                        .get("user")
                        .and_then(|u| text(u, "login"))
                        .map(|l| graphql_login(&l)),
                    body,
                    url: text(c, "html_url"),
                    created_at: text(c, "created_at").and_then(|t| t.parse().ok()),
                })
            })
            .collect(),
    )
}

/// Maps the validated REST requested reviewers (users, then teams) to labels.
pub(crate) fn requested_from_rest(json: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let users = v.get("users")?.as_array()?;
    let teams = v.get("teams")?.as_array()?;
    Some(
        users
            .iter()
            .filter_map(|u| text(u, "login"))
            .chain(
                teams
                    .iter()
                    .filter_map(|t| text(t, "name").or_else(|| text(t, "slug"))),
            )
            .collect(),
    )
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::traits::ForgeRepository;

    fn repo() -> ForgeRepository {
        ForgeRepository {
            kind: crate::forge::traits::ForgeKind::GitHub,
            host: "github.com".to_string(),
            owner: "owner".to_string(),
            name: "repo".to_string(),
        }
    }

    const BASE_FIELDS: &str = r#"
            "number": 42,
            "title": "Add panel",
            "url": "https://github.com/owner/repo/pull/42",
            "state": "OPEN",
            "isDraft": false,
            "author": { "login": "alice" },
            "headRefName": "feature",
            "baseRefName": "main",
            "headRefOid": "abc1234567890",
            "baseRefOid": "def0987654321",
            "body": "Ship the panel",
            "updatedAt": "2026-06-01T12:00:00Z",
            "closed": false,
            "mergedAt": null,
            "reviewDecision": "REVIEW_REQUIRED",
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "BLOCKED"
    "#;

    #[test]
    fn should_parse_gh_flat_export_shapes() {
        let json = format!(
            r#"{{
            {BASE_FIELDS},
            "reviewRequests": [
                {{ "__typename": "User", "login": "bob" }},
                {{ "__typename": "Team", "name": "security", "slug": "org/security" }}
            ],
            "latestReviews": [
                {{
                    "author": {{ "login": "carol" }},
                    "state": "APPROVED",
                    "submittedAt": "2026-06-01T11:00:00Z"
                }}
            ],
            "statusCheckRollup": [
                {{
                    "__typename": "CheckRun",
                    "name": "build",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS"
                }}
            ]
        }}"#
        );

        let info = parse_pull_request_info(&json, &repo()).unwrap();
        assert_eq!(info.requested_reviewers, vec!["bob", "security"]);
        assert_eq!(info.latest_reviews.len(), 1);
        assert_eq!(info.checks.len(), 1);
    }

    #[test]
    fn should_parse_wrapped_graphql_shapes() {
        let json = format!(
            r#"{{
            {BASE_FIELDS},
            "reviewRequests": {{
                "nodes": [
                    {{ "requestedReviewer": {{ "__typename": "User", "login": "bob" }} }}
                ]
            }},
            "latestReviews": {{
                "nodes": [
                    {{ "author": {{ "login": "carol" }}, "state": "APPROVED" }}
                ]
            }},
            "statusCheckRollup": {{
                "nodes": [
                    {{
                        "commit": {{
                            "statusCheckRollup": {{
                                "contexts": {{
                                    "nodes": [
                                        {{
                                            "__typename": "StatusContext",
                                            "context": "ci/travis",
                                            "state": "FAILURE"
                                        }}
                                    ]
                                }}
                            }}
                        }}
                    }}
                ]
            }}
        }}"#
        );

        let info = parse_pull_request_info(&json, &repo()).unwrap();
        assert_eq!(info.requested_reviewers, vec!["bob"]);
        assert_eq!(info.latest_reviews.len(), 1);
        assert_eq!(info.checks[0].name, "ci/travis");
    }

    #[test]
    fn should_parse_issue_comments_and_check_urls() {
        let json = format!(
            r#"{{
            {BASE_FIELDS},
            "comments": [
                {{
                    "author": {{ "login": "dave" }},
                    "body": "Ship it",
                    "url": "https://github.com/owner/repo/pull/42#issuecomment-1",
                    "createdAt": "2026-06-01T10:00:00Z"
                }}
            ],
            "statusCheckRollup": [
                {{
                    "__typename": "CheckRun",
                    "name": "build",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                    "detailsUrl": "https://github.com/owner/repo/actions/runs/1"
                }}
            ]
        }}"#
        );

        let info = parse_pull_request_info(&json, &repo()).unwrap();
        assert_eq!(info.issue_comments.len(), 1);
        assert_eq!(info.issue_comments[0].body, "Ship it");
        assert_eq!(
            info.issue_comments[0].url.as_deref(),
            Some("https://github.com/owner/repo/pull/42#issuecomment-1")
        );
        assert_eq!(
            info.checks[0].url.as_deref(),
            Some("https://github.com/owner/repo/actions/runs/1")
        );
    }

    #[test]
    fn rest_checks_map_to_the_graphql_rollup_rows() {
        let runs = r#"[{"name":"build","status":"completed","conclusion":"success","details_url":"https://x/1"},
            {"name":"lint","status":"in_progress","conclusion":null,"details_url":""},{"name":"","status":"queued"}]"#;
        let statuses = r#"[{"context":"atlantis/plan","state":"success","target_url":""}]"#;
        let checks = checks_from_rest(runs, statuses).unwrap();
        assert_eq!(checks.len(), 3);
        assert_eq!(
            (checks[0].status.as_deref(), checks[0].conclusion.as_deref()),
            (Some("COMPLETED"), Some("SUCCESS"))
        );
        assert_eq!(checks[1].status.as_deref(), Some("IN_PROGRESS"));
        assert!(checks[1].conclusion.is_none() && checks[1].url.is_none());
        assert_eq!(
            (checks[2].name.as_str(), checks[2].conclusion.as_deref()),
            ("atlantis/plan", Some("SUCCESS"))
        );
        assert!(checks_from_rest("nope", "[]").is_none());
    }

    #[test]
    fn live_and_rest_checks_render_in_the_same_order() {
        let rest = checks_from_rest(
            r#"[{"name":"b","status":"completed","conclusion":"success"},{"name":"a","status":"completed","conclusion":"success"}]"#,
            r#"[{"context":"a0","state":"success"}]"#,
        )
        .unwrap();
        let graphql_order = vec![rest[2].clone(), rest[0].clone(), rest[1].clone()];
        let names = |v: Vec<PullRequestCheckStatus>| -> Vec<String> {
            sorted_checks(v).into_iter().map(|c| c.name).collect()
        };
        assert_eq!(names(rest), names(graphql_order));
        assert_eq!(sorted_requested(vec!["b".into(), "a".into()]), ["a", "b"]);
    }

    #[test]
    fn rest_comments_and_requested_reviewers_match_graphql_labels() {
        let c = r#"[{"user":{"login":"caliber-ai-reviewer[bot]"},"body":"hi","html_url":"https://x/c","created_at":"2026-10-09T18:02:37Z"},
            {"user":{"login":"a"},"body":"  "},{"user":null,"body":"ghost"}]"#;
        let comments = comments_from_rest(c).unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].author.as_deref(), Some("caliber-ai-reviewer"));
        assert!(comments[0].created_at.is_some() && comments[1].author.is_none());
        let r = r#"{"users":[{"login":"u"}],"teams":[{"name":"Core","slug":"core"},{"slug":"s"}]}"#;
        assert_eq!(requested_from_rest(r).unwrap(), ["u", "Core", "s"]);
        assert!(requested_from_rest(r#"{"users":[]}"#).is_none());
    }

    #[test]
    fn should_tolerate_empty_optional_sections() {
        let json = r#"{
            "number": 1,
            "title": "Empty",
            "url": "https://github.com/owner/repo/pull/1",
            "state": "OPEN",
            "headRefName": "feature",
            "baseRefName": "main",
            "headRefOid": "abc1234567890",
            "baseRefOid": "def0987654321",
            "body": "",
            "closed": false
        }"#;

        let info = parse_pull_request_info(json, &repo()).unwrap();
        assert!(info.requested_reviewers.is_empty());
        assert!(info.latest_reviews.is_empty());
        assert!(info.checks.is_empty());
    }
}
