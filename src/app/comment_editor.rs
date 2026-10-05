use crate::editor::{EditorLaunch, LaunchState};
use std::io::Write;

use super::*;

fn edited_body(original: &str, edited: String) -> Option<String> {
    if edited == original || edited.strip_suffix('\n') == Some(original) {
        return None;
    }
    Some(edited)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentEditorIdentity {
    Local(String),
    Remote {
        id: String,
        viewer: String,
        repository: ForgeRepository,
        number: u64,
        head_sha: String,
    },
}

pub(crate) struct PrEditEvent {
    pub repository: ForgeRepository,
    pub number: u64,
    pub head_sha: String,
    pub viewer: String,
    pub id: String,
    pub body: String,
    pub result: std::result::Result<(), String>,
}

pub struct PendingCommentEditor {
    pub file: tempfile::NamedTempFile,
    pub original: String,
    pub identity: CommentEditorIdentity,
}

impl App {
    pub fn queue_editor_for_comment_at_cursor(&mut self) -> bool {
        let annotation = self
            .line_annotations
            .get(self.diff_state.cursor_line)
            .cloned();
        let candidate = match annotation {
            Some(
                AnnotatedLine::ReviewComment { .. }
                | AnnotatedLine::FileComment { .. }
                | AnnotatedLine::LineComment { .. },
            ) => self
                .find_comment_at_cursor()
                .and_then(|location| self.local_comment_at(&location))
                .map(|comment| {
                    (
                        comment.content.clone(),
                        CommentEditorIdentity::Local(comment.id.clone()),
                    )
                }),
            Some(AnnotatedLine::RemoteThreadLine {
                thread_idx,
                comment_idx,
            }) => {
                let comment = self
                    .forge_review_threads
                    .get(thread_idx)
                    .and_then(|t| t.comments.get(comment_idx));
                match (comment, self.pr_viewer_login.as_deref(), &self.diff_source) {
                    (Some(comment), Some(viewer), DiffSource::PullRequest(pr))
                        if pr.key.repository.kind == crate::forge::traits::ForgeKind::GitHub
                            && comment
                                .author
                                .as_deref()
                                .is_some_and(|author| author.eq_ignore_ascii_case(viewer)) =>
                    {
                        Some((
                            comment.body.clone(),
                            CommentEditorIdentity::Remote {
                                id: comment.id.clone(),
                                viewer: viewer.to_string(),
                                repository: pr.key.repository.clone(),
                                number: pr.key.number,
                                head_sha: pr.key.head_sha.clone(),
                            },
                        ))
                    }
                    _ => None,
                }
            }
            Some(AnnotatedLine::RemoteReviewSummaryLine { .. }) => {
                self.set_warning("GitHub review summaries cannot be edited with e");
                return true;
            }
            _ => return false,
        };
        let Some((body, identity)) = candidate else {
            self.set_warning("Comment unavailable or not yours to edit");
            return true;
        };
        if matches!(identity, CommentEditorIdentity::Local(_)) {
            let Some(location) = self.find_comment_at_cursor() else {
                return true;
            };
            if self
                .local_comment_at(&location)
                .is_some_and(Comment::is_locked)
            {
                self.set_warning("Submitted comments cannot be edited from the local session");
                return true;
            }
        }
        if self.pending_comment_editor.is_some()
            || !self.comment_editor_launches.is_empty()
            || self.pending_comment_rx.is_some()
            || !self.pending_comment_queue.is_empty()
            || self.pr_submit_rx.is_some()
            || self.pr_edit_rx.is_some()
            || self.pr_delete_rx.is_some()
            || self.pr_reply_rx.is_some()
        {
            self.set_warning("Wait for the current comment operation before editing");
            return true;
        }
        match tempfile::Builder::new()
            .prefix("tuicr-comment-")
            .suffix(".md")
            .tempfile()
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(body.as_bytes()) {
                    self.set_error(format!("Cannot prepare comment editor: {error}"));
                    return true;
                }
                self.pending_editor_target = Some(EditorTarget {
                    path: file.path().to_path_buf(),
                    line: None,
                    label: "comment".into(),
                });
                self.pending_comment_editor = Some(PendingCommentEditor {
                    file,
                    original: body,
                    identity,
                });
            }
            Err(error) => self.set_error(format!("Cannot prepare comment editor: {error}")),
        }
        true
    }

    pub fn finish_comment_editor(&mut self, pending: PendingCommentEditor) {
        match std::fs::read_to_string(pending.file.path()) {
            Ok(body) => match edited_body(&pending.original, body) {
                Some(body) => self.apply_comment_editor_change(pending.identity, body),
                None => self.set_message("Comment unchanged"),
            },
            Err(error) => self.set_error(format!("Cannot read edited comment: {error}")),
        }
    }

    pub(super) fn apply_comment_editor_change(
        &mut self,
        identity: CommentEditorIdentity,
        body: String,
    ) {
        if body.trim().is_empty() {
            self.set_warning("Comment body cannot be empty");
            return;
        }
        match identity {
            CommentEditorIdentity::Local(id) => {
                let Some(location) = self.session.find_comment_by_id(&id) else {
                    self.set_warning("Comment no longer exists");
                    return;
                };
                let comment = match location {
                    CommentLocation::Review { index } => {
                        self.session.review_comments.get_mut(index)
                    }
                    CommentLocation::File { path, index } => self
                        .session
                        .files
                        .get_mut(&path)
                        .and_then(|file| file.file_comments.get_mut(index)),
                    CommentLocation::Line {
                        path,
                        line,
                        side,
                        index,
                    } => self
                        .session
                        .files
                        .get_mut(&path)
                        .and_then(|file| file.line_comments.get_mut(&line))
                        .and_then(|comments| comments.get_mut(index))
                        .filter(|comment| comment.side.unwrap_or(LineSide::New) == side),
                };
                if let Some(comment) = comment.filter(|comment| !comment.is_locked()) {
                    comment.content = body;
                    self.dirty = true;
                    self.rebuild_annotations();
                    match self.save_current_session_merging_external() {
                        Ok(_) => self.set_message("Comment updated"),
                        Err(error) => {
                            self.set_error(format!("Comment updated but save failed: {error}"))
                        }
                    }
                } else {
                    self.set_warning("Comment can no longer be edited locally");
                }
            }
            CommentEditorIdentity::Remote {
                id,
                viewer,
                repository,
                number,
                head_sha,
            } => {
                let valid_pr = matches!(&self.diff_source, DiffSource::PullRequest(pr)
                    if pr.key.repository == repository && pr.key.number == number && pr.key.head_sha == head_sha);
                if !valid_pr || self.pr_viewer_login.as_deref() != Some(viewer.as_str()) {
                    self.set_warning("PR changed before comment edit could be saved");
                    return;
                }
                if self.pr_edit_rx.is_some()
                    || self.pr_reload_rx.is_some()
                    || self.pr_threads_rx.is_some()
                    || self.forge_review_threads_loading
                    || self.pr_thread_resolution_rx.is_some()
                    || self.pr_reaction_rx.is_some()
                    || self.pr_delete_rx.is_some()
                    || self.pr_reply_rx.is_some()
                    || self.pr_submit_rx.is_some()
                    || self.pending_comment_rx.is_some()
                    || !self.pending_comment_queue.is_empty()
                {
                    self.set_warning("Wait for the current GitHub operation before editing");
                    return;
                }
                let owned = self
                    .forge_review_threads
                    .iter()
                    .flat_map(|thread| &thread.comments)
                    .any(|comment| {
                        comment.id == id
                            && comment
                                .author
                                .as_deref()
                                .is_some_and(|author| author.eq_ignore_ascii_case(&viewer))
                    });
                if !owned {
                    self.set_warning("Comment is no longer available for editing");
                    return;
                }
                let (tx, rx) = std::sync::mpsc::channel();
                self.pr_edit_rx = Some(rx);
                std::thread::spawn(move || {
                    let backend = create_forge_backend(&repository, None, false, false);
                    let result = backend
                        .current_viewer(&repository)
                        .and_then(|actual| match actual {
                            Some(actual) if actual.eq_ignore_ascii_case(&viewer) => {
                                backend.update_review_comment(&repository, &id, &body)
                            }
                            _ => Err(TuicrError::Forge(
                                "GitHub account changed; refresh the PR before editing".into(),
                            )),
                        })
                        .map_err(|error| error.to_string());
                    let _ = tx.send(PrEditEvent {
                        repository,
                        number,
                        head_sha,
                        viewer,
                        id,
                        body,
                        result,
                    });
                });
                self.set_message("Updating comment on GitHub…");
            }
        }
    }

    pub fn poll_pr_edit_events(&mut self) {
        let Some(rx) = &self.pr_edit_rx else {
            return;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pr_edit_rx = None;
                self.set_error("GitHub comment update worker disconnected");
                return;
            }
        };
        self.pr_edit_rx = None;
        let PrEditEvent {
            repository,
            number,
            head_sha,
            viewer,
            id,
            body,
            result,
        } = event;
        let valid_pr = matches!(&self.diff_source, DiffSource::PullRequest(pr)
            if pr.key.repository == repository && pr.key.number == number && pr.key.head_sha == head_sha);
        if !valid_pr || self.pr_viewer_login.as_deref() != Some(viewer.as_str()) {
            return;
        }
        match result {
            Ok(()) => {
                for thread in &mut self.forge_review_threads {
                    for comment in &mut thread.comments {
                        if comment.id == id {
                            comment.body = body.clone();
                        }
                    }
                }
                self.rebuild_annotations();
                self.set_message("Comment updated on GitHub");
            }
            Err(error) => self.set_error(format!("GitHub comment update failed: {error}")),
        }
    }

    pub fn track_comment_editor_launch(
        &mut self,
        launch: EditorLaunch,
        pending: PendingCommentEditor,
    ) {
        self.comment_editor_launches.push((launch, pending));
    }

    pub fn poll_comment_editor_launches(&mut self) -> bool {
        let mut finished = Vec::new();
        let mut changed = false;
        self.comment_editor_launches
            .retain_mut(|(launch, pending)| match launch.poll() {
                LaunchState::Running => true,
                LaunchState::Exited => {
                    finished.push((
                        pending.identity.clone(),
                        std::fs::read_to_string(pending.file.path()),
                        pending.original.clone(),
                    ));
                    changed = true;
                    false
                }
                LaunchState::FailedToLaunch(status) => {
                    finished.push((
                        pending.identity.clone(),
                        Err(std::io::Error::other(format!("Editor failed: {status}"))),
                        pending.original.clone(),
                    ));
                    changed = true;
                    false
                }
            });
        for (identity, result, original) in finished {
            match result {
                Ok(body) => match edited_body(&original, body) {
                    Some(body) => self.apply_comment_editor_change(identity, body),
                    None => self.set_message("Comment unchanged"),
                },
                Err(error) => self.set_error(format!("Cannot read edited comment: {error}")),
            }
        }
        changed
    }

    pub fn take_pending_comment_editor(&mut self) -> Option<PendingCommentEditor> {
        self.pending_comment_editor.take()
    }
}
