use super::*;

enum SummaryAnnotationTarget {
    Review {
        comment_idx: usize,
    },
    File {
        file_idx: usize,
        comment_idx: usize,
    },
    Line {
        file_idx: usize,
        line: u32,
        side: LineSide,
        comment_idx: usize,
    },
}

impl App {
    /// Whether the `═══ Review Comments ═══` section has anything to show:
    /// a remote review summary, a local review-level comment, or a visible
    /// review-level (line: None) remote thread. Mirrors exactly what the
    /// section renders, so the header gate stays in sync between the renderer
    /// and the annotation model.
    pub fn has_review_section_content(&self) -> bool {
        if !self.forge_review_summaries.is_empty() || !self.session.review_comments.is_empty() {
            return true;
        }
        let visibility = self.session.remote_comments_visibility;
        if matches!(
            visibility,
            crate::forge::remote_comments::PrCommentsVisibility::Hide
        ) {
            return false;
        }
        self.forge_review_threads
            .iter()
            .filter(|thread| thread.line.is_none())
            .any(|thread| visibility.render_decision(thread).is_some())
    }

    /// Whether the `═══ Review Comments ═══` section header should render.
    /// Omitted in single-file view and while the section has no content yet.
    pub fn show_review_comments_header(&self) -> bool {
        !self.is_single_file_view && self.has_review_section_content()
    }

    pub fn comment_navigator_idx_at_screen_row(&self, screen_row: u16) -> Option<usize> {
        let inner = self.comment_navigator_inner_area?;
        if screen_row < inner.y || screen_row >= inner.y + inner.height {
            return None;
        }
        let rel = (screen_row - inner.y) as usize;
        let idx = self.comment_navigator_state.list_state.offset() + rel;
        let total = self.build_comment_navigator_items().len();
        (idx < total).then_some(idx)
    }

    fn comment_navigator_key(annotation: &AnnotatedLine) -> Option<CommentNavigatorKey> {
        match annotation {
            AnnotatedLine::ReviewComment { comment_idx } => Some(CommentNavigatorKey::Review {
                comment_idx: *comment_idx,
            }),
            AnnotatedLine::FileComment {
                file_idx,
                comment_idx,
            } => Some(CommentNavigatorKey::File {
                file_idx: *file_idx,
                comment_idx: *comment_idx,
            }),
            AnnotatedLine::LineComment {
                file_idx,
                line,
                side,
                comment_idx,
            } => Some(CommentNavigatorKey::Line {
                file_idx: *file_idx,
                line: *line,
                side: *side,
                comment_idx: *comment_idx,
            }),
            AnnotatedLine::RemoteThreadLine { thread_idx, .. } => {
                Some(CommentNavigatorKey::Remote {
                    thread_idx: *thread_idx,
                })
            }
            AnnotatedLine::RemoteReviewSummaryLine { summary_idx } => {
                Some(CommentNavigatorKey::RemoteReview {
                    summary_idx: *summary_idx,
                })
            }
            _ => None,
        }
    }

    fn comment_navigator_item_for_key(
        &self,
        key: CommentNavigatorKey,
        target_annotation: usize,
    ) -> Option<CommentNavigatorItem> {
        match key {
            CommentNavigatorKey::Review { comment_idx } => {
                let comment = self.session.review_comments.get(comment_idx)?;
                Some(CommentNavigatorItem {
                    key: CommentNavigatorKey::Review { comment_idx },
                    kind: CommentNavigatorKind::Local(
                        comment.comment_type.clone(),
                        comment.lifecycle_state,
                    ),
                    target_annotation,
                    path: None,
                    line: None,
                    side: None,
                    author: Some(comment.author.clone()),
                })
            }
            CommentNavigatorKey::File {
                file_idx,
                comment_idx,
            } => {
                let path = self.diff_files.get(file_idx)?.display_path();
                let review = self.session.files.get(path)?;
                let comment = review.file_comments.get(comment_idx)?;
                Some(CommentNavigatorItem {
                    key: CommentNavigatorKey::File {
                        file_idx,
                        comment_idx,
                    },
                    kind: CommentNavigatorKind::Local(
                        comment.comment_type.clone(),
                        comment.lifecycle_state,
                    ),
                    target_annotation,
                    path: Some(path.display().to_string()),
                    line: None,
                    side: None,
                    author: Some(comment.author.clone()),
                })
            }
            CommentNavigatorKey::Line {
                file_idx,
                line,
                side,
                comment_idx,
            } => {
                let path = self.diff_files.get(file_idx)?.display_path();
                let review = self.session.files.get(path)?;
                let comments = review.line_comments.get(&line)?;
                let comment = comments.get(comment_idx)?;
                Some(CommentNavigatorItem {
                    key: CommentNavigatorKey::Line {
                        file_idx,
                        line,
                        side,
                        comment_idx,
                    },
                    kind: CommentNavigatorKind::Local(
                        comment.comment_type.clone(),
                        comment.lifecycle_state,
                    ),
                    target_annotation,
                    path: Some(path.display().to_string()),
                    line: Some(line),
                    side: Some(side),
                    author: Some(comment.author.clone()),
                })
            }
            CommentNavigatorKey::Remote { thread_idx } => {
                let thread = self.forge_review_threads.get(thread_idx)?;
                let muted = self
                    .session
                    .remote_comments_visibility
                    .render_decision(thread)?;
                let side = match thread.side {
                    crate::forge::remote_comments::RemoteCommentSide::Right => LineSide::New,
                    crate::forge::remote_comments::RemoteCommentSide::Left => LineSide::Old,
                };
                let author = thread.root().and_then(|c| c.author.clone());
                Some(CommentNavigatorItem {
                    key: CommentNavigatorKey::Remote { thread_idx },
                    kind: CommentNavigatorKind::Remote { muted },
                    target_annotation,
                    path: Some(thread.path.clone()),
                    line: thread.line,
                    side: Some(side),
                    author,
                })
            }
            CommentNavigatorKey::RemoteReview { summary_idx } => {
                let summary = self.forge_review_summaries.get(summary_idx)?;
                Some(CommentNavigatorItem {
                    key: CommentNavigatorKey::RemoteReview { summary_idx },
                    kind: CommentNavigatorKind::Remote { muted: false },
                    target_annotation,
                    path: None,
                    line: None,
                    side: None,
                    author: summary.author.clone(),
                })
            }
        }
    }

    pub fn build_comment_navigator_items(&self) -> Vec<CommentNavigatorItem> {
        let mut items = Vec::new();
        let mut last_key: Option<CommentNavigatorKey> = None;

        for (idx, annotation) in self.line_annotations.iter().enumerate() {
            let Some(key) = Self::comment_navigator_key(annotation) else {
                last_key = None;
                continue;
            };

            if last_key.as_ref() == Some(&key) {
                continue;
            }

            if let Some(item) = self.comment_navigator_item_for_key(key.clone(), idx) {
                items.push(item);
                last_key = Some(key);
            }
        }

        items
    }

    pub fn has_comment_navigator_items(&self) -> bool {
        !self.build_comment_navigator_items().is_empty()
    }

    pub fn sync_comment_navigator_selection(&mut self, items: &[CommentNavigatorItem]) {
        if items.is_empty() {
            self.comment_navigator_state.list_state.select(None);
            return;
        }

        if self.focused_panel == FocusedPanel::Diff
            && let Some(annotation) = self.line_annotations.get(self.diff_state.cursor_line)
            && let Some(key) = Self::comment_navigator_key(annotation)
            && let Some(idx) = items.iter().position(|item| item.key == key)
        {
            self.comment_navigator_state.select(idx);
            return;
        }

        let selected = self
            .comment_navigator_state
            .selected()
            .min(items.len().saturating_sub(1));
        self.comment_navigator_state.select(selected);
    }

