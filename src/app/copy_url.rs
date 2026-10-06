use super::*;
use crate::forge::traits::ForgeKind;
use sha2::{Digest, Sha256};

impl App {
    pub fn url_at_cursor(&self) -> Option<String> {
        let DiffSource::PullRequest(pr) = &self.diff_source else {
            return None;
        };
        let annotation = self.line_annotations.get(self.diff_state.cursor_line)?;
        let url = match annotation {
            AnnotatedLine::RemoteThreadLine {
                thread_idx,
                comment_idx,
            } => self
                .forge_review_threads
                .get(*thread_idx)?
                .comments
                .get(*comment_idx)?
                .url
                .clone(),
            AnnotatedLine::RemoteReviewSummaryLine { summary_idx } => {
                self.forge_review_summaries.get(*summary_idx)?.url.clone()
            }
            AnnotatedLine::IssueComment { comment_idx } => self
                .pr_info
                .as_ref()?
                .issue_comments
                .get(*comment_idx)?
                .url
                .clone()?,
            AnnotatedLine::DiffLine {
                file_idx,
                old_lineno,
                new_lineno,
                ..
            }
            | AnnotatedLine::SideBySideLine {
                file_idx,
                old_lineno,
                new_lineno,
                ..
            } => {
                let old = matches!(annotation, AnnotatedLine::SideBySideLine { .. })
                    && self.cursor_side == LineSide::Old;
                let (line, side) = if old || new_lineno.is_none() {
                    ((*old_lineno)?, LineSide::Old)
                } else {
                    ((*new_lineno)?, LineSide::New)
                };
                let file = self.diff_files.get(*file_idx)?;
                let path = if side == LineSide::Old {
                    file.old_path
                        .as_deref()
                        .unwrap_or(file.display_path().as_path())
                } else {
                    file.display_path().as_path()
                };
                return file_line_url(pr, &path.to_string_lossy(), line, side);
            }
            _ => return None,
        };
        (!url.is_empty()).then_some(url)
    }
}

fn encode_path(path: &str) -> String {
    path.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn file_line_url(
    pr: &PullRequestDiffSource,
    path: &str,
    line: u32,
    side: LineSide,
) -> Option<String> {
    let repo = &pr.key.repository;
    let base = format!("https://{}/{}/{}", repo.host, repo.owner, repo.name);
    let sha = if side == LineSide::Old {
        &pr.base_sha
    } else {
        &pr.key.head_sha
    };
    let encoded = encode_path(path);
    Some(match repo.kind {
        ForgeKind::GitHub => {
            let hash = format!("{:x}", Sha256::digest(path.as_bytes()));
            let side = if side == LineSide::Old { 'L' } else { 'R' };
            format!(
                "{}/files#diff-{hash}{side}{line}",
                pr.url.trim_end_matches('/')
            )
        }
        ForgeKind::GitLab => format!("{base}/-/blob/{sha}/{encoded}#L{line}"),
        ForgeKind::Gitea => format!("{base}/src/commit/{sha}/{encoded}#L{line}"),
        ForgeKind::Bitbucket => format!("{base}/src/{sha}/{encoded}#lines-{line}"),
        // These forges do not expose a stable revision/file-line URL in the shared model.
        ForgeKind::AzureDevOps | ForgeKind::Gerrit => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(kind: ForgeKind) -> PullRequestDiffSource {
        PullRequestDiffSource {
            key: crate::forge::traits::PrSessionKey {
                repository: crate::forge::traits::ForgeRepository {
                    kind,
                    host: "example.com".into(),
                    owner: "owner".into(),
                    name: "repo".into(),
                },
                number: 1,
                head_sha: "head".into(),
            },
            base_sha: "base".into(),
            title: String::new(),
            url: "https://example.com/owner/repo/pull/1".into(),
            head_ref_name: String::new(),
            base_ref_name: String::new(),
            state: "open".into(),
            closed: false,
            merged: false,
        }
    }

    #[test]
    fn github_links_target_pr_diff_and_correct_side() {
        let pr = pr(ForgeKind::GitHub);
        let hash = format!("{:x}", Sha256::digest(b"src/main.rs"));
        for (side, marker) in [(LineSide::Old, 'L'), (LineSide::New, 'R')] {
            assert_eq!(
                file_line_url(&pr, "src/main.rs", 12, side),
                Some(format!("{}/files#diff-{hash}{marker}12", pr.url))
            );
        }
    }

    #[test]
    fn revision_links_encode_paths_and_select_base_or_head() {
        for (kind, route, anchor) in [
            (ForgeKind::GitLab, "-/blob", "L"),
            (ForgeKind::Gitea, "src/commit", "L"),
            (ForgeKind::Bitbucket, "src", "lines-"),
        ] {
            for (side, sha) in [(LineSide::Old, "base"), (LineSide::New, "head")] {
                assert_eq!(
                    file_line_url(&pr(kind), "dir/a #é.rs", 9, side),
                    Some(format!(
                        "https://example.com/owner/repo/{route}/{sha}/dir/a%20%23%C3%A9.rs#{anchor}9"
                    ))
                );
            }
        }
    }

    #[test]
    fn unsupported_line_links_do_not_copy_misleading_urls() {
        for kind in [ForgeKind::AzureDevOps, ForgeKind::Gerrit] {
            assert!(file_line_url(&pr(kind), "file", 1, LineSide::New).is_none());
        }
    }
}
