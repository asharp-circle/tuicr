use super::*;
use crate::forge::remote_comments::{GITHUB_REACTIONS, RemoteReviewComment};

impl App {
    fn is_github_operation_busy(&self) -> bool {
        self.pr_reaction_rx.is_some()
            || self.pr_threads_rx.is_some()
            || self.pr_delete_rx.is_some()
            || self.pr_reply_rx.is_some()
            || self.pr_thread_resolution_rx.is_some()
            || self.forge_review_threads_loading
    }

    pub fn open_reaction_picker(&mut self) {
        let DiffSource::PullRequest(pr) = &self.diff_source else {
            self.set_message("Reactions are only available on GitHub PRs");
            return;
        };
        if pr.key.repository.kind != crate::forge::traits::ForgeKind::GitHub {
            self.set_message("Reactions are only available on GitHub PRs");
            return;
        }
        if self.is_github_operation_busy() {
            self.set_message("Wait for the current GitHub operation to finish");
            return;
        }
        let Some(AnnotatedLine::RemoteThreadLine {
            thread_idx,
            comment_idx,
        }) = self.line_annotations.get(self.diff_state.cursor_line)
        else {
            self.set_message("Move cursor to a remote review comment");
            return;
        };
        let Some(comment) = self
            .forge_review_threads
            .get(*thread_idx)
            .and_then(|thread| thread.comments.get(*comment_idx))
        else {
            self.set_message("Comment is no longer available; refresh the PR");
            return;
        };
        self.reaction_target = Some((pr.key.clone(), comment.id.clone()));
        self.reaction_cursor = 0;
        self.input_mode = InputMode::ReactionPicker;
    }

    pub fn select_reaction(&mut self) {
        self.select_reaction_with_worker(|key, comment_id, content, remove, tx| {
            std::thread::spawn(move || {
                let backend = super::create_forge_backend(&key.repository, None, false, false);
                let result = backend
                    .toggle_comment_reaction(&key.repository, &comment_id, &content, remove)
                    .map_err(|error| error.to_string());
                let _ = tx.send(PrReactionEvent { key, result });
            });
        });
    }

    pub(crate) fn select_reaction_with_worker<F>(&mut self, spawn_worker: F)
    where
        F: FnOnce(
            crate::forge::traits::PrSessionKey,
            String,
            String,
            bool,
            std::sync::mpsc::Sender<PrReactionEvent>,
        ),
    {
        if self.is_github_operation_busy() {
            self.set_message("Wait for the current GitHub operation to finish");
            return;
        }
        let Some((target_key, comment_id)) = self.reaction_target.take() else {
            self.input_mode = InputMode::Normal;
            return;
        };
        self.input_mode = InputMode::Normal;
        let DiffSource::PullRequest(pr) = &self.diff_source else {
            self.set_warning("PR changed while selecting a reaction");
            return;
        };
        if pr.key != target_key {
            self.set_warning("PR changed while selecting a reaction");
            return;
        }
        let Some(comment) = self
            .forge_review_threads
            .iter()
            .flat_map(|thread| &thread.comments)
            .find(|comment| comment.id == comment_id)
        else {
            self.set_warning("Comment is no longer available; refresh the PR");
            return;
        };
        let key = pr.key.clone();
        let content = GITHUB_REACTIONS[self.reaction_cursor].0.to_string();
        let remove = Self::comment_reaction_is_remove(comment, &content);
        let (tx, rx) = std::sync::mpsc::channel();
        self.pr_reaction_rx = Some(rx);
        spawn_worker(key, comment_id, content, remove, tx);
        self.set_message("Updating GitHub reaction…");
    }

    pub(crate) fn comment_reaction_is_remove(comment: &RemoteReviewComment, content: &str) -> bool {
        comment
            .reactions
            .iter()
            .any(|reaction| reaction.content == content && reaction.viewer_has_reacted)
    }

    pub fn poll_pr_reaction_events(&mut self) {
        self.poll_pr_reaction_events_with_refetch(|app| app.refetch_pr_threads());
    }

    pub(crate) fn poll_pr_reaction_events_with_refetch<F>(&mut self, mut refetch: F)
    where
        F: FnMut(&mut Self),
    {
        let Some(rx) = self.pr_reaction_rx.as_ref() else {
            return;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pr_reaction_rx = None;
                self.set_error("GitHub reaction update failed: worker disconnected");
                refetch(self);
                return;
            }
        };
        self.pr_reaction_rx = None;
        if !matches!(&self.diff_source, DiffSource::PullRequest(pr) if pr.key == event.key) {
            return;
        }
        match event.result {
            Ok(()) => self.set_message("GitHub reaction updated"),
            Err(error) => self.set_error(format!("GitHub reaction update failed: {error}")),
        }
        refetch(self);
    }
}

#[derive(Debug)]
pub(crate) struct PrReactionEvent {
    pub(crate) key: crate::forge::traits::PrSessionKey,
    pub(crate) result: std::result::Result<(), String>,
}