    pub fn comment_navigator_down(&mut self, n: usize) {
        let items = self.build_comment_navigator_items();
        let max_idx = items.len().saturating_sub(1);
        let new_idx = (self.comment_navigator_state.selected() + n).min(max_idx);
        self.comment_navigator_state.select(new_idx);
    }

    pub fn comment_navigator_up(&mut self, n: usize) {
        let new_idx = self.comment_navigator_state.selected().saturating_sub(n);
        self.comment_navigator_state.select(new_idx);
    }

    pub fn comment_navigator_viewport_scroll_down(&mut self, lines: usize) {
        let total = self.build_comment_navigator_items().len();
        let viewport = self.comment_navigator_state.viewport_height.max(1);
        let max_offset = total.saturating_sub(viewport);
        let new_offset = (self.comment_navigator_state.list_state.offset() + lines).min(max_offset);
        *self.comment_navigator_state.list_state.offset_mut() = new_offset;
        if self.comment_navigator_state.selected() < new_offset {
            self.comment_navigator_state.select(new_offset);
        }
    }

    pub fn comment_navigator_viewport_scroll_up(&mut self, lines: usize) {
        let viewport = self.comment_navigator_state.viewport_height.max(1);
        let new_offset = self
            .comment_navigator_state
            .list_state
            .offset()
            .saturating_sub(lines);
        *self.comment_navigator_state.list_state.offset_mut() = new_offset;
        let max_visible = (new_offset + viewport).saturating_sub(1);
        if self.comment_navigator_state.selected() > max_visible {
            self.comment_navigator_state.select(max_visible);
        }
    }

    pub fn jump_to_selected_comment(&mut self) -> bool {
        let items = self.build_comment_navigator_items();
        let Some(item) = items.get(self.comment_navigator_state.selected()) else {
            self.set_message("No comments to navigate");
            return false;
        };
        let file_idx = item.path.as_deref().and_then(|path| {
            self.diff_files
                .iter()
                .position(|file| file.display_path() == Path::new(path))
        });
        self.move_cursor_to_annotation(item.target_annotation);
        if let Some(file_idx) = file_idx {
            let file_changed = self.diff_state.current_file_idx != file_idx;
            self.diff_state.current_file_idx = file_idx;
            if self.is_single_file_view && file_changed {
                self.rebuild_annotations();
            }
        }
        self.center_cursor();
        self.focused_panel = FocusedPanel::Diff;
        true
    }

    /// Open a comment selected in the summary view in the continuous diff.
    /// Reviewed files and hunks are revealed without changing their persisted
    /// reviewed state.
    pub fn jump_to_summary_comment(&mut self, target: SummaryCommentTarget) -> bool {
        let mut target_file = None;
        let mut target_hunk = None;

        let resolved = match target {
            SummaryCommentTarget::Review { comment_id } => {
                let Some(comment_idx) = self
                    .session
                    .review_comments
                    .iter()
                    .position(|comment| comment.id == comment_id)
                else {
                    self.set_warning("That summary comment no longer exists");
                    return false;
                };
                SummaryAnnotationTarget::Review { comment_idx }
            }
            SummaryCommentTarget::File { path, comment_id } => {
                let Some(file_idx) = self
                    .diff_files
                    .iter()
                    .position(|file| file.display_path() == &path)
                else {
                    self.set_warning("That comment's file is no longer in the diff");
                    return false;
                };
                let Some(comment_idx) = self.session.files.get(&path).and_then(|review| {
                    review
                        .file_comments
                        .iter()
                        .position(|comment| comment.id == comment_id)
                }) else {
                    self.set_warning("That summary comment no longer exists");
                    return false;
                };
                target_file = Some((file_idx, path));
                SummaryAnnotationTarget::File {
                    file_idx,
                    comment_idx,
                }
            }
            SummaryCommentTarget::Line {
                path,
                line,
                side,
                comment_id,
            } => {
                let Some(file_idx) = self
                    .diff_files
                    .iter()
                    .position(|file| file.display_path() == &path)
                else {
                    self.set_warning("That comment's file is no longer in the diff");
                    return false;
                };
                let Some(comment_idx) = self
                    .session
                    .files
                    .get(&path)
                    .and_then(|review| review.line_comments.get(&line))
                    .and_then(|comments| {
                        comments.iter().position(|comment| comment.id == comment_id)
                    })
                else {
                    self.set_warning("That summary comment no longer exists");
                    return false;
                };

                target_hunk = self.diff_files[file_idx]
                    .hunks
                    .iter()
                    .position(|hunk| {
                        hunk.lines.iter().any(|diff_line| match side {
                            LineSide::Old => diff_line.old_lineno == Some(line),
                            LineSide::New => diff_line.new_lineno == Some(line),
                        })
                    })
                    .map(|hunk_idx| (file_idx, hunk_idx));
                target_file = Some((file_idx, path));
                SummaryAnnotationTarget::Line {
                    file_idx,
                    line,
                    side,
                    comment_idx,
                }
            }
        };

        let previous_file_idx = self.diff_state.current_file_idx;
        let previous_single_file_view = self.is_single_file_view;
        let previous_revealed_file = self.revealed_reviewed_file.clone();
        let previous_revealed_hunk = self.revealed_reviewed_hunk.clone();

        self.is_single_file_view = false;
        self.revealed_reviewed_file = None;
        self.revealed_reviewed_hunk = None;
        if let Some((file_idx, path)) = &target_file {
            self.diff_state.current_file_idx = *file_idx;
            if self.session.is_file_reviewed(path) {
                self.reveal_reviewed_file(*file_idx);
            }
        }
        if let Some((file_idx, hunk_idx)) = target_hunk
            && self.is_hunk_reviewed(file_idx, hunk_idx)
        {
            self.reveal_reviewed_hunk(file_idx, hunk_idx);
        }

        self.rebuild_annotations();
        let annotation_idx =
            self.line_annotations
                .iter()
                .position(|annotation| match (&resolved, annotation) {
                    (
                        SummaryAnnotationTarget::Review { comment_idx },
                        AnnotatedLine::ReviewComment {
                            comment_idx: candidate,
                        },
                    ) => comment_idx == candidate,
                    (
                        SummaryAnnotationTarget::File {
                            file_idx,
                            comment_idx,
                        },
                        AnnotatedLine::FileComment {
                            file_idx: candidate_file,
                            comment_idx: candidate_comment,
                        },
                    ) => file_idx == candidate_file && comment_idx == candidate_comment,
                    (
                        SummaryAnnotationTarget::Line {
                            file_idx,
                            line,
                            side,
                            comment_idx,
                        },
                        AnnotatedLine::LineComment {
                            file_idx: candidate_file,
                            line: candidate_line,
                            side: candidate_side,
                            comment_idx: candidate_comment,
                        },
                    ) => {
                        file_idx == candidate_file
                            && line == candidate_line
                            && side == candidate_side
                            && comment_idx == candidate_comment
                    }
                    _ => false,
                });

        let Some(annotation_idx) = annotation_idx else {
            self.diff_state.current_file_idx = previous_file_idx;
            self.is_single_file_view = previous_single_file_view;
            self.revealed_reviewed_file = previous_revealed_file;
            self.revealed_reviewed_hunk = previous_revealed_hunk;
            self.rebuild_annotations();
            self.set_warning("That comment is hidden by the current diff or filters");
            return false;
        };

        self.move_cursor_to_annotation(annotation_idx);
        self.center_cursor();
        self.focused_panel = FocusedPanel::Diff;
        self.exit_summary_mode();
        true
    }

