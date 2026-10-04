//! Fuzzy file picker modal state and scoring (`InputMode::FilePicker`).
//!
//! Allows quick navigation across all diff files in the active review (PR or
//! local) using an interactive fuzzy matching modal.

use super::*;
use crate::model::FileStatus;

#[derive(Debug, Clone)]
pub struct FilePickerCandidate {
    pub file_idx: usize,
    pub path: String,
    pub filename: String,
    pub status: FileStatus,
    pub is_reviewed: bool,
}

#[derive(Debug, Clone)]
pub struct FilePickerMatch {
    pub candidate_idx: usize,
    pub score: i32,
    pub matched_indices: Vec<usize>,
}

#[derive(Default)]
pub struct FilePickerState {
    pub candidates: Vec<FilePickerCandidate>,
    pub matches: Vec<FilePickerMatch>,
    pub query: String,
    pub list_state: ratatui::widgets::ListState,
}

impl FilePickerState {
    pub fn update_matches(&mut self) {
        let q = self.query.trim();
        if q.is_empty() {
            self.matches = self
                .candidates
                .iter()
                .enumerate()
                .map(|(candidate_idx, _)| FilePickerMatch {
                    candidate_idx,
                    score: 0,
                    matched_indices: Vec::new(),
                })
                .collect();
        } else {
            let mut matches: Vec<FilePickerMatch> = self
                .candidates
                .iter()
                .enumerate()
                .filter_map(|(candidate_idx, candidate)| {
                    fuzzy_match(q, &candidate.path).map(|(score, matched_indices)| {
                        FilePickerMatch {
                            candidate_idx,
                            score,
                            matched_indices,
                        }
                    })
                })
                .collect();
            // Sort matches by score descending; on tie, sort by shorter path then candidate index
            matches.sort_by(|a, b| {
                b.score
                    .cmp(&a.score)
                    .then_with(|| {
                        self.candidates[a.candidate_idx]
                            .path
                            .len()
                            .cmp(&self.candidates[b.candidate_idx].path.len())
                    })
                    .then_with(|| a.candidate_idx.cmp(&b.candidate_idx))
            });
            self.matches = matches;
        }
    }

    pub fn selected(&self) -> usize {
        self.list_state.selected().unwrap_or(0)
    }

    pub fn select(&mut self, index: usize) {
        if self.matches.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(index.min(self.matches.len() - 1)));
        }
    }

    pub fn selected_file_idx(&self) -> Option<usize> {
        let idx = self.list_state.selected()?;
        let m = self.matches.get(idx)?;
        Some(self.candidates[m.candidate_idx].file_idx)
    }
}

impl App {
    /// Open the file picker: populate all diff files from the active review
    /// and pre-select the currently active file if possible.
    pub fn enter_file_picker_mode(&mut self) {
        if self.diff_files.is_empty() {
            self.set_warning("No files to navigate");
            return;
        }

        let candidates: Vec<FilePickerCandidate> = self
            .diff_files
            .iter()
            .enumerate()
            .map(|(file_idx, file)| {
                let path_buf = file.display_path();
                let path_str = path_buf.to_string_lossy().to_string();
                let filename = path_buf
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&path_str)
                    .to_string();
                let is_reviewed = self.session.is_file_reviewed(path_buf);
                FilePickerCandidate {
                    file_idx,
                    path: path_str,
                    filename,
                    status: file.status,
                    is_reviewed,
                }
            })
            .collect();

        self.file_picker.candidates = candidates;
        self.file_picker.query.clear();
        self.file_picker.update_matches();

        let initial_idx = self
            .file_picker
            .matches
            .iter()
            .position(|m| {
                self.file_picker.candidates[m.candidate_idx].file_idx
                    == self.diff_state.current_file_idx
            })
            .unwrap_or(0);
        self.file_picker.select(initial_idx);
        self.input_mode = InputMode::FilePicker;
    }

    pub fn cancel_file_picker(&mut self) {
        self.exit_file_picker_mode();
    }

    pub fn confirm_file_picker(&mut self) {
        let target = self.file_picker.selected_file_idx();
        self.exit_file_picker_mode();
        if let Some(idx) = target {
            self.jump_to_file(idx);
        }
    }

    fn exit_file_picker_mode(&mut self) {
        self.input_mode = InputMode::Normal;
        self.file_picker.candidates.clear();
        self.file_picker.matches.clear();
        self.file_picker.query.clear();
        self.file_picker.list_state = ratatui::widgets::ListState::default();
    }

    pub fn move_file_picker_selection(&mut self, delta: isize) {
        let len = self.file_picker.matches.len();
        if len == 0 {
            return;
        }
        let current = self.file_picker.selected() as isize;
        let next = (current + delta).clamp(0, len as isize - 1);
        self.file_picker.select(next as usize);
    }

    pub fn file_picker_insert_char(&mut self, ch: char) {
        self.file_picker.query.push(ch);
        self.file_picker.update_matches();
        self.file_picker.select(0);
    }

    pub fn file_picker_paste(&mut self, text: &str) {
        for ch in text.chars() {
            if ch != '\r' && ch != '\n' {
                self.file_picker.query.push(ch);
            }
        }
        self.file_picker.update_matches();
        self.file_picker.select(0);
    }

    pub fn file_picker_delete_char(&mut self) {
        if self.file_picker.query.pop().is_some() {
            self.file_picker.update_matches();
            self.file_picker.select(0);
        }
    }

    pub fn file_picker_clear_query(&mut self) {
        if !self.file_picker.query.is_empty() {
            self.file_picker.query.clear();
            self.file_picker.update_matches();
            self.file_picker.select(0);
        }
    }
}

