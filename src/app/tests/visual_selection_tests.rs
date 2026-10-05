use crate::app::*;
use crate::model::FileStatus;
use crate::vcs::traits::VcsType;

fn p(idx: usize, off: usize) -> SelPoint {
    SelPoint {
        annotation_idx: idx,
        char_offset: off,
        side: LineSide::New,
    }
}

#[test]
fn collapsed_starts_at_point() {
    let sel = VisualSelection::collapsed(p(5, 3));
    assert_eq!(sel.anchor, p(5, 3));
    assert_eq!(sel.head, p(5, 3));
}

#[test]
fn ordered_returns_anchor_head_when_already_in_order() {
    let sel = VisualSelection {
        anchor: p(1, 0),
        head: p(4, 8),
    };
    let (start, end) = sel.ordered();
    assert_eq!(start, p(1, 0));
    assert_eq!(end, p(4, 8));
}

#[test]
fn ordered_swaps_when_head_before_anchor_by_idx() {
    let sel = VisualSelection {
        anchor: p(4, 0),
        head: p(1, 0),
    };
    let (start, end) = sel.ordered();
    assert_eq!(start, p(1, 0));
    assert_eq!(end, p(4, 0));
}

#[test]
fn ordered_breaks_ties_on_idx_by_char_offset() {
    let sel = VisualSelection {
        anchor: p(7, 20),
        head: p(7, 5),
    };
    let (start, end) = sel.ordered();
    assert_eq!(start, p(7, 5));
    assert_eq!(end, p(7, 20));
}

struct DummyVcs {
    info: VcsInfo,
}

impl VcsBackend for DummyVcs {
    fn info(&self) -> &VcsInfo {
        &self.info
    }
    fn get_working_tree_diff(&self, _highlighter: &SyntaxHighlighter) -> Result<Vec<DiffFile>> {
        Err(TuicrError::NoChanges)
    }
    fn fetch_context_lines(
        &self,
        _file_path: &Path,
        _file_status: FileStatus,
        _ref_commit: Option<&str>,
        _start_line: u32,
        _end_line: u32,
    ) -> Result<Vec<DiffLine>> {
        Ok(Vec::new())
    }
    fn file_line_count(
        &self,
        _file_path: &Path,
        _file_status: FileStatus,
        _ref_commit: Option<&str>,
    ) -> Result<u32> {
        Ok(0)
    }
}

fn build_app() -> App {
    let vcs_info = VcsInfo {
        root_path: PathBuf::from("/tmp"),
        head_commit: "head".to_string(),
        branch_name: Some("main".to_string()),
        vcs_type: VcsType::Git,
    };
    let session = ReviewSession::new(
        vcs_info.root_path.clone(),
        vcs_info.head_commit.clone(),
        vcs_info.branch_name.clone(),
        SessionDiffSource::WorkingTree,
    );
    App::build(
        Box::new(DummyVcs {
            info: vcs_info.clone(),
        }),
        vcs_info,
        Theme::dark(),
        None,
        false,
        Vec::new(),
        session,
        DiffSource::WorkingTree,
        InputMode::Normal,
        Vec::new(),
        None,
        None,
    )
    .expect("failed to build test app")
}

#[test]
fn visual_selection_can_start_inside_rendered_review_comment() {
    let mut app = build_app();
    app.diff_state.viewport_width = 60;
    app.session.review_comments.push(crate::model::Comment::new(
        "first line\nsecond line".to_string(),
        CommentType::from_id("note"),
        None,
    ));
    app.rebuild_annotations();
    let rows: Vec<_> = app
        .line_annotations
        .iter()
        .enumerate()
        .filter_map(|(idx, ann)| matches!(ann, AnnotatedLine::ReviewComment { .. }).then_some(idx))
        .collect();
    let idx = rows[2];
    app.diff_state.cursor_line = idx;
    crate::handler::handle_diff_action(&mut app, crate::input::Action::EnterVisualMode);
    assert_eq!(app.input_mode, InputMode::VisualSelect);
    let text = app.content_for_side(idx, LineSide::New).unwrap();
    assert!(text.contains("second line"), "{text}");
    assert!(!text.contains("first line"));
    assert_eq!(
        app.visual_selection.unwrap().head.char_offset,
        text.chars().count()
    );
    app.diff_state.cursor_line = rows[1];
    app.extend_visual_to_cursor();
    assert_eq!(
        app.visual_selection.unwrap().ordered().0.annotation_idx,
        rows[1]
    );
    let geom = app.selection_geometry(idx, ratatui::layout::Rect::new(0, 0, 60, 10), LineSide::Old);
    assert_eq!(geom.content_x_start, 1);
    assert_eq!(geom.content_width, 59);
}

#[test]
fn comment_selection_maps_unicode_characters_to_terminal_cells() {
    let mut app = build_app();
    app.diff_state.viewport_width = 60;
    app.session.review_comments.push(crate::model::Comment::new(
        "a界b👩‍🔬👍🏽".to_string(),
        CommentType::from_id("note"),
        None,
    ));
    app.rebuild_annotations();
    let idx = app
        .line_annotations
        .iter()
        .enumerate()
        .find_map(|(idx, _)| {
            app.rendered_comment_text(idx)
                .filter(|text| text.contains("a界b"))
                .map(|_| idx)
        })
        .unwrap();
    let text = app.rendered_comment_text(idx).unwrap();
    let offset = text.chars().position(|ch| ch == '界').unwrap();
    let cells = app.comment_selection_cells(idx, 60).unwrap();
    assert_eq!(cells[offset].2, 2);
    assert_eq!(cells[offset + 1].1, cells[offset].1 + 2);
    let emoji = text.chars().position(|ch| ch == '👩').unwrap();
    assert_eq!(cells[emoji].2, 2);
    assert_eq!(cells[emoji], cells[emoji + 2]);
    app.diff_state.scroll_x = 2;
    app.diff_state.wrap_lines = false;
    let scrolled = app.comment_selection_cells(idx, 60).unwrap();
    assert_eq!(scrolled[2].1, 1);
}

#[test]
fn copying_comment_selection_omits_box_decoration() {
    let mut app = build_app();
    app.diff_state.viewport_width = 60;
    app.session.review_comments.push(crate::model::Comment::new(
        "first\nsecond".into(),
        CommentType::from_id("note"),
        None,
    ));
    app.rebuild_annotations();
    let rows: Vec<_> = app
        .line_annotations
        .iter()
        .enumerate()
        .filter_map(|(idx, ann)| matches!(ann, AnnotatedLine::ReviewComment { .. }).then_some(idx))
        .collect();
    app.diff_state.cursor_line = rows[0];
    app.enter_visual_mode_at_cursor();
    app.diff_state.cursor_line = *rows.last().unwrap();
    app.extend_visual_to_cursor();
    assert_eq!(app.visual_selection_text(), "first\nsecond");
    app.line_annotations.insert(
        0,
        AnnotatedLine::DiffLine {
            file_idx: 0,
            hunk_idx: 0,
            line_idx: 0,
            old_lineno: None,
            new_lineno: Some(1),
        },
    );
    app.visual_selection = Some(VisualSelection {
        anchor: p(0, 0),
        head: p(rows.last().unwrap() + 1, 100),
    });
    assert!(!app.visual_selection_text().contains("first"));
}