    pub fn next_comment(&mut self) {
        let items = self.build_comment_navigator_items();
        if items.is_empty() {
            self.set_message("No comments");
            return;
        }

        let cursor = self
            .diff_state
            .cursor_line
            .min(self.line_annotations.len().saturating_sub(1));
        let target_idx = items
            .iter()
            .position(|item| item.target_annotation > cursor)
            .unwrap_or(0);

        self.comment_navigator_state.select(target_idx);
        self.jump_to_selected_comment();
        self.set_message(format!("Comment {}/{}", target_idx + 1, items.len()));
    }

    pub fn prev_comment(&mut self) {
        let items = self.build_comment_navigator_items();
        if items.is_empty() {
            self.set_message("No comments");
            return;
        }

        let cursor = self
            .diff_state
            .cursor_line
            .min(self.line_annotations.len().saturating_sub(1));
        let current_key = self
            .line_annotations
            .get(cursor)
            .and_then(Self::comment_navigator_key);
        let target_idx = items
            .iter()
            .rposition(|item| {
                item.target_annotation < cursor && Some(&item.key) != current_key.as_ref()
            })
            .unwrap_or(items.len() - 1);

        self.comment_navigator_state.select(target_idx);
        self.jump_to_selected_comment();
        self.set_message(format!("Comment {}/{}", target_idx + 1, items.len()));
    }

    /// True when the cursor sits on a local comment already pushed to the forge.
    /// Editing stays locked; deleting requires a successful remote API call.
    pub fn cursor_on_locked_comment(&self) -> bool {
        let Some(location) = self.find_comment_at_cursor() else {
            return false;
        };
        match location {
            CommentLocation::Review { index } => self
                .session
                .review_comments
                .get(index)
                .is_some_and(|c| c.is_locked()),
            CommentLocation::File { path, index } => self
                .session
                .files
                .get(&path)
                .and_then(|review| review.file_comments.get(index))
                .is_some_and(|c| c.is_locked()),
            CommentLocation::Line {
                path,
                line,
                side,
                index,
            } => self
                .session
                .files
                .get(&path)
                .and_then(|review| review.line_comments.get(&line))
                .and_then(|comments| {
                    let mut side_idx = 0;
                    for c in comments {
                        if c.side.unwrap_or(LineSide::New) == side {
                            if side_idx == index {
                                return Some(c);
                            }
                            side_idx += 1;
                        }
                    }
                    None
                })
                .is_some_and(|c| c.is_locked()),
        }
    }

    /// True when the cursor is on a fetched forge thread row.
    pub fn cursor_on_remote_thread(&self) -> bool {
        matches!(
            self.line_annotations.get(self.diff_state.cursor_line),
            Some(AnnotatedLine::RemoteThreadLine { .. })
        )
    }

    /// Find the comment at the current cursor position.
    pub(super) fn find_comment_at_cursor(&self) -> Option<CommentLocation> {
        let target = self.diff_state.cursor_line;
        let commit_set = self.selected_commit_set();
        match self.line_annotations.get(target) {
            Some(AnnotatedLine::ReviewComment { comment_idx }) => Some(CommentLocation::Review {
                index: *comment_idx,
            }),
            Some(AnnotatedLine::FileComment {
                file_idx,
                comment_idx,
            }) => {
                let path = self.diff_files.get(*file_idx)?.display_path().clone();
                // Guard against stale annotations from an async commit-selection
                // reload: if the comment is no longer visible under the current
                // selection, treat the cursor as not on a comment.
                let visible = self
                    .session
                    .files
                    .get(&path)
                    .and_then(|r| r.file_comments.get(*comment_idx))
                    .is_some_and(|c| Self::comment_visible_with(c, commit_set.as_ref()));
                if !visible {
                    return None;
                }
                Some(CommentLocation::File {
                    path,
                    index: *comment_idx,
                })
            }
            Some(AnnotatedLine::LineComment {
                file_idx,
                line,
                side,
                comment_idx,
            }) => {
                let path = self.diff_files.get(*file_idx)?.display_path().clone();
                let visible = self
                    .session
                    .files
                    .get(&path)
                    .and_then(|r| r.line_comments.get(line))
                    .and_then(|c| c.get(*comment_idx))
                    .is_some_and(|c| Self::comment_visible_with(c, commit_set.as_ref()));
                if !visible {
                    return None;
                }
                Some(CommentLocation::Line {
                    path,
                    line: *line,
                    side: *side,
                    index: *comment_idx,
                })
            }
            _ => None,
        }
    }

    /// Content of the comment at the current cursor position, if any.
    /// Resolves through the same lookup `dd` and `i` use, so `Y` yanks
    /// exactly the comment the cursor is sitting on.
    pub fn comment_content_at_cursor(&self) -> Option<String> {
        match self.find_comment_at_cursor()? {
            CommentLocation::Review { index } => self
                .session
                .review_comments
                .get(index)
                .map(|c| c.content.clone()),
            CommentLocation::File { path, index } => self
                .session
                .files
                .get(&path)
                .and_then(|review| review.file_comments.get(index))
                .map(|c| c.content.clone()),
            CommentLocation::Line {
                path,
                line,
                side,
                index,
            } => self
                .session
                .files
                .get(&path)
                .and_then(|review| review.line_comments.get(&line))
                .and_then(|comments| comments.get(index))
                // Same side guard as the delete path: the annotation index is
                // absolute into the stored Vec, so confirm we landed on the
                // side the cursor is actually showing.
                .filter(|c| c.side.unwrap_or(LineSide::New) == side)
                .map(|c| c.content.clone()),
        }
    }

    pub fn toggle_remote_thread_resolution(&mut self) -> bool {
        if self.forge_kind() != Some(crate::forge::traits::ForgeKind::GitHub) {
            self.set_message("Thread resolution is only available for GitHub PRs");
            return false;
        }
        let Some(AnnotatedLine::RemoteThreadLine { thread_idx, .. }) =
            self.line_annotations.get(self.diff_state.cursor_line)
        else {
            self.set_message("Move cursor to a remote review thread");
            return false;
        };
        let Some(thread) = self.forge_review_threads.get(*thread_idx) else {
            return false;
        };
        if self.pr_reaction_rx.is_some()
            || self.pr_edit_rx.is_some()
            || self.pr_thread_resolution_rx.is_some()
            || self.pr_reply_rx.is_some()
            || self.pr_delete_rx.is_some()
            || self.pr_threads_rx.is_some()
            || self.forge_review_threads_loading
        {
            self.set_message("Wait for the current thread operation to finish");
            return false;
        }
        let DiffSource::PullRequest(pr) = &self.diff_source else {
            return false;
        };
        let key = pr.key.clone();
        let thread_id = thread.id.clone();
        let resolved = !thread.is_resolved;
        let (tx, rx) = std::sync::mpsc::channel();
        self.pr_thread_resolution_rx = Some(rx);
        std::thread::spawn(move || {
            let backend = super::create_forge_backend(&key.repository, None, false, false);
            let result = backend
                .set_review_thread_resolved(&key.repository, &thread_id, resolved)
                .map_err(|e| e.to_string());
            let _ = tx.send(PrThreadResolutionEvent {
                key,
                thread_id,
                resolved,
                result,
            });
        });
        self.set_message(if resolved {
            "Resolving GitHub thread…"
        } else {
            "Reopening GitHub thread…"
        });
        true
    }

