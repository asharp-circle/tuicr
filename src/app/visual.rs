use super::*;

impl App {
    pub fn rendered_comment_text(&self, idx: usize) -> Option<String> {
        use crate::ui::{comment_panel, diff_view, pr_info_panel};
        let annotation = self.line_annotations.get(idx)?;
        let width = self.diff_state.viewport_width.saturating_sub(1);
        let local = match annotation {
            AnnotatedLine::ReviewComment { comment_idx } => {
                self.session.review_comments.get(*comment_idx)
            }
            AnnotatedLine::FileComment {
                file_idx,
                comment_idx,
            } => self
                .session
                .files
                .get(self.diff_files.get(*file_idx)?.display_path())?
                .file_comments
                .get(*comment_idx),
            AnnotatedLine::LineComment {
                file_idx,
                line,
                comment_idx,
                ..
            } => self
                .session
                .files
                .get(self.diff_files.get(*file_idx)?.display_path())?
                .line_comments
                .get(line)?
                .get(*comment_idx),
            _ => None,
        };
        let (lines, row) = if let Some(comment) = local {
            let row = idx - self.comment_block_start(idx);
            (
                comment_panel::format_comment_lines(
                    &self.theme,
                    diff_view::comment_type_presentation(self, &comment.comment_type),
                    &comment.content,
                    match annotation {
                        AnnotatedLine::LineComment { line, .. } => {
                            Some(comment.line_range.unwrap_or(LineRange::single(*line)))
                        }
                        _ => None,
                    },
                    width,
                    (comment.author != self.username).then_some(comment.author.as_str()),
                    Some(comment.lifecycle_state),
                ),
                row,
            )
        } else {
            let row = self.line_annotations[..idx]
                .iter()
                .rev()
                .take_while(|a| match (a, annotation) {
                    (
                        AnnotatedLine::IssueComment { comment_idx: a },
                        AnnotatedLine::IssueComment { comment_idx: b },
                    ) => a == b,
                    (
                        AnnotatedLine::RemoteThreadLine { thread_idx: a, .. },
                        AnnotatedLine::RemoteThreadLine { thread_idx: b, .. },
                    ) => a == b,
                    (
                        AnnotatedLine::RemoteReviewSummaryLine { summary_idx: a },
                        AnnotatedLine::RemoteReviewSummaryLine { summary_idx: b },
                    ) => a == b,
                    _ => false,
                })
                .count();
            let lines = match annotation {
                AnnotatedLine::IssueComment { comment_idx } => {
                    pr_info_panel::format_issue_comment_lines(
                        &self.theme,
                        self.pr_info.as_ref()?.issue_comments.get(*comment_idx)?,
                        width,
                        &diff_view::comment_type_presentation(self, &CommentType::from_id("note")),
                    )
                }
                AnnotatedLine::RemoteThreadLine { thread_idx, .. } => {
                    let thread = self.forge_review_threads.get(*thread_idx)?;
                    comment_panel::format_remote_thread_lines(
                        &self.theme,
                        thread,
                        self.session
                            .remote_comments_visibility
                            .render_decision(thread)
                            .unwrap_or(false),
                        self.forge_kind(),
                        self.diff_state.viewport_width,
                    )
                }
                AnnotatedLine::RemoteReviewSummaryLine { summary_idx } => {
                    comment_panel::format_remote_review_summary_lines(
                        &self.theme,
                        self.forge_review_summaries.get(*summary_idx)?,
                        self.diff_state.viewport_width,
                        self.forge_kind(),
                    )
                }
                _ => return None,
            };
            (lines, row)
        };
        Some(
            lines
                .get(row)?
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect(),
        )
    }

    pub(crate) fn comment_selection_cells(
        &self,
        idx: usize,
        width: usize,
    ) -> Option<Vec<(usize, usize, usize)>> {
        use ratatui::text::Span;
        use unicode_segmentation::UnicodeSegmentation;
        use unicode_width::UnicodeWidthStr;
        let text = self.rendered_comment_text(idx)?;
        let scroll = if self.diff_view_mode == DiffViewMode::Unified && !self.diff_state.wrap_lines
        {
            self.diff_state.scroll_x
        } else {
            0
        };
        let visible: String = text.chars().skip(scroll).collect();
        let spans = [Span::raw(format!(" {visible}"))];
        let rows = if self.diff_state.wrap_lines {
            crate::ui::text_utils::wrap_spans(&spans, width)
        } else {
            vec![spans.to_vec()]
        };
        let mut cells = vec![(0, 0, 0); scroll.min(text.chars().count())];
        let mut indicator = true;
        for (row, spans) in rows.iter().enumerate() {
            let mut col = 0;
            let row_text: String = spans.iter().map(|span| span.content.as_ref()).collect();
            for grapheme in row_text.graphemes(true) {
                let w = grapheme.width();
                if indicator {
                    indicator = false;
                } else {
                    cells.extend(std::iter::repeat_n((row, col, w), grapheme.chars().count()));
                }
                col += w;
            }
        }
        Some(cells)
    }