/// Compute fuzzy match score and matched character indices in `target`.
///
/// Returns `Some((score, matched_indices))` if all characters in `pattern`
/// appear in sequence in `target`, else `None`.
pub fn fuzzy_match(pattern: &str, target: &str) -> Option<(i32, Vec<usize>)> {
    let pattern_trimmed = pattern.trim();
    if pattern_trimmed.is_empty() {
        return Some((0, Vec::new()));
    }

    let pattern_chars: Vec<char> = pattern_trimmed.chars().collect();
    let target_chars: Vec<char> = target.chars().collect();
    let m = pattern_chars.len();
    let n = target_chars.len();
    if m > n {
        return None;
    }

    let is_case_sensitive = pattern_chars.iter().any(|c| c.is_uppercase());
    let char_match = |p: char, t: char| -> bool {
        if is_case_sensitive {
            p == t
        } else {
            p.to_ascii_lowercase() == t.to_ascii_lowercase()
        }
    };

    // Quick subsequence verification
    let mut pi = 0;
    for &tc in &target_chars {
        if pi < m && char_match(pattern_chars[pi], tc) {
            pi += 1;
        }
    }
    if pi < m {
        return None;
    }

    let last_slash_idx = target_chars.iter().rposition(|&c| c == '/' || c == '\\');
    let filename_start = last_slash_idx.map_or(0, |i| i + 1);

    // Dynamic programming for best score and parent alignment
    let mut dp: Vec<Vec<Option<i32>>> = vec![vec![None; n]; m];
    let mut parent: Vec<Vec<Option<usize>>> = vec![vec![None; n]; m];

    // Initialize first row
    let p0 = pattern_chars[0];
    for j in 0..n {
        let tj = target_chars[j];
        if char_match(p0, tj) {
            let is_boundary = if j == 0 {
                true
            } else {
                let prev = target_chars[j - 1];
                matches!(prev, '/' | '\\' | '_' | '-' | '.' | ' ')
                    || (prev.is_ascii_lowercase() && tj.is_ascii_uppercase())
            };
            let boundary_bonus = if is_boundary { 30 } else { 0 };
            let in_filename = j >= filename_start;
            let filename_bonus = if in_filename { 20 } else { 0 };
            let is_prefix = j == 0 || j == filename_start;
            let prefix_bonus = if is_prefix { 40 } else { 0 };

            let dist = if in_filename { j - filename_start } else { j };
            let score = 10 + boundary_bonus + filename_bonus + prefix_bonus - (dist as i32).min(20);
            dp[0][j] = Some(score);
        }
    }

    // Fill remaining rows
    for i in 1..m {
        let pi_char = pattern_chars[i];
        for j in i..n {
            let tj = target_chars[j];
            if !char_match(pi_char, tj) {
                continue;
            }

            let is_boundary = {
                let prev = target_chars[j - 1];
                matches!(prev, '/' | '\\' | '_' | '-' | '.' | ' ')
                    || (prev.is_ascii_lowercase() && tj.is_ascii_uppercase())
            };
            let boundary_bonus = if is_boundary { 30 } else { 0 };
            let in_filename = j >= filename_start;
            let filename_bonus = if in_filename { 20 } else { 0 };

            let mut best_prev_k = None;
            let mut best_prev_score = i32::MIN;

            for k in (i - 1)..j {
                if let Some(prev_score) = dp[i - 1][k] {
                    let mut step_score = prev_score + 10 + boundary_bonus + filename_bonus;
                    if k + 1 == j {
                        step_score += 25; // Consecutive match bonus
                    } else {
                        let gap = (j - 1 - k) as i32;
                        step_score -= (gap * 2).min(30);
                    }

                    if step_score > best_prev_score {
                        best_prev_score = step_score;
                        best_prev_k = Some(k);
                    }
                }
            }

            if let Some(k) = best_prev_k {
                dp[i][j] = Some(best_prev_score);
                parent[i][j] = Some(k);
            }
        }
    }

    // Find best ending score in row m - 1
    let mut best_final_j = None;
    let mut best_final_score = i32::MIN;
    for j in (m - 1)..n {
        if let Some(score) = dp[m - 1][j] {
            if score > best_final_score {
                best_final_score = score;
                best_final_j = Some(j);
            }
        }
    }

    let curr_j = best_final_j?;

    // Backtrace to get matched indices
    let mut matched_indices = Vec::with_capacity(m);
    matched_indices.push(curr_j);
    let mut trace_j = curr_j;
    for i in (1..m).rev() {
        let p = parent[i][trace_j].expect("valid parent in DP");
        matched_indices.push(p);
        trace_j = p;
    }
    matched_indices.reverse();

    // Extra bonus if exact filename matches pattern
    let filename: String = target_chars[filename_start..].iter().collect();
    let pattern_str: String = pattern_chars.iter().collect();
    if filename.eq_ignore_ascii_case(&pattern_str) {
        best_final_score += 100;
    }

    Some((best_final_score, matched_indices))
}