    pub fn poll_pr_thread_resolution_events(&mut self) {
        let Some(rx) = self.pr_thread_resolution_rx.as_ref() else {
            return;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pr_thread_resolution_rx = None;
                self.set_error("GitHub thread update failed: worker disconnected");
                self.refetch_pr_threads();
                return;
            }
        };
        self.pr_thread_resolution_rx = None;
        let DiffSource::PullRequest(pr) = &self.diff_source else {
            return;
        };
        if pr.key != event.key {
            return;
        }
        match event.result {
            Ok(()) => {
                if let Some(thread) = self
                    .forge_review_threads
                    .iter_mut()
                    .find(|t| t.id == event.thread_id)
                {
                    thread.is_resolved = event.resolved;
                    self.rebuild_annotations();
                    self.diff_state.cursor_line = self
                        .diff_state
                        .cursor_line
                        .min(self.line_annotations.len().saturating_sub(1));
                    self.set_message(if event.resolved {
                        "GitHub thread resolved"
                    } else {
                        "GitHub thread reopened"
                    });
                } else {
                    self.set_warning("Thread updated on GitHub; refresh to see its current state");
                }
                self.refetch_pr_threads();
            }
            Err(error) => {
                self.set_error(format!("GitHub thread update failed: {error}"));
                self.refetch_pr_threads();
            }
        }
    }

    /// Content of a remote review comment at the cursor.
    ///
    /// A remote thread is rendered after its anchor line, so accept both its
    /// rendered rows and the diff row it is attached to. Rendered rows retain
    /// their reply identity; an anchor line copies the thread's root comment.
    /// Review-level summaries are already their own annotated rows.
    pub fn remote_comment_content_at_cursor(&self) -> Option<String> {
        use crate::forge::remote_comments::RemoteCommentSide;

        let annotation = self.line_annotations.get(self.diff_state.cursor_line)?;
        let thread_idx = match annotation {
            AnnotatedLine::IssueComment { comment_idx } => {
                return self
                    .pr_info
                    .as_ref()
                    .and_then(|info| info.issue_comments.get(*comment_idx))
                    .map(|comment| comment.body.clone());
            }
            AnnotatedLine::RemoteReviewSummaryLine { summary_idx } => {
                return self
                    .forge_review_summaries
                    .get(*summary_idx)
                    .map(|summary| summary.body.clone());
            }
            AnnotatedLine::RemoteThreadLine {
                thread_idx,
                comment_idx,
            } => {
                return self
                    .forge_review_threads
                    .get(*thread_idx)
                    .and_then(|thread| thread.comments.get(*comment_idx))
                    .map(|comment| comment.body.clone());
            }
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
                let path = self
                    .diff_files
                    .get(*file_idx)?
                    .display_path()
                    .to_string_lossy();
                self.forge_review_threads.iter().position(|thread| {
                    thread.path == path
                        && self
                            .session
                            .remote_comments_visibility
                            .render_decision(thread)
                            .is_some()
                        && match thread.side {
                            RemoteCommentSide::Left => thread.line == *old_lineno,
                            RemoteCommentSide::Right => thread.line == *new_lineno,
                        }
                })
            }
            _ => None,
        }?;

        self.forge_review_threads
            .get(thread_idx)
            .and_then(|thread| thread.root())
            .map(|comment| comment.body.clone())
    }

    pub(super) fn local_comment_at(&self, location: &CommentLocation) -> Option<&Comment> {
        match location {
            CommentLocation::Review { index } => self.session.review_comments.get(*index),
            CommentLocation::File { path, index } => {
                self.session.files.get(path)?.file_comments.get(*index)
            }
            CommentLocation::Line {
                path,
                line,
                side,
                index,
            } => self
                .session
                .files
                .get(path)?
                .line_comments
                .get(line)?
                .get(*index)
                .filter(|c| c.side.unwrap_or(LineSide::New) == *side),
        }
    }

    pub fn delete_comment_at_cursor(&mut self) -> bool {
        if self.pr_delete_rx.is_some() || self.pr_edit_rx.is_some() || self.pr_reply_rx.is_some() {
            self.set_message("Wait for the current GitHub operation");
            return false;
        }
        if self.pr_submit_rx.is_some()
            || self.pending_comment_rx.is_some()
            || !self.pending_comment_queue.is_empty()
        {
            self.set_warning("Wait for the review submission before deleting a comment");
            return false;
        }
        if let Some(location) = self.find_comment_at_cursor() {
            let Some(comment) = self.local_comment_at(&location) else {
                return false;
            };
            if comment.is_locked() {
                let Some(viewer) = self.pr_viewer_login.as_deref() else {
                    self.set_warning("Cannot verify the current forge user; comment not deleted");
                    return false;
                };
                let DiffSource::PullRequest(pr) = &self.diff_source else {
                    return false;
                };
                if pr.key.repository.kind != crate::forge::traits::ForgeKind::GitHub {
                    self.set_warning("Remote comment deletion is not supported by this forge");
                    return false;
                }
                let review_id = comment.remote_review_id.clone();
                let remote_comment_id = comment.remote_comment_id.clone();
                let remote_body = match &location {
                    CommentLocation::Review { .. } => comment.content.clone(),
                    CommentLocation::Line { .. } => crate::forge::submit::build_inline_body(
                        comment,
                        false,
                        crate::forge::submit::SubmitContext::new(
                            &self.forge_config,
                            &self.comment_types,
                        ),
                    ),
                    CommentLocation::File { .. } => crate::forge::submit::build_inline_body(
                        comment,
                        true,
                        crate::forge::submit::SubmitContext::new(
                            &self.forge_config,
                            &self.comment_types,
                        ),
                    ),
                };
                let matched = match &location {
                    CommentLocation::Review { .. } => self
                        .forge_review_summaries
                        .iter()
                        .find(|s| {
                            s.state == crate::forge::remote_comments::RemoteReviewState::Pending
                                && s.author
                                    .as_deref()
                                    .is_some_and(|a| a.eq_ignore_ascii_case(viewer))
                                && review_id.as_deref() == s.database_id.as_deref()
                                && s.body == remote_body
                                && self
                                    .session
                                    .review_comments
                                    .iter()
                                    .filter(|c| c.remote_review_id == review_id && c.is_locked())
                                    .count()
                                    == 1
                        })
                        .map(|s| RemoteDeleteTarget::Review(s.id.clone())),
                    CommentLocation::Line {
                        path, line, side, ..
                    } => self
                        .forge_review_threads
                        .iter()
                        .filter(|thread| {
                            thread.path == path.to_string_lossy()
                                && thread.line == Some(*line)
                                && (thread.side
                                    == crate::forge::remote_comments::RemoteCommentSide::Left)
                                    == (*side == LineSide::Old)
                        })
                        .flat_map(|thread| &thread.comments)
                        .find(|remote| {
                            remote
                                .author
                                .as_deref()
                                .is_some_and(|a| a.eq_ignore_ascii_case(viewer))
                                && remote.body == remote_body
                                && remote.review_database_id.as_deref() == review_id.as_deref()
                                && remote_comment_id
                                    .as_deref()
                                    .is_none_or(|id| remote.id == id)
                        })
                        .map(|remote| RemoteDeleteTarget::Comment(remote.id.clone())),
                    CommentLocation::File { path, .. } => self
                        .forge_review_threads
                        .iter()
                        .filter(|thread| thread.path == path.to_string_lossy())
                        .flat_map(|thread| &thread.comments)
                        .find(|remote| {
                            remote
                                .author
                                .as_deref()
                                .is_some_and(|a| a.eq_ignore_ascii_case(viewer))
                                && remote.body == remote_body
                                && remote.review_database_id.as_deref() == review_id.as_deref()
                                && remote_comment_id
                                    .as_deref()
                                    .is_none_or(|id| remote.id == id)
                        })
                        .map(|remote| RemoteDeleteTarget::Comment(remote.id.clone())),
                };
                let target = matched.or_else(|| {
                    remote_comment_id.and_then(|id| {
                        self.forge_review_threads
                            .iter()
                            .flat_map(|thread| &thread.comments)
                            .find(|remote| {
                                remote.id == id
                                    && remote
                                        .author
                                        .as_deref()
                                        .is_some_and(|a| a.eq_ignore_ascii_case(viewer))
                            })
                            .map(|_| RemoteDeleteTarget::Comment(id))
                    })
                });
                let Some(target) = target else {
                    self.set_warning("Unable to identify your pending GitHub comment; refresh with :e before deleting (submitted review summaries cannot be deleted)");
                    return false;
                };
                return self.confirm_remote_comment_delete(target, Some(comment.id.clone()));
            }
            if self.session.remove_comment(&location) {
                self.dirty = true;
                self.set_message("Comment deleted");
                self.rebuild_annotations();
                return true;
            }
            return false;
        }
        let annotation = self.line_annotations.get(self.diff_state.cursor_line);
        let remote = match annotation {
            Some(AnnotatedLine::RemoteThreadLine {
                thread_idx,
                comment_idx,
            }) => self
                .forge_review_threads
                .get(*thread_idx)
                .and_then(|t| t.comments.get(*comment_idx))
                .map(|c| (c.id.clone(), c.author.clone(), false)),
            Some(AnnotatedLine::RemoteReviewSummaryLine { summary_idx }) => {
                let summary = self.forge_review_summaries.get(*summary_idx);
                if summary.is_some_and(|s| {
                    s.state != crate::forge::remote_comments::RemoteReviewState::Pending
                }) {
                    self.set_warning("GitHub does not permit deleting submitted review summaries");
                    return false;
                }
                summary.map(|s| (s.id.clone(), s.author.clone(), true))
            }
            _ => None,
        };
        let Some((id, author, review)) = remote else {
            self.set_message("No comment at cursor");
            return false;
        };
        let Some(viewer) = self.pr_viewer_login.as_deref() else {
            self.set_warning("Cannot verify the current forge user; comment not deleted");
            return false;
        };
        if !author
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(viewer))
        {
            self.set_warning("Only your own comments can be deleted");
            return false;
        }
        let target = if review {
            RemoteDeleteTarget::Review(id)
        } else {
            RemoteDeleteTarget::Comment(id)
        };
        self.confirm_remote_comment_delete(target, None)
    }

    fn confirm_remote_comment_delete(
        &mut self,
        target: RemoteDeleteTarget,
        local_id: Option<String>,
    ) -> bool {
        if self.forge_kind() != Some(crate::forge::traits::ForgeKind::GitHub) {
            self.set_warning("Remote comment deletion is not supported by this forge");
            return false;
        }
        self.enter_confirm_mode(ConfirmAction::DeleteRemoteComment { target, local_id });
        true
    }

    pub fn start_remote_comment_delete(
        &mut self,
        target: RemoteDeleteTarget,
        local_id: Option<String>,
    ) -> bool {
        let DiffSource::PullRequest(pr) = &self.diff_source else {
            return false;
        };
        if pr.key.repository.kind != crate::forge::traits::ForgeKind::GitHub {
            self.set_warning("Remote comment deletion is not supported by this forge");
            return false;
        }
        let Some(expected_viewer) = self.pr_viewer_login.clone() else {
            self.set_warning("Cannot verify the current forge user; comment not deleted");
            return false;
        };
        if self.pr_submit_rx.is_some()
            || self.pending_comment_rx.is_some()
            || !self.pending_comment_queue.is_empty()
            || self.pr_delete_rx.is_some()
            || self.pr_edit_rx.is_some()
            || self.pr_reply_rx.is_some()
            || self.pr_thread_resolution_rx.is_some()
            || self.pr_reaction_rx.is_some()
        {
            self.set_warning("Wait for the current GitHub operation to finish");
            return false;
        }
        let repository = pr.key.repository.clone();
        let pr_number = pr.key.number;
        let head_sha = pr.key.head_sha.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.pr_delete_rx = Some(rx);
        std::thread::spawn(move || {
            let backend = super::create_forge_backend(&repository, None, false, false);
            let result = backend
                .current_viewer(&repository)
                .and_then(|actual| match actual {
                    Some(actual) if actual.eq_ignore_ascii_case(&expected_viewer) => Ok(()),
                    _ => Err(TuicrError::Forge(
                        "GitHub account changed; refresh the PR before deleting".into(),
                    )),
                })
                .and_then(|()| match &target {
                    RemoteDeleteTarget::Review(id) => backend.delete_review(&repository, id),
                    RemoteDeleteTarget::Comment(id) => {
                        backend.delete_review_comment(&repository, id)
                    }
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(PrDeleteEvent::Done {
                repository,
                pr_number,
                head_sha,
                target,
                local_id,
                expected_viewer,
                result,
            });
        });
        self.set_message("Deleting comment from GitHub…");
        true
    }

    pub fn poll_pr_delete_events(&mut self) {
        let Some(rx) = self.pr_delete_rx.as_ref() else {
            return;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pr_delete_rx = None;
                self.set_warning("Comment deletion failed: worker disconnected");
                return;
            }
        };
        self.pr_delete_rx = None;
        let PrDeleteEvent::Done {
            repository,
            pr_number,
            head_sha,
            target,
            local_id,
            expected_viewer,
            result,
        } = event;
        let DiffSource::PullRequest(pr) = &self.diff_source else {
            return;
        };
        if pr.key.repository != repository
            || pr.key.number != pr_number
            || pr.key.head_sha != head_sha
            || self.pr_viewer_login.as_deref() != Some(expected_viewer.as_str())
        {
            return;
        }
        match result {
            Ok(()) => {
                if let Some(local_id) = local_id
                    && let Some(location) = self.session.find_comment_by_id(&local_id)
                    && self
                        .session
                        .remove_comment_matching(&location, |comment| comment.id == local_id)
                {
                    self.dirty = true;
                }
                match target {
                    RemoteDeleteTarget::Comment(id) => {
                        for thread in &mut self.forge_review_threads {
                            thread.comments.retain(|c| c.id != id);
                        }
                        self.forge_review_threads.retain(|t| !t.comments.is_empty());
                    }
                    RemoteDeleteTarget::Review(id) => {
                        self.forge_review_summaries.retain(|s| s.id != id);
                    }
                }
                self.rebuild_annotations();
                if let Err(error) = self.save_current_session_merging_external() {
                    self.set_warning(format!(
                        "Comment deleted on GitHub but session save failed: {error}"
                    ));
                } else {
                    self.set_message("Comment deleted from GitHub");
                }
                self.refetch_pr_threads();
            }
            Err(error) => {
                self.set_warning(format!("Comment deletion failed: {error}"));
                self.refetch_pr_threads();
            }
        }
    }

    pub fn clear_comments(&mut self, scope: ClearScope) {
        if self.pending_comment_rx.is_some() || !self.pending_comment_queue.is_empty() {
            self.set_warning("Wait for pending GitHub comments to finish before clearing");
            return;
        }
        let (cleared, unreviewed) = self.session.clear_comments(scope);
        if cleared == 0 && unreviewed == 0 {
            self.set_message("No comments to clear");
            return;
        }

        self.dirty = true;
        self.rebuild_annotations();
        let msg = match (cleared, unreviewed) {
            (0, n) => format!("Unreviewed {n} files"),
            (c, 0) => format!("Cleared {c} comments"),
            (c, n) => format!("Cleared {c} comments, unreviewed {n} files"),
        };
        self.set_message(msg);
    }

    /// True if two annotation rows belong to the same rendered comment.
    /// `AnnotatedLine` is not `Eq`, so compare the identifying fields.
    fn same_comment(a: &AnnotatedLine, b: &AnnotatedLine) -> bool {
        use AnnotatedLine::{FileComment, LineComment, ReviewComment};
        match (a, b) {
            (ReviewComment { comment_idx: x }, ReviewComment { comment_idx: y }) => x == y,
            (
                FileComment {
                    file_idx: f1,
                    comment_idx: c1,
                },
                FileComment {
                    file_idx: f2,
                    comment_idx: c2,
                },
            ) => f1 == f2 && c1 == c2,
            (
                LineComment {
                    file_idx: f1,
                    line: l1,
                    side: s1,
                    comment_idx: c1,
                },
                LineComment {
                    file_idx: f2,
                    line: l2,
                    side: s2,
                    comment_idx: c2,
                },
            ) => f1 == f2 && l1 == l2 && s1 == s2 && c1 == c2,
            _ => false,
        }
    }

    /// First annotation row of the comment rendered at `cursor_line` (or
    /// `cursor_line` itself when it isn't on a comment).
    pub(in crate::app) fn comment_block_start(&self, cursor_line: usize) -> usize {
        let Some(cur) = self.line_annotations.get(cursor_line) else {
            return cursor_line;
        };
        let mut start = cursor_line;
        while start > 0
            && self
                .line_annotations
                .get(start - 1)
                .is_some_and(|prev| Self::same_comment(prev, cur))
        {
            start -= 1;
        }
        start
    }

    /// Byte offset in the loaded `comment_buffer` for the start
    /// (`cursor_at_end == false`) or end of the comment line the diff cursor is
    /// on. The comment's block begins at annotation row `block_start` (row 0 =
    /// top border, then one row per wrapped segment, then the bottom border).
    pub(in crate::app) fn comment_current_line_cursor(
        &self,
        block_start: usize,
        cursor_at_end: bool,
    ) -> usize {
        let content = &self.comment_buffer;
        let content_area = self.diff_state.viewport_width.saturating_sub(10);
        // Visual content row under the cursor (skip the top border at row 0).
        let visual_target = self
            .diff_state
            .cursor_line
            .saturating_sub(block_start)
            .saturating_sub(1);

        let mut visual = 0usize;
        let mut byte = 0usize;
        let mut line_start = 0usize;
        let mut line_len = 0usize;
        for line in content.split('\n') {
            line_start = byte;
            line_len = line.len();
            let segs = crate::ui::comment_panel::wrap_segments(line, content_area)
                .len()
                .max(1);
            if visual_target < visual + segs {
                return if cursor_at_end {
                    line_start + line_len
                } else {
                    line_start
                };
            }
            visual += segs;
            byte += line.len() + 1;
        }
        // Cursor on the bottom border or past the content: use the last line.
        if cursor_at_end {
            line_start + line_len
        } else {
            line_start
        }
    }

    /// Enter edit mode for the comment at the current cursor position.
    /// `cursor_at_end` places the text cursor at the end of the current comment
    /// line (vim `A` / the default non-vim behavior); otherwise at its start
    /// (vim `i`). Returns true if a comment was found and edit mode entered.
    pub fn enter_edit_mode(&mut self, cursor_at_end: bool) -> bool {
        if self.pending_comment_rx.is_some() || !self.pending_comment_queue.is_empty() {
            self.set_warning("Wait for the GitHub comment to finish before editing");
            return false;
        }
        let location = self.find_comment_at_cursor();
        // First annotation row of the comment under the cursor, so we can place
        // the text cursor on the line the diff cursor is actually pointing at.
        let block_start = self.comment_block_start(self.diff_state.cursor_line);

        match location {
            Some(CommentLocation::Review { index }) => {
                if let Some(comment) = self.session.review_comments.get(index) {
                    self.input_mode = InputMode::Comment;
                    self.diff_state.scroll_x = 0;
                    self.comment_buffer = comment.content.clone();
                    self.comment_cursor =
                        self.comment_current_line_cursor(block_start, cursor_at_end);
                    self.comment_type = comment.comment_type.clone();
                    self.comment_is_review_level = true;
                    self.comment_is_file_level = false;
                    self.comment_line = None;
                    self.editing_comment_id = Some(comment.id.clone());
                    self.reply_thread_id = None;
                    return true;
                }
            }
            Some(CommentLocation::File { path, index }) => {
                if let Some(review) = self.session.files.get(&path)
                    && let Some(comment) = review.file_comments.get(index)
                {
                    self.input_mode = InputMode::Comment;
                    self.diff_state.scroll_x = 0;
                    self.comment_buffer = comment.content.clone();
                    self.comment_cursor =
                        self.comment_current_line_cursor(block_start, cursor_at_end);
                    self.comment_type = comment.comment_type.clone();
                    self.comment_is_review_level = false;
                    self.comment_is_file_level = true;
                    self.comment_line = None;
                    self.editing_comment_id = Some(comment.id.clone());
                    self.reply_thread_id = None;
                    return true;
                }
            }
            Some(CommentLocation::Line {
                path,
                line,
                side,
                index,
            }) => {
                if let Some(review) = self.session.files.get(&path)
                    && let Some(comments) = review.line_comments.get(&line)
                {
                    // `comment_idx` from the annotation is the absolute index
                    // into the stored Vec (see `push_comments`); look it up
                    // directly, verifying the side matches.
                    if let Some(comment) = comments.get(index)
                        && comment.side.unwrap_or(LineSide::New) == side
                    {
                        self.input_mode = InputMode::Comment;
                        self.diff_state.scroll_x = 0;
                        self.comment_buffer = comment.content.clone();
                        self.comment_cursor =
                            self.comment_current_line_cursor(block_start, cursor_at_end);
                        self.comment_type = comment.comment_type.clone();
                        self.comment_is_review_level = false;
                        self.comment_is_file_level = false;
                        self.comment_line = Some((line, side));
                        self.editing_comment_id = Some(comment.id.clone());
                        self.reply_thread_id = None;
                        return true;
                    }
                }
            }
            None => {}
        }

        false
    }

    /// Start a reply only when the cursor is on a visible remote thread row.
    pub fn enter_thread_reply_mode(&mut self) -> bool {
        let Some(AnnotatedLine::RemoteThreadLine { thread_idx, .. }) =
            self.line_annotations.get(self.diff_state.cursor_line)
        else {
            return false;
        };
        let Some(thread) = self.forge_review_threads.get(*thread_idx) else {
            return false;
        };
        if self.forge_kind() != Some(crate::forge::traits::ForgeKind::GitHub) {
            self.set_warning("Replying to threads is currently supported only on GitHub");
            return true;
        }
        if let DiffSource::PullRequest(pr) = &self.diff_source
            && let Some(reason) = pr.read_only_reason()
        {
            self.set_warning(format!("Cannot reply: PR is {reason}"));
            return true;
        }
        let thread_id = thread.id.clone();
        self.enter_comment_mode(false, None);
        if self
            .failed_thread_reply
            .as_ref()
            .is_some_and(|(id, _)| id == &thread_id)
        {
            let (_, text) = self.failed_thread_reply.take().unwrap();
            self.comment_cursor = text.len();
            self.comment_buffer = text;
        }
        self.reply_thread_id = Some(thread_id);
        self.rebuild_annotations();
        true
    }

    pub fn enter_comment_mode(&mut self, file_level: bool, line: Option<(u32, LineSide)>) {
        self.input_mode = InputMode::Comment;
        if self.diff_view_mode != DiffViewMode::SideBySide {
            self.diff_state.scroll_x = 0;
        }
        self.comment_buffer.clear();
        self.comment_cursor = 0;
        self.comment_type = self.default_comment_type();
        self.comment_is_review_level = false;
        self.comment_is_file_level = file_level;
        self.comment_line = line;
        self.comment_line_range = None;
        self.editing_comment_id = None;
        self.reply_thread_id = None;
    }

    pub fn enter_review_comment_mode(&mut self) {
        self.input_mode = InputMode::Comment;
        self.diff_state.scroll_x = 0;
        self.comment_buffer.clear();
        self.comment_cursor = 0;
        self.comment_type = self.default_comment_type();
        self.comment_is_review_level = true;
        self.comment_is_file_level = false;
        self.comment_line = None;
        self.comment_line_range = None;
        self.editing_comment_id = None;
        self.reply_thread_id = None;
    }

    pub fn exit_comment_mode(&mut self) {
        self.input_mode = InputMode::Normal;
        self.comment_buffer.clear();
        self.comment_cursor = 0;
        self.comment_vim_editor = None;
        self.comment_vim_command = None;
        self.comment_vim_pending = CommentVimPending::None;
        self.comment_is_review_level = false;
        self.editing_comment_id = None;
        self.reply_thread_id = None;
        self.comment_line_range = None;
        self.rebuild_annotations();
    }

    pub fn save_comment(&mut self) {
        if self.comment_buffer.trim().is_empty() {
            self.set_message("Comment cannot be empty");
            return;
        }

        let content = self.comment_buffer.trim().to_string();
        if let Some(thread_id) = self.reply_thread_id.clone() {
            self.start_thread_reply(thread_id, content);
            return;
        }

        let mut message = "Error: Could not save comment".to_string();
        let mut autosave_error = None;
        let mut new_comment = None;
        let mut new_target = None;

        // Check if we're editing an existing comment
        if let Some(editing_id) = &self.editing_comment_id {
            if let Some(comment) = self
                .session
                .review_comments
                .iter_mut()
                .find(|c| &c.id == editing_id)
            {
                comment.content = content.clone();
                comment.comment_type = self.comment_type.clone();
                message = "Review comment updated".to_string();
            } else if let Some(path) = self.current_file_path().cloned()
                && let Some(review) = self.session.get_file_mut(&path)
            {
                if let Some(comment) = review
                    .file_comments
                    .iter_mut()
                    .find(|c| &c.id == editing_id)
                {
                    comment.content = content.clone();
                    comment.comment_type = self.comment_type.clone();
                    message = "Comment updated".to_string();
                } else {
                    // If not found in file comments, search in line comments
                    let mut found_comment = None;
                    for comments in review.line_comments.values_mut() {
                        if let Some(comment) = comments.iter_mut().find(|c| &c.id == editing_id) {
                            found_comment = Some(comment);
                            break;
                        }
                    }

                    if let Some(comment) = found_comment {
                        comment.content = content.clone();
                        comment.comment_type = self.comment_type.clone();
                        message = if let Some((line, _)) = self.comment_line {
                            format!("Comment on line {line} updated")
                        } else {
                            "Comment updated".to_string()
                        };
                    } else {
                        message = "Error: Comment to edit not found".to_string();
                    }
                }
            }
        } else if self.comment_is_review_level {
            let request = AddCommentRequest {
                target: CommentTarget::Review,
                content,
                comment_type: self.comment_type.clone(),
                author: self.username.clone(),
                commit_id: None,
            };
            message = match add_comment_to_session(&mut self.session, request) {
                Ok(_) => "Review comment added".to_string(),
                Err(e) => format!("Error: Could not save comment: {e}"),
            };
        } else if let Some(path) = self.current_file_path().cloned() {
            let (target, success_message) = if self.comment_is_file_level {
                (
                    CommentTarget::File { path },
                    "File comment added".to_string(),
                )
            } else if let Some((range, side)) = self.comment_line_range {
                let message = if range.is_single() {
                    format!("Comment added to line {}", range.end)
                } else {
                    format!("Comment added to lines {}-{}", range.start, range.end)
                };
                (CommentTarget::LineRange { path, range, side }, message)
            } else if let Some((line, side)) = self.comment_line {
                (
                    CommentTarget::Line { path, line, side },
                    format!("Comment added to line {line}"),
                )
            } else {
                (
                    CommentTarget::File { path },
                    "File comment added".to_string(),
                )
            };

            let request = AddCommentRequest {
                target,
                content,
                comment_type: self.comment_type.clone(),
                author: self.username.clone(),
                commit_id: self.commit_id_for_new_comment(),
            };
            new_target = Some(request.target.clone());
            message = match add_comment_to_session(&mut self.session, request) {
                Ok(comment) => {
                    new_comment = Some(comment);
                    success_message
                }
                Err(e) => format!("Error: Could not save comment: {e}"),
            };
        }

        if !message.starts_with("Error:") {
            self.dirty = true;
            if let Err(e) = self.save_current_session_merging_external() {
                autosave_error = Some(format!("{message}; autosave failed: {e}"));
            }
        }
        if let Some(error) = autosave_error {
            self.set_error(error);
        } else {
            self.set_message(message);
        }
        self.rebuild_annotations();
        if let (Some(comment), Some(target)) = (new_comment, new_target) {
            self.start_pending_comment(comment, target);
        }

        self.exit_comment_mode();
    }

    fn start_pending_comment(&mut self, comment: Comment, target: CommentTarget) {
        use crate::forge::submit::{CommentAnchor, MappedComment, SubmitContext, map_comment};
        use crate::forge::traits::ForgeKind;
        let DiffSource::PullRequest(pr) = &self.diff_source else {
            return;
        };
        if pr.key.repository.kind != ForgeKind::GitHub || pr.is_read_only() {
            return;
        }
        let path = match &target {
            CommentTarget::File { path }
            | CommentTarget::Line { path, .. }
            | CommentTarget::LineRange { path, .. } => path,
            CommentTarget::Review => return,
        };
        let files = self.range_diff_files.as_ref().unwrap_or(&self.diff_files);
        let Some(file) = files.iter().find(|file| file.display_path() == path) else {
            return;
        };
        let anchor = match target {
            CommentTarget::File { .. } => CommentAnchor::FileLevel,
            CommentTarget::Line { line, side, .. } => CommentAnchor::Line { line, side },
            CommentTarget::LineRange { .. } => CommentAnchor::Range,
            CommentTarget::Review => return,
        };
        let MappedComment::Inline(inline) = map_comment(
            &comment,
            anchor,
            file,
            SubmitContext::new(&self.forge_config, &self.comment_types),
        ) else {
            self.set_warning("Comment saved locally; it cannot be anchored on GitHub");
            return;
        };
        if self.pr_submit_rx.is_some() {
            self.set_warning("Comment saved locally; review submission is in progress");
            return;
        }
        let key = pr.key.clone();
        self.pr_threads_epoch += 1;
        let commit_id = match self.commit_selection_range {
            Some((start, end))
                if start <= end
                    && end < self.pr_commits.len()
                    && !(start == 0 && end + 1 == self.pr_commits.len()) =>
            {
                self.pr_commits[start].oid.clone()
            }
            _ => key.head_sha.clone(),
        };
        let item = PendingCommentQueueItem {
            key,
            commit_id,
            comment_id: comment.id.clone(),
            inline,
        };
        if self.pending_comment_rx.is_some() {
            self.pending_comment_queue.push_back(item);
            self.set_message("Comment queued for pending GitHub review");
        } else {
            self.dispatch_pending_comment(item);
        }
    }

    fn dispatch_pending_comment(&mut self, item: PendingCommentQueueItem) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending_comment_rx = Some(rx);
        std::thread::spawn(move || {
            let backend = create_forge_backend(&item.key.repository, None, false, false);
            let result = backend
                .add_pending_comment(
                    &item.key.repository,
                    item.key.number,
                    &item.commit_id,
                    &item.inline,
                )
                .map_err(|error| error.to_string());
            let _ = tx.send(PendingCommentEvent {
                key: item.key,
                comment_id: item.comment_id,
                result,
            });
        });
    }

    pub fn poll_pending_comment_events(&mut self) {
        let Some(rx) = self.pending_comment_rx.as_ref() else {
            return;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pending_comment_rx = None;
                self.set_error("GitHub comment worker terminated unexpectedly (saved locally)");
                self.drain_next_pending_comment();
                return;
            }
        };
        self.pending_comment_rx = None;
        if matches!(&self.diff_source, DiffSource::PullRequest(pr) if pr.key == event.key) {
            match event.result {
                Ok(review_id) => {
                    self.viewer_has_pending_review = true;
                    for review in self.session.files.values_mut() {
                        for comment in review.file_comments.iter_mut().chain(
                            review
                                .line_comments
                                .values_mut()
                                .flat_map(|comments| comments.iter_mut()),
                        ) {
                            if comment.id == event.comment_id {
                                comment.lifecycle_state =
                                    crate::model::comment::CommentLifecycleState::PushedDraft;
                                comment.remote_review_id = Some(review_id.to_string());
                            }
                        }
                    }
                    let _ = self.save_current_session_merging_external();
                    self.rebuild_annotations();
                    self.set_message("Comment added to pending GitHub review");
                }
                Err(error) => {
                    self.set_error(format!("GitHub comment failed (saved locally): {error}"));
                }
            }
        }
        self.drain_next_pending_comment();
    }

    fn drain_next_pending_comment(&mut self) {
        while let Some(next_item) = self.pending_comment_queue.pop_front() {
            if matches!(&self.diff_source, DiffSource::PullRequest(pr) if pr.key == next_item.key) {
                self.dispatch_pending_comment(next_item);
                break;
            }
        }
    }

    fn start_thread_reply(&mut self, thread_id: String, body: String) {
        let DiffSource::PullRequest(pr) = &self.diff_source else {
            self.set_warning("Thread reply requires a pull request");
            return;
        };
        if let Some(reason) = pr.read_only_reason() {
            self.set_warning(format!("Cannot reply: PR is {reason}"));
            return;
        }
        if self.pr_reply_rx.is_some()
            || self.pr_edit_rx.is_some()
            || self.pr_threads_rx.is_some()
            || self.pr_submit_rx.is_some()
            || self.pending_comment_rx.is_some()
            || !self.pending_comment_queue.is_empty()
            || self.pr_delete_rx.is_some()
            || self.pr_thread_resolution_rx.is_some()
            || self.pr_reaction_rx.is_some()
        {
            self.set_warning("Wait for the current GitHub operation to finish");
            return;
        }
        if !self.forge_review_threads.iter().any(|t| t.id == thread_id) {
            self.set_warning("Thread changed; refresh before replying");
            return;
        }
        let repository = pr.key.repository.clone();
        let pr_number = pr.key.number;
        let head_sha = pr.key.head_sha.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.pr_reply_rx = Some(rx);
        self.pending_thread_reply = Some((thread_id.clone(), body.clone()));
        self.exit_comment_mode();
        std::thread::spawn(move || {
            let backend = super::create_forge_backend(&repository, None, false, false);
            let result = backend
                .reply_to_review_thread(&repository, &thread_id, &body)
                .map_err(|e| e.to_string());
            let _ = tx.send(PrReplyEvent::Done {
                repository,
                pr_number,
                head_sha,
                thread_id,
                result,
            });
        });
        self.set_message("Posting thread reply to GitHub…");
    }

    pub fn poll_pr_reply_events(&mut self) {
        let Some(rx) = self.pr_reply_rx.as_ref() else {
            return;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pr_reply_rx = None;
                self.failed_thread_reply = self.pending_thread_reply.take();
                self.set_warning("Thread reply failed: worker disconnected; refresh to verify");
                return;
            }
        };
        self.pr_reply_rx = None;
        let pending_reply = self.pending_thread_reply.take();
        let PrReplyEvent::Done {
            repository,
            pr_number,
            head_sha,
            thread_id,
            result,
        } = event;
        let DiffSource::PullRequest(pr) = &self.diff_source else {
            return;
        };
        if pr.key.repository != repository
            || pr.key.number != pr_number
            || pr.key.head_sha != head_sha
        {
            return;
        }
        match result {
            Ok(comment) => {
                if self
                    .failed_thread_reply
                    .as_ref()
                    .is_some_and(|(id, _)| id == &thread_id)
                {
                    self.failed_thread_reply = None;
                }
                if let Some(thread) = self
                    .forge_review_threads
                    .iter_mut()
                    .find(|t| t.id == thread_id)
                {
                    if !thread.comments.iter().any(|c| c.id == comment.id) {
                        thread.comments.push(comment);
                    }
                    self.rebuild_annotations();
                    self.set_message("Thread reply posted");
                } else {
                    self.set_warning("Thread reply posted; refresh to see it");
                }
            }
            Err(error) => {
                self.failed_thread_reply = pending_reply.filter(|(id, _)| id == &thread_id);
                self.set_error(format!("Thread reply failed: {error}"));
            }
        }
    }

    pub fn cycle_comment_type(&mut self) {
        if self.comment_types.is_empty() {
            return;
        }
        if self.comment_types.len() == 1 {
            self.set_message("Only one comment type configured");
            return;
        }

        let current_id = self.comment_type.id().to_string();
        let current_index = self
            .comment_types
            .iter()
            .position(|comment_type| comment_type.id == current_id)
            .unwrap_or(0);
        let next_index = (current_index + 1) % self.comment_types.len();
        let next_id = self.comment_types[next_index].id.clone();
        self.comment_type = CommentType::from_id(&next_id);
        self.announce_comment_type();
    }

    pub fn cycle_comment_type_reverse(&mut self) {
        if self.comment_types.is_empty() {
            return;
        }
        if self.comment_types.len() == 1 {
            self.set_message("Only one comment type configured");
            return;
        }

        let current_id = self.comment_type.id();
        let current_index = self
            .comment_types
            .iter()
            .position(|comment_type| comment_type.id == current_id)
            .unwrap_or(0);
        let prev_index = if current_index == 0 {
            self.comment_types.len() - 1
        } else {
            current_index - 1
        };
        self.comment_type = CommentType::from_id(&self.comment_types[prev_index].id);
        self.announce_comment_type();
    }

    /// Emit a status message naming the current comment type. `None` has an
    /// empty label, so fall back to its id (`none`) for legible feedback.
    fn announce_comment_type(&mut self) {
        let comment_type = self.comment_type.clone();
        let label = self.comment_type_label(&comment_type);
        let display = if label.is_empty() {
            comment_type.id().to_string()
        } else {
            label
        };
        self.set_message(format!("Comment type: {display}"));
    }
}