    pub(crate) fn visual_selection_text(&self) -> String {
        let Some(sel) = self.visual_selection else {
            return String::new();
        };
        let (start, end) = sel.ordered();
        let side = sel.anchor.side;
        let skip_comments = matches!(
            self.line_annotations.get(sel.anchor.annotation_idx),
            Some(
                AnnotatedLine::DiffLine { .. }
                    | AnnotatedLine::SideBySideLine { .. }
                    | AnnotatedLine::ExpandedContext { .. }
            )
        );
        let mut snippets = Vec::new();
        for idx in start.annotation_idx..=end.annotation_idx {
            let snippet = if let Some(content) = self.content_for_side(idx, side) {
                let total = content.chars().count();
                let (mut lo, hi) = sel.char_range(idx, total);
                if self.rendered_comment_text(idx).is_some() {
                    if skip_comments {
                        continue;
                    }
                    let Some(body) = content.strip_prefix("    │  ") else {
                        continue;
                    };
                    let prefix_len = total - body.chars().count();
                    lo = lo.max(prefix_len);
                    if hi <= lo {
                        continue;
                    }
                }
                char_slice(&content, lo, Some(hi)).to_string()
            } else if let Some(text) = self.atomic_text_for_annotation(idx) {
                text
            } else {
                continue;
            };
            snippets.push(snippet);
        }
        snippets.join("\n")
    }

    pub fn copy_visual_selection(&mut self) -> Result<usize> {
        let out = self.visual_selection_text();
        if out.is_empty() {
            return Ok(0);
        }
        let count = out.chars().count();
        crate::output::copy_text_to_clipboard(&out)
            .map_err(|e| TuicrError::Clipboard(format!("{e}")))?;
        Ok(count)
    }

    pub fn enter_visual_mode_at_cursor(&mut self) {
        let idx = self.diff_state.cursor_line;
        let side = self
            .get_line_at_cursor()
            .map(|(_, s)| s)
            .unwrap_or(LineSide::New);
        let len = self.annotation_content_len(idx, side);
        let anchor = SelPoint {
            annotation_idx: idx,
            char_offset: 0,
            side,
        };
        let head = SelPoint {
            annotation_idx: idx,
            char_offset: len,
            side,
        };
        self.input_mode = InputMode::VisualSelect;
        self.visual_selection = Some(VisualSelection { anchor, head });
    }

    pub fn exit_visual_mode(&mut self) {
        self.input_mode = InputMode::Normal;
        self.visual_selection = None;
    }

    pub fn get_visual_selection(&self) -> Option<&VisualSelection> {
        if self.input_mode != InputMode::VisualSelect {
            return None;
        }
        self.visual_selection.as_ref()
    }

    pub fn annotation_content_len(&self, idx: usize, side: LineSide) -> usize {
        self.content_for_side(idx, side)
            .map(|s| s.chars().count())
            .unwrap_or(0)
    }

    pub fn extend_visual_to_cursor(&mut self) {
        let Some(sel) = self.visual_selection else {
            return;
        };
        let anchor_idx = sel.anchor.annotation_idx;
        let cursor_idx = self.diff_state.cursor_line;
        let side = sel.anchor.side;
        let anchor_len = self.annotation_content_len(anchor_idx, side);
        let cursor_len = self.annotation_content_len(cursor_idx, side);
        let (anchor_char, head_char) = if cursor_idx >= anchor_idx {
            (0, cursor_len)
        } else {
            (anchor_len, 0)
        };
        self.visual_selection = Some(VisualSelection {
            anchor: SelPoint {
                annotation_idx: anchor_idx,
                char_offset: anchor_char,
                side,
            },
            head: SelPoint {
                annotation_idx: cursor_idx,
                char_offset: head_char,
                side,
            },
        });
    }

    pub fn visual_selection_line_range(&self) -> Option<(LineRange, LineSide)> {
        let sel = self.get_visual_selection()?;
        let (start, end) = sel.ordered();
        let start_line = self.annotation_line_for_side(start.annotation_idx, start.side);
        let end_line = self.annotation_line_for_side(end.annotation_idx, end.side);
        let start_ln = start_line?;
        let end_ln = end_line?;
        Some((LineRange::new(start_ln, end_ln), start.side))
    }

    fn annotation_line_for_side(&self, idx: usize, side: LineSide) -> Option<u32> {
        match self.line_annotations.get(idx)? {
            AnnotatedLine::DiffLine {
                old_lineno,
                new_lineno,
                ..
            }
            | AnnotatedLine::SideBySideLine {
                old_lineno,
                new_lineno,
                ..
            } => match side {
                LineSide::New => *new_lineno,
                LineSide::Old => *old_lineno,
            },
            _ => None,
        }
    }

    pub fn enter_comment_from_visual(&mut self) {
        if let Some((range, side)) = self.visual_selection_line_range() {
            self.comment_line_range = Some((range, side));
            self.comment_line = Some((range.end, side));
            self.input_mode = InputMode::Comment;
            if self.diff_view_mode != DiffViewMode::SideBySide {
                self.diff_state.scroll_x = 0;
            }
            self.comment_buffer.clear();
            self.comment_cursor = 0;
            self.comment_type = self.default_comment_type();
            self.comment_is_review_level = false;
            self.comment_is_file_level = false;
            self.reply_thread_id = None;
            self.editing_comment_id = None;
            self.visual_selection = None;
        } else {
            self.set_warning("Invalid visual selection");
            self.exit_visual_mode();
        }
    }
}
