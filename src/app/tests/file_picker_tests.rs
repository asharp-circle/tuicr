use crate::app::file_picker::fuzzy_match;
use crate::app::*;
use crate::handler::handle_file_picker_action;
use crate::input::keybindings::{Action, map_file_picker_mode};
use crate::model::{DiffFile, DiffHunk, DiffLine, FileStatus, LineOrigin};
use crate::vcs::traits::{VcsBackend, VcsInfo, VcsType};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;

struct StubVcs(VcsInfo);
impl VcsBackend for StubVcs {
    fn info(&self) -> &VcsInfo {
        &self.0
    }
    fn get_working_tree_diff(
        &self,
        _hl: &crate::syntax::SyntaxHighlighter,
    ) -> crate::error::Result<Vec<DiffFile>> {
        Ok(Vec::new())
    }
    fn fetch_context_lines(
        &self,
        _path: &std::path::Path,
        _status: FileStatus,
        _ref_commit: Option<&str>,
        _start: u32,
        _end: u32,
    ) -> crate::error::Result<Vec<DiffLine>> {
        Ok(Vec::new())
    }
    fn file_line_count(
        &self,
        _path: &std::path::Path,
        _status: FileStatus,
        _ref_commit: Option<&str>,
    ) -> crate::error::Result<u32> {
        Ok(0)
    }
}

fn dummy_diff_file(path: &str) -> DiffFile {
    let hunk = DiffHunk {
        header: "@@ -0,0 +1,1 @@".to_string(),
        lines: vec![DiffLine {
            origin: LineOrigin::Addition,
            content: "hello".to_string(),
            old_lineno: None,
            new_lineno: Some(1),
            highlighted_spans: None,
        }],
        old_start: 1,
        old_count: 0,
        new_start: 1,
        new_count: 1,
    };
    let content_hash = DiffFile::compute_content_hash(std::slice::from_ref(&hunk));
    DiffFile {
        old_path: None,
        new_path: Some(PathBuf::from(path)),
        status: FileStatus::Modified,
        hunks: vec![hunk],
        is_binary: false,
        is_too_large: false,
        is_commit_message: false,
        content_hash,
    }
}

fn app_with_files(diff_files: Vec<DiffFile>) -> App {
    let vcs_info = VcsInfo {
        root_path: PathBuf::from("/tmp"),
        head_commit: "head".into(),
        branch_name: Some("main".into()),
        vcs_type: VcsType::Git,
    };
    let session = ReviewSession::new(
        vcs_info.root_path.clone(),
        vcs_info.head_commit.clone(),
        vcs_info.branch_name.clone(),
        SessionDiffSource::WorkingTree,
    );
    App::build(
        Box::new(StubVcs(vcs_info.clone())),
        vcs_info,
        crate::theme::Theme::dark(),
        None,
        false,
        diff_files,
        session,
        DiffSource::WorkingTree,
        InputMode::Normal,
        Vec::new(),
        None,
        None,
    )
    .expect("build app")
}

#[test]
fn fuzzy_match_empty_pattern() {
    let res = fuzzy_match("", "src/main.rs");
    assert!(res.is_some());
    let (score, indices) = res.unwrap();
    assert_eq!(score, 0);
    assert!(indices.is_empty());
}

#[test]
fn fuzzy_match_subsequence() {
    let res = fuzzy_match("fp", "src/app/file_picker.rs");
    assert!(res.is_some());
    let (_, indices) = res.unwrap();
    // 'f' in file_picker and 'p' in picker
    assert_eq!(indices, vec![8, 13]);
}

#[test]
fn fuzzy_match_smart_case() {
    // Lowercase matches case-insensitively
    assert!(fuzzy_match("main", "src/Main.rs").is_some());
    // Uppercase matches case-sensitively
    assert!(fuzzy_match("Main", "src/Main.rs").is_some());
    assert!(fuzzy_match("MAIN", "src/main.rs").is_none());
}

#[test]
fn fuzzy_match_filename_preference() {
    // Matching in filename should score higher than matching only in directory
    let (score_file, _) = fuzzy_match("app", "src/app_runner.rs").unwrap();
    let (score_dir, _) = fuzzy_match("app", "src/app/foo.rs").unwrap();
    assert!(score_file > score_dir);
}

#[test]
fn enter_file_picker_empty_warns() {
    let mut app = app_with_files(Vec::new());
    app.enter_file_picker_mode();
    assert_eq!(app.input_mode, InputMode::Normal);
    assert!(app.message.is_some());
}

#[test]
fn enter_and_exit_file_picker() {
    let mut app = app_with_files(vec![
        dummy_diff_file("src/main.rs"),
        dummy_diff_file("src/app/mod.rs"),
    ]);

    app.enter_file_picker_mode();
    assert_eq!(app.input_mode, InputMode::FilePicker);
    assert_eq!(app.file_picker.candidates.len(), 2);
    assert_eq!(app.file_picker.matches.len(), 2);

    app.cancel_file_picker();
    assert_eq!(app.input_mode, InputMode::Normal);
}

#[test]
fn file_picker_filter_and_confirm() {
    let mut app = app_with_files(vec![
        dummy_diff_file("src/main.rs"),
        dummy_diff_file("src/lib.rs"),
        dummy_diff_file("src/ui/app_layout.rs"),
    ]);

    app.enter_file_picker_mode();
    app.file_picker_insert_char('l');
    app.file_picker_insert_char('i');
    app.file_picker_insert_char('b');

    assert_eq!(app.file_picker.matches.len(), 1);
    assert_eq!(app.file_picker.selected_file_idx(), Some(1));

    app.confirm_file_picker();
    assert_eq!(app.input_mode, InputMode::Normal);
    assert_eq!(app.diff_state.current_file_idx, 1);
}

#[test]
fn file_picker_navigation_and_clear() {
    let mut app = app_with_files(vec![
        dummy_diff_file("src/a.rs"),
        dummy_diff_file("src/b.rs"),
        dummy_diff_file("src/c.rs"),
    ]);

    app.enter_file_picker_mode();
    assert_eq!(app.file_picker.selected(), 0);

    handle_file_picker_action(&mut app, Action::CursorDown(1));
    assert_eq!(app.file_picker.selected(), 1);

    handle_file_picker_action(&mut app, Action::CursorDown(10));
    assert_eq!(app.file_picker.selected(), 2);

    handle_file_picker_action(&mut app, Action::CursorUp(1));
    assert_eq!(app.file_picker.selected(), 1);

    handle_file_picker_action(&mut app, Action::InsertChar('z'));
    assert_eq!(app.file_picker.matches.len(), 0);

    handle_file_picker_action(&mut app, Action::ClearLine);
    assert_eq!(app.file_picker.matches.len(), 3);
}

#[test]
fn file_picker_key_mappings() {
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(map_file_picker_mode(esc), Action::ExitMode);

    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(map_file_picker_mode(ctrl_c), Action::ExitMode);

    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(map_file_picker_mode(enter), Action::SubmitInput);

    let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(map_file_picker_mode(backspace), Action::DeleteChar);

    let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(map_file_picker_mode(ctrl_u), Action::ClearLine);

    let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(map_file_picker_mode(down), Action::CursorDown(1));

    let ctrl_j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
    assert_eq!(map_file_picker_mode(ctrl_j), Action::CursorDown(1));

    let ctrl_k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
    assert_eq!(map_file_picker_mode(ctrl_k), Action::CursorUp(1));

    let char_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
    assert_eq!(map_file_picker_mode(char_a), Action::InsertChar('a'));
}
