use std::io::Write as _;
use std::sync::Arc;

use rmux_core::{input::mode, GridRenderOptions, OptionStore, Pane, Screen, Session};

use crate::pane_transcript::PaneTranscript;

use super::pane_screen::{pane_default_style, pane_selection_overlay_style};
use super::pane_scrollbar::{
    resolve_pane_scrollbar, PaneScrollbarRenderContext, RenderedPaneScrollbar,
};
use super::{
    cursor_position_bytes, replace_cursor_position_bytes, styled_pane_screen,
    truncate_rendered_pane_line, visible_pane_geometry, StatusGeometry,
};

const LINE_REVISION_GENERATION_BITS: u32 = 32;
#[cfg(test)]
const LINE_REVISION_GENERATION_MASK: u64 = (1_u64 << LINE_REVISION_GENERATION_BITS) - 1;
const LINE_REVISION_ID_MASK: u64 = !((1_u64 << LINE_REVISION_GENERATION_BITS) - 1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaneRenderDelta {
    Incremental(PaneRenderDeltaFrame),
    RequiresFullRefresh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneRenderDeltaFrame {
    frame: Vec<u8>,
    cursor_style: Option<u32>,
}

impl PaneRenderDeltaFrame {
    pub(crate) fn new(frame: Vec<u8>, cursor_style: Option<u32>) -> Self {
        Self {
            frame,
            cursor_style,
        }
    }

    pub(crate) fn frame(&self) -> &[u8] {
        &self.frame
    }

    pub(crate) fn cursor_style(&self) -> Option<u32> {
        self.cursor_style
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneRenderSnapshot {
    x: u16,
    y: u16,
    rows: u16,
    cols: u16,
    terminal_cols: u16,
    terminal_rows: u16,
    lines: Vec<Arc<Vec<u8>>>,
    cursor: Vec<u8>,
    cursor_row: u16,
    cursor_col: u16,
    cursor_style: u32,
    title: String,
    path: String,
    mode: u32,
    line_revisions: Vec<u64>,
    scrollbar: Option<RenderedPaneScrollbar>,
}

impl PaneRenderSnapshot {
    pub(crate) const fn cursor_style(&self) -> u32 {
        self.cursor_style
    }

    fn cursor_visible(&self) -> bool {
        self.mode & mode::MODE_CURSOR != 0
    }

    fn cursor_visibility_changed(&self, next: &Self) -> bool {
        self.cursor_visible() != next.cursor_visible()
    }

    fn append_final_cursor_state(&self, frame: &mut Vec<u8>) {
        frame.extend_from_slice(&self.cursor);
        if self.cursor_visible() {
            frame.extend_from_slice(b"\x1b[?25h");
        } else {
            frame.extend_from_slice(b"\x1b[?25l");
        }
    }

    pub(crate) fn capture(
        session: &Session,
        options: &OptionStore,
        pane: &Pane,
        screen: &Screen,
    ) -> Option<Self> {
        let geometry = StatusGeometry::for_session(session, options);
        let raw_pane_geometry =
            visible_pane_geometry(session, options, pane, geometry.content_rows)?;
        let (pane_geometry, scrollbar) = resolve_pane_scrollbar(
            session,
            options,
            pane,
            PaneScrollbarRenderContext {
                geometry: raw_pane_geometry,
                history_size: screen.history_size(),
                alternate_on: screen.is_alternate(),
                copy_mode_scroll_position: None,
                content_y_offset: geometry.content_y_offset,
            },
        );
        if pane_geometry.cols() == 0 || pane_geometry.rows() == 0 {
            return None;
        }

        let utf8 = rmux_core::Utf8Config::from_options(options);
        let lines =
            capture_visible_pane_lines(session, options, pane, screen, pane_geometry.rows())
                .into_iter()
                .map(|line| {
                    truncate_rendered_pane_line(&line, usize::from(pane_geometry.cols()), &utf8)
                })
                .map(Arc::new)
                .collect::<Vec<_>>();
        let line_revisions = (0..usize::from(pane_geometry.rows()))
            .map(|row| screen.visible_line_revision(row).unwrap_or(0))
            .collect::<Vec<_>>();

        let (cursor_x, cursor_y) = screen.cursor_position();
        let cursor_row = pane_geometry
            .y()
            .saturating_add(geometry.content_y_offset)
            .saturating_add(cursor_y.min(u32::from(pane_geometry.rows().saturating_sub(1))) as u16);
        let cursor_col = pane_geometry
            .x()
            .saturating_add(cursor_x.min(u32::from(pane_geometry.cols().saturating_sub(1))) as u16);
        let cursor = cursor_position_bytes(cursor_row, cursor_col);

        Some(Self {
            x: pane_geometry.x(),
            y: pane_geometry.y().saturating_add(geometry.content_y_offset),
            rows: pane_geometry.rows(),
            cols: pane_geometry.cols(),
            terminal_cols: geometry.terminal_size.cols,
            terminal_rows: geometry.terminal_size.rows,
            lines,
            cursor,
            cursor_row,
            cursor_col,
            cursor_style: screen.cursor_style(),
            title: screen.title().to_owned(),
            path: screen.path().to_owned(),
            mode: screen.mode(),
            line_revisions,
            scrollbar,
        })
    }

    pub(crate) fn capture_unstyled_transcript_reusing(
        session: &Session,
        options: &OptionStore,
        pane: &Pane,
        transcript: &PaneTranscript,
        previous: Option<&Self>,
    ) -> Option<Self> {
        let default_style = pane_default_style(session, options, pane);
        let state = transcript.render_state();
        if state.has_selected_cells {
            return None;
        }

        let geometry = StatusGeometry::for_session(session, options);
        let raw_pane_geometry =
            visible_pane_geometry(session, options, pane, geometry.content_rows)?;
        let (pane_geometry, scrollbar) = resolve_pane_scrollbar(
            session,
            options,
            pane,
            PaneScrollbarRenderContext {
                geometry: raw_pane_geometry,
                history_size: transcript.history_size(),
                alternate_on: transcript.is_alternate(),
                copy_mode_scroll_position: None,
                content_y_offset: geometry.content_y_offset,
            },
        );
        if pane_geometry.cols() == 0 || pane_geometry.rows() == 0 {
            return None;
        }

        let utf8 = rmux_core::Utf8Config::from_options(options);
        let previous = previous.filter(|previous| {
            previous.x == pane_geometry.x()
                && previous.y == pane_geometry.y().saturating_add(geometry.content_y_offset)
                && previous.rows == pane_geometry.rows()
                && previous.cols == pane_geometry.cols()
        });
        let changes = transcript.capture_main_visible_line_changes(
            usize::from(pane_geometry.rows()),
            GridRenderOptions {
                with_sequences: true,
                include_empty_cells: true,
                trim_spaces: false,
                ..GridRenderOptions::default()
            },
            previous.map(|snapshot| snapshot.line_revisions.as_slice()),
            default_style.as_ref(),
        );
        let mut lines = Vec::with_capacity(changes.len());
        let mut line_revisions = Vec::with_capacity(changes.len());
        for (row, change) in changes.into_iter().enumerate() {
            line_revisions.push(change.revision);
            if let Some(line) = change.rendered {
                lines.push(Arc::new(truncate_rendered_pane_line(
                    &line,
                    usize::from(pane_geometry.cols()),
                    &utf8,
                )));
            } else if let Some(previous_line) = change
                .previous_row
                .and_then(|row| previous.and_then(|snapshot| snapshot.lines.get(row)))
            {
                lines.push(Arc::clone(previous_line));
            } else if let Some(previous_line) =
                previous.and_then(|snapshot| snapshot.lines.get(row))
            {
                lines.push(Arc::clone(previous_line));
            } else {
                lines.push(Arc::new(Vec::new()));
            }
        }

        let (cursor_x, cursor_y) = state.cursor_position;
        let cursor_row = pane_geometry
            .y()
            .saturating_add(geometry.content_y_offset)
            .saturating_add(cursor_y.min(u32::from(pane_geometry.rows().saturating_sub(1))) as u16);
        let cursor_col = pane_geometry
            .x()
            .saturating_add(cursor_x.min(u32::from(pane_geometry.cols().saturating_sub(1))) as u16);
        let cursor = cursor_position_bytes(cursor_row, cursor_col);

        Some(Self {
            x: pane_geometry.x(),
            y: pane_geometry.y().saturating_add(geometry.content_y_offset),
            rows: pane_geometry.rows(),
            cols: pane_geometry.cols(),
            terminal_cols: geometry.terminal_size.cols,
            terminal_rows: geometry.terminal_size.rows,
            lines,
            cursor,
            cursor_row,
            cursor_col,
            cursor_style: state.cursor_style,
            title: state.title,
            path: state.path,
            mode: state.mode,
            line_revisions,
            scrollbar,
        })
    }

    pub(crate) fn diff_to(&self, next: &Self) -> PaneRenderDelta {
        if self.requires_full_refresh(next) {
            return PaneRenderDelta::RequiresFullRefresh;
        }
        if let Some(frame) = self.diff_plain_cursor_append_to(next) {
            return PaneRenderDelta::Incremental(frame);
        }
        if let Some(frame) = self.diff_scroll_up_to(next) {
            return PaneRenderDelta::Incremental(frame);
        }

        let mut frame = Vec::new();
        let blank_line = vec![b' '; usize::from(next.cols)];
        let full_width = next.x == 0 && next.cols == next.terminal_cols;
        let changed_rows = self.lines.len().max(next.lines.len());
        for row in 0..changed_rows {
            let previous_line = self
                .lines
                .get(row)
                .map(|line| line.as_slice())
                .unwrap_or(blank_line.as_slice());
            let next_line = next
                .lines
                .get(row)
                .map(|line| line.as_slice())
                .unwrap_or(blank_line.as_slice());
            if previous_line == next_line {
                continue;
            }
            if frame.is_empty() {
                frame.extend_from_slice(b"\x1b[s\x1b[?25l");
            }
            let screen_row = next.y.saturating_add(row as u16);
            if let Some(spans) = styled_row_span_delta(previous_line, next_line, screen_row, next.x)
            {
                frame.extend_from_slice(&spans);
                continue;
            }
            frame.extend_from_slice(cursor_position_bytes(screen_row, next.x).as_slice());
            frame.extend_from_slice(b"\x1b[0m");
            let (line, clear_to_eol) =
                compact_plain_ascii_line_delta(previous_line, next_line, full_width)
                    .unwrap_or((next_line, false));
            frame.extend_from_slice(line);
            if clear_to_eol {
                frame.extend_from_slice(b"\x1b[K");
            }
        }

        if self.scrollbar != next.scrollbar {
            if frame.is_empty() {
                frame.extend_from_slice(b"\x1b[s\x1b[?25l");
            }
            if let Some(scrollbar) = next.scrollbar.as_ref() {
                frame.extend_from_slice(scrollbar.frame().as_slice());
            }
        }

        if !frame.is_empty() {
            frame.extend_from_slice(b"\x1b[0m\x1b[u");
            next.append_final_cursor_state(&mut frame);
        } else if self.cursor_visibility_changed(next) {
            next.append_final_cursor_state(&mut frame);
        } else if self.cursor != next.cursor {
            frame.extend_from_slice(&next.cursor);
        }

        PaneRenderDelta::Incremental(PaneRenderDeltaFrame {
            frame,
            cursor_style: (self.cursor_style != next.cursor_style).then_some(next.cursor_style),
        })
    }

    pub(crate) fn full_frame(&self) -> Vec<u8> {
        let mut frame = Vec::with_capacity(self.frame_capacity_hint());
        frame.extend_from_slice(b"\x1b[s\x1b[?25l\x1b[0m");
        for (row, line) in self.lines.iter().enumerate() {
            if row >= usize::from(self.rows) {
                break;
            }
            frame.extend_from_slice(
                cursor_position_bytes(self.y.saturating_add(row as u16), self.x).as_slice(),
            );
            frame.extend_from_slice(b"\x1b[0m");
            frame.extend_from_slice(line);
        }
        if let Some(scrollbar) = self.scrollbar.as_ref() {
            frame.extend_from_slice(scrollbar.frame().as_slice());
        }
        frame.extend_from_slice(b"\x1b[0m\x1b[u");
        self.append_final_cursor_state(&mut frame);
        frame
    }

    pub(crate) fn can_forward_plain_bytes(&self, bytes: &[u8]) -> bool {
        if self.x != 0 || self.cols != self.terminal_cols {
            return false;
        }
        let Some(would_scroll) = self.plain_bytes_would_scroll(bytes) else {
            return false;
        };
        !would_scroll || self.covers_full_terminal_height()
    }

    fn can_apply_plain_bytes_to_snapshot(&self, bytes: &[u8]) -> bool {
        self.plain_bytes_would_scroll(bytes).is_some()
    }

    fn covers_full_terminal_height(&self) -> bool {
        self.y == 0 && self.rows == self.terminal_rows
    }

    fn plain_bytes_would_scroll(&self, bytes: &[u8]) -> Option<bool> {
        let mut row = self.cursor_row.saturating_sub(self.y);
        let mut col = self.cursor_col.saturating_sub(self.x);
        if row >= self.rows || col >= self.cols {
            return None;
        }

        let mut index = 0;
        let mut would_scroll = false;
        while index < bytes.len() {
            match bytes[index] {
                b'\r' => {
                    col = 0;
                    index += 1;
                }
                b'\n' => {
                    if row.saturating_add(1) < self.rows {
                        row = row.saturating_add(1);
                    } else {
                        would_scroll = true;
                    }
                    index += 1;
                }
                b' '..=b'~' => {
                    if col >= self.cols {
                        return None;
                    }
                    col = col.saturating_add(1);
                    index += 1;
                }
                _ => return None,
            }
        }

        Some(would_scroll)
    }

    #[cfg(test)]
    pub(crate) fn positioned_plain_echo_frame(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        if bytes.is_empty() || !bytes.iter().all(|byte| matches!(*byte, b' '..=b'~')) {
            return None;
        }
        let row = self.cursor_row.checked_sub(self.y)?;
        let col = self.cursor_col.checked_sub(self.x)?;
        if row >= self.rows || col >= self.cols {
            return None;
        }
        let next_col = col.checked_add(bytes.len().try_into().ok()?)?;
        if next_col > self.cols {
            return None;
        }
        let line = self.lines.get(usize::from(row))?;
        if !is_plain_ascii_render_line(line) {
            return None;
        }

        let mut frame = Vec::with_capacity(bytes.len().saturating_add(32));
        frame.extend_from_slice(&cursor_position_bytes(self.cursor_row, self.cursor_col));
        frame.extend_from_slice(bytes);
        frame.extend_from_slice(&cursor_position_bytes(self.cursor_row, self.x + next_col));
        Some(frame)
    }

    pub(crate) fn positioned_plain_output_frame(&mut self, bytes: &[u8]) -> Option<Vec<u8>> {
        if bytes.is_empty() || self.x == 0 && self.cols == self.terminal_cols {
            return None;
        }
        if !self.can_apply_plain_bytes_to_snapshot(bytes)
            || self
                .lines
                .iter()
                .any(|line| !is_plain_ascii_render_line(line))
        {
            return None;
        }

        let mut row = self.cursor_row.checked_sub(self.y)?;
        let mut col = self.cursor_col.checked_sub(self.x)?;
        let mut frame = Vec::with_capacity(bytes.len().saturating_add(64));
        frame.extend_from_slice(b"\x1b[s\x1b[?25l\x1b[0m");
        let mut touched_rows = Vec::new();
        let mut index = 0;
        let original = self.clone();
        while index < bytes.len() {
            match bytes[index] {
                b'\r' => {
                    col = 0;
                    index += 1;
                }
                b'\n' => {
                    if row.saturating_add(1) >= self.rows {
                        return self.restore_positioned_plain_output(original);
                    } else {
                        row = row.saturating_add(1);
                    }
                    index += 1;
                }
                b' '..=b'~' => {
                    let run_end = bytes[index..]
                        .iter()
                        .position(|byte| !matches!(*byte, b' '..=b'~'))
                        .map_or(bytes.len(), |offset| index + offset);
                    let Ok(run_len) = u16::try_from(run_end - index) else {
                        return self.restore_positioned_plain_output(original);
                    };
                    if col.saturating_add(run_len) > self.cols {
                        return self.restore_positioned_plain_output(original);
                    }
                    frame.extend_from_slice(&cursor_position_bytes(
                        self.y.saturating_add(row),
                        self.x.saturating_add(col),
                    ));
                    frame.extend_from_slice(&bytes[index..run_end]);
                    if self
                        .apply_plain_run_to_snapshot(
                            usize::from(row),
                            usize::from(col),
                            &bytes[index..run_end],
                            &mut touched_rows,
                        )
                        .is_none()
                    {
                        return self.restore_positioned_plain_output(original);
                    }
                    col = col.saturating_add(run_len);
                    index = run_end;
                }
                _ => return self.restore_positioned_plain_output(original),
            }
        }

        for row_index in touched_rows {
            if let Some(revision) = self.line_revisions.get_mut(row_index) {
                *revision = revision.saturating_add(1);
            }
        }
        self.cursor_row = self.y.saturating_add(row);
        self.cursor_col = self.x.saturating_add(col);
        replace_cursor_position_bytes(&mut self.cursor, self.cursor_row, self.cursor_col);
        frame.extend_from_slice(b"\x1b[0m\x1b[u");
        self.append_final_cursor_state(&mut frame);
        Some(frame)
    }

    fn restore_positioned_plain_output(&mut self, original: Self) -> Option<Vec<u8>> {
        *self = original;
        None
    }

    pub(crate) fn apply_forwarded_plain_bytes(&mut self, bytes: &[u8]) -> bool {
        if !self.can_apply_plain_bytes_to_snapshot(bytes) {
            return false;
        }

        let mut row = self.cursor_row.saturating_sub(self.y);
        let mut col = self.cursor_col.saturating_sub(self.x);
        let mut touched_rows = Vec::new();
        for byte in bytes {
            match *byte {
                b'\r' => col = 0,
                b'\n' => {
                    if row.saturating_add(1) >= self.rows {
                        self.scroll_snapshot_up_one();
                        row = self.rows.saturating_sub(1);
                    } else {
                        row = row.saturating_add(1);
                    }
                }
                b' '..=b'~' => {
                    if self
                        .apply_plain_run_to_snapshot(
                            usize::from(row),
                            usize::from(col),
                            std::slice::from_ref(byte),
                            &mut touched_rows,
                        )
                        .is_none()
                    {
                        return false;
                    }
                    col = col.saturating_add(1);
                }
                _ => return false,
            }
        }
        for row_index in touched_rows {
            if let Some(revision) = self.line_revisions.get_mut(row_index) {
                *revision = revision.saturating_add(1);
            }
        }

        self.cursor_row = self.y.saturating_add(row);
        self.cursor_col = self.x.saturating_add(col);
        replace_cursor_position_bytes(&mut self.cursor, self.cursor_row, self.cursor_col);
        true
    }

    fn apply_plain_run_to_snapshot(
        &mut self,
        row_index: usize,
        col_index: usize,
        bytes: &[u8],
        touched_rows: &mut Vec<usize>,
    ) -> Option<()> {
        let cols = usize::from(self.cols);
        let end = col_index.checked_add(bytes.len())?;
        if row_index >= self.lines.len() || end > cols {
            return None;
        }
        let line = Arc::make_mut(&mut self.lines[row_index]);
        if line.len() < cols {
            line.resize(cols, b' ');
        }
        line[col_index..end].copy_from_slice(bytes);
        if touched_rows.last().copied() != Some(row_index) && !touched_rows.contains(&row_index) {
            touched_rows.push(row_index);
        }
        Some(())
    }

    fn scroll_snapshot_up_one(&mut self) {
        if self.lines.is_empty() {
            return;
        }
        let _ = self.lines.remove(0);
        self.lines.push(Arc::new(Vec::new()));
        if !self.line_revisions.is_empty() {
            let _ = self.line_revisions.remove(0);
            self.line_revisions
                .push(next_synthetic_line_revision(&self.line_revisions));
        }
    }

    fn requires_full_refresh(&self, next: &Self) -> bool {
        self.x != next.x || self.y != next.y || self.rows != next.rows || self.cols != next.cols
    }

    fn diff_plain_cursor_append_to(&self, next: &Self) -> Option<PaneRenderDeltaFrame> {
        if self.x != 0
            || next.x != 0
            || next.cols != next.terminal_cols
            || self.cursor_style != next.cursor_style
            || self.cursor_row < self.y
            || self.cursor_col < self.x
        {
            return None;
        }

        let row = usize::from(self.cursor_row - self.y);
        let rows = usize::from(next.rows);
        if row >= rows || row >= self.lines.len() || row >= next.lines.len() {
            return None;
        }

        let previous_line = self.lines[row].as_slice();
        let next_line = next.lines[row].as_slice();
        if !is_plain_ascii_render_line(previous_line) || !is_plain_ascii_render_line(next_line) {
            return None;
        }

        let previous_visible = trim_trailing_ascii_spaces_len(previous_line);
        let next_visible = trim_trailing_ascii_spaces_len(next_line);
        let previous_cursor_col = usize::from(self.cursor_col - self.x);
        if previous_cursor_col != previous_visible || next_visible <= previous_visible {
            return None;
        }
        if next_line.get(..previous_visible) != Some(&previous_line[..previous_visible]) {
            return None;
        }
        if other_rows_changed(&self.lines, &next.lines, row) {
            return None;
        }

        let same_line_cursor = next.cursor_row == self.cursor_row
            && next.cursor_col >= next.x
            && usize::from(next.cursor_col - next.x) == next_visible;
        let next_line_cursor = next.cursor_row == self.cursor_row.saturating_add(1)
            && next.cursor_col == next.x
            && row + 1 < rows;
        if !same_line_cursor && !next_line_cursor {
            return None;
        }

        let mut frame = next_line[previous_visible..next_visible].to_vec();
        if next_line_cursor {
            frame.extend_from_slice(b"\r\n");
        }
        if self.cursor_visibility_changed(next) {
            next.append_final_cursor_state(&mut frame);
        }
        Some(PaneRenderDeltaFrame {
            frame,
            cursor_style: None,
        })
    }

    fn diff_scroll_up_to(&self, next: &Self) -> Option<PaneRenderDeltaFrame> {
        if self.x != 0
            || next.x != 0
            || next.cols != next.terminal_cols
            || self.rows == 0
            || self.lines.len() != next.lines.len()
            || self.line_revisions.len() != next.line_revisions.len()
        {
            return None;
        }

        let rows = usize::from(next.rows).min(next.lines.len());
        let scroll_rows = (1..rows).find(|scroll_rows| {
            self.line_revisions[*scroll_rows..rows] == next.line_revisions[..rows - *scroll_rows]
                && self.lines[*scroll_rows..rows] == next.lines[..rows - *scroll_rows]
                && self.lines[*scroll_rows..rows]
                    .iter()
                    .any(|line| trim_trailing_ascii_spaces_len(line) > 0)
        })?;
        if next.lines[rows - scroll_rows..rows]
            .iter()
            .all(|line| line.is_empty())
            && self.cursor == next.cursor
        {
            return None;
        }

        let mut frame = Vec::new();
        frame.extend_from_slice(b"\x1b[s\x1b[?25l");
        write!(
            &mut frame,
            "\x1b[{};{}r\x1b[{}S\x1b[r",
            next.y.saturating_add(1),
            next.y.saturating_add(next.rows),
            scroll_rows
        )
        .expect("writing to Vec cannot fail");
        let blank_line = vec![b' '; usize::from(next.cols)];
        for row in rows - scroll_rows..rows {
            frame.extend_from_slice(
                cursor_position_bytes(next.y.saturating_add(row as u16), next.x).as_slice(),
            );
            frame.extend_from_slice(b"\x1b[0m");
            let (line, clear_to_eol) =
                compact_plain_ascii_line_delta(&blank_line, next.lines[row].as_slice(), true)
                    .unwrap_or((next.lines[row].as_slice(), false));
            frame.extend_from_slice(line);
            if clear_to_eol {
                frame.extend_from_slice(b"\x1b[K");
            }
        }
        frame.extend_from_slice(b"\x1b[0m\x1b[u");
        next.append_final_cursor_state(&mut frame);

        Some(PaneRenderDeltaFrame {
            frame,
            cursor_style: (self.cursor_style != next.cursor_style).then_some(next.cursor_style),
        })
    }

    fn frame_capacity_hint(&self) -> usize {
        let line_bytes = self.lines.iter().map(|line| line.len()).sum::<usize>();
        line_bytes
            .saturating_add(usize::from(self.rows).saturating_mul(16))
            .saturating_add(
                self.scrollbar
                    .as_ref()
                    .map_or(0, |scrollbar| scrollbar.frame().len()),
            )
            .saturating_add(32)
    }
}

fn compact_plain_ascii_line_delta<'a>(
    previous_line: &[u8],
    next_line: &'a [u8],
    full_width: bool,
) -> Option<(&'a [u8], bool)> {
    if !is_plain_ascii_render_line(previous_line) || !is_plain_ascii_render_line(next_line) {
        return None;
    }

    let previous_visible = trim_trailing_ascii_spaces_len(previous_line);
    let next_visible = trim_trailing_ascii_spaces_len(next_line);
    if previous_visible > next_visible && !full_width {
        return None;
    }

    Some((&next_line[..next_visible], previous_visible > next_visible))
}

/// The effective SGR state, one entry per attribute slot, so two rows that
/// reach the same style through different sequences compare equal.
type SgrState = std::collections::BTreeMap<u8, String>;

const SGR_BOLD: u8 = 0;
const SGR_DIM: u8 = 1;
const SGR_ITALIC: u8 = 2;
const SGR_UNDERLINE: u8 = 3;
const SGR_BLINK: u8 = 4;
const SGR_REVERSE: u8 = 5;
const SGR_HIDDEN: u8 = 6;
const SGR_STRIKE: u8 = 7;
const SGR_OVERLINE: u8 = 8;
const SGR_FOREGROUND: u8 = 9;
const SGR_BACKGROUND: u8 = 10;
const SGR_UNDERLINE_COLOR: u8 = 11;

trait SgrSequence {
    fn sequence(&self) -> Vec<u8>;
}

impl SgrSequence for SgrState {
    /// The state as one SGR sequence to emit after a reset.
    fn sequence(&self) -> Vec<u8> {
        if self.is_empty() {
            return Vec::new();
        }
        let params: Vec<&str> = self.values().map(String::as_str).collect();
        format!("\x1b[{}m", params.join(";")).into_bytes()
    }
}

/// Apply one SGR parameter list. `None` for anything not modelled, so the
/// row keeps the whole-row write rather than risk a wrong style.
fn apply_sgr(state: &mut SgrState, params: &[u8]) -> Option<()> {
    let params = std::str::from_utf8(params).ok()?;
    if params.is_empty() {
        state.clear();
        return Some(());
    }
    let mut tokens = params.split(';');
    while let Some(token) = tokens.next() {
        // A colon form (`38:2::r:g:b`, `4:3`) is one self-contained token.
        let code: u16 = token.split(':').next()?.parse().ok()?;
        let mut set = |slot: u8, value: String| {
            state.insert(slot, value);
        };
        match code {
            0 => state.clear(),
            1 => set(SGR_BOLD, token.to_owned()),
            2 => set(SGR_DIM, token.to_owned()),
            3 => set(SGR_ITALIC, token.to_owned()),
            4 => set(SGR_UNDERLINE, token.to_owned()),
            5 | 6 => set(SGR_BLINK, token.to_owned()),
            7 => set(SGR_REVERSE, token.to_owned()),
            8 => set(SGR_HIDDEN, token.to_owned()),
            9 => set(SGR_STRIKE, token.to_owned()),
            21 => set(SGR_UNDERLINE, token.to_owned()),
            53 => set(SGR_OVERLINE, token.to_owned()),
            22 => {
                state.remove(&SGR_BOLD);
                state.remove(&SGR_DIM);
            }
            23 => drop(state.remove(&SGR_ITALIC)),
            24 => drop(state.remove(&SGR_UNDERLINE)),
            25 => drop(state.remove(&SGR_BLINK)),
            27 => drop(state.remove(&SGR_REVERSE)),
            28 => drop(state.remove(&SGR_HIDDEN)),
            29 => drop(state.remove(&SGR_STRIKE)),
            55 => drop(state.remove(&SGR_OVERLINE)),
            30..=37 | 90..=97 => set(SGR_FOREGROUND, token.to_owned()),
            40..=47 | 100..=107 => set(SGR_BACKGROUND, token.to_owned()),
            39 => drop(state.remove(&SGR_FOREGROUND)),
            49 => drop(state.remove(&SGR_BACKGROUND)),
            59 => drop(state.remove(&SGR_UNDERLINE_COLOR)),
            38 | 48 | 58 => {
                let slot = match code {
                    38 => SGR_FOREGROUND,
                    48 => SGR_BACKGROUND,
                    _ => SGR_UNDERLINE_COLOR,
                };
                let value = if token.contains(':') {
                    token.to_owned()
                } else {
                    // `38;5;n` or `38;2;r;g;b` spread over the next tokens.
                    let kind = tokens.next()?;
                    let count = match kind {
                        "5" => 1,
                        "2" => 3,
                        _ => return None,
                    };
                    let mut parts = vec![token.to_owned(), kind.to_owned()];
                    for _ in 0..count {
                        parts.push(tokens.next()?.to_owned());
                    }
                    parts.join(";")
                };
                set(slot, value);
            }
            _ => return None,
        }
    }
    Some(())
}

/// One terminal cell of a rendered row: the SGR state in effect, the bytes
/// of its character (plus any zero-width marks that follow), and its width.
#[derive(Debug)]
struct RowCell {
    style: std::rc::Rc<[u8]>,
    text: std::ops::Range<usize>,
    width: usize,
}

/// Split a rendered row into cells indexed by start column (`None` marks the
/// trailing columns of a wide character). `None` overall when the row holds
/// anything but SGR sequences and printable characters (OSC hyperlinks,
/// other CSI, control bytes): those rows keep the whole-row write.
fn row_cells(line: &[u8]) -> Option<Vec<Option<RowCell>>> {
    let text = std::str::from_utf8(line).ok()?;
    let mut columns: Vec<Option<RowCell>> = Vec::new();
    let mut style = SgrState::new();
    // Rebuilt only when an SGR sequence changes it; cells share it.
    let mut current: std::rc::Rc<[u8]> = std::rc::Rc::from(Vec::new());
    let mut last_cell: Option<usize> = None;
    let mut index = 0;
    while index < line.len() {
        if line[index] == 0x1b {
            if line.get(index + 1) != Some(&b'[') {
                return None;
            }
            let end = ansi_csi_end(line, index)?;
            let sequence = &line[index..end];
            if *sequence.last()? != b'm' {
                return None;
            }
            apply_sgr(&mut style, &sequence[2..sequence.len() - 1])?;
            current = std::rc::Rc::from(style.sequence());
            index = end;
            continue;
        }
        let ch = text[index..].chars().next()?;
        let len = ch.len_utf8();
        if ch.is_control() {
            return None;
        }
        match unicode_width::UnicodeWidthChar::width(ch) {
            Some(0) => {
                // A zero-width mark belongs to the cell before it.
                let cell = columns.get_mut(last_cell?)?.as_mut()?;
                cell.text.end = index + len;
            }
            Some(width) => {
                last_cell = Some(columns.len());
                columns.push(Some(RowCell {
                    style: std::rc::Rc::clone(&current),
                    text: index..index + len,
                    width,
                }));
                columns.extend((1..width).map(|_| None));
            }
            None => return None,
        }
        index += len;
    }
    Some(columns)
}

/// The number of terminal columns a rendered row occupies, when the row is
/// made only of SGR sequences and printable characters.
pub(crate) fn styled_row_width(line: &[u8]) -> Option<usize> {
    row_cells(line).map(|columns| columns.len())
}

fn ansi_csi_end(line: &[u8], start: usize) -> Option<usize> {
    line.get(start + 2..)?
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte))
        .map(|offset| start + 3 + offset)
}

/// Repaint only the changed columns of a styled row, as tmux does, instead
/// of rewriting the whole row: a one-cell change (a spinner tick) in a
/// styled 160-column row otherwise costs the full ~2.5 KB row, several
/// times a second.
///
/// Rows of different rendered width keep the whole-row path, which owns
/// what a shorter row must clear. `None` falls back to that path.
pub(crate) fn styled_row_span_delta(
    previous: &[u8],
    next: &[u8],
    row: u16,
    x: u16,
) -> Option<Vec<u8>> {
    let before = row_cells(previous)?;
    let after = row_cells(next)?;
    if before.len() != after.len() {
        return None;
    }
    let width = after.len();
    let same = |column: usize| match (&before[column], &after[column]) {
        (Some(a), Some(b)) => {
            a.width == b.width
                && a.style == b.style
                && previous[a.text.clone()] == next[b.text.clone()]
        }
        (None, None) => true,
        _ => false,
    };

    let mut frame = Vec::new();
    let mut column = 0;
    while column < width {
        if same(column) {
            column += 1;
            continue;
        }
        // Begin at the cell that owns this column in the new row.
        let mut start = column;
        while start > 0 && after[start].is_none() {
            start -= 1;
        }
        let mut end = column;
        while end < width && !same(end) {
            end += 1;
        }
        // Finish a wide cell the run ends inside.
        while end < width && after[end].is_none() {
            end += 1;
        }
        let first = after[start].as_ref()?;
        let last = after[start..end].iter().rev().find_map(Option::as_ref)?;
        let column_offset = u16::try_from(start).ok()?;
        frame.extend_from_slice(&cursor_position_bytes(row, x.saturating_add(column_offset)));
        frame.extend_from_slice(b"\x1b[0m");
        frame.extend_from_slice(&first.style);
        frame.extend_from_slice(&next[first.text.start..last.text.end]);
        column = end;
    }
    // Only when it actually beats rewriting the row.
    (frame.len() < next.len()).then_some(frame)
}

fn is_plain_ascii_render_line(line: &[u8]) -> bool {
    line.iter().all(|byte| matches!(*byte, b' '..=b'~' | b'\t'))
}

fn trim_trailing_ascii_spaces_len(line: &[u8]) -> usize {
    line.iter()
        .rposition(|byte| *byte != b' ')
        .map_or(0, |index| index + 1)
}

fn other_rows_changed(
    previous: &[Arc<Vec<u8>>],
    next: &[Arc<Vec<u8>>],
    changed_row: usize,
) -> bool {
    let rows = previous.len().max(next.len());
    for row in 0..rows {
        if row == changed_row {
            continue;
        }
        let previous_line = previous
            .get(row)
            .map(|line| line.as_slice())
            .unwrap_or_default();
        let next_line = next
            .get(row)
            .map(|line| line.as_slice())
            .unwrap_or_default();
        if previous_line != next_line {
            return true;
        }
    }
    false
}

fn next_synthetic_line_revision(revisions: &[u64]) -> u64 {
    revisions
        .iter()
        .map(|revision| revision & LINE_REVISION_ID_MASK)
        .max()
        .unwrap_or_default()
        .saturating_add(1_u64 << LINE_REVISION_GENERATION_BITS)
        & LINE_REVISION_ID_MASK
}

fn capture_visible_pane_lines(
    session: &Session,
    options: &OptionStore,
    pane: &Pane,
    screen: &Screen,
    rows: u16,
) -> Vec<Vec<u8>> {
    let render_options = pane_render_options();
    let default_style = pane_default_style(session, options, pane);
    if screen.has_selected_cells() && pane_selection_overlay_style(session, options, pane).is_some()
    {
        return styled_pane_screen(session, options, pane, screen)
            .capture_transcript_lines_independent(
                rmux_core::ScreenCaptureRange::default(),
                render_options,
            )
            .into_iter()
            .take(usize::from(rows))
            .collect();
    }

    (0..usize::from(rows))
        .filter_map(|row| {
            if let Some(style) = default_style.as_ref() {
                screen.render_visible_line_independent_with_default_style(
                    row,
                    render_options,
                    style,
                )
            } else {
                screen.render_visible_line_independent(row, render_options)
            }
        })
        .collect()
}

fn pane_render_options() -> GridRenderOptions {
    GridRenderOptions {
        with_sequences: true,
        include_empty_cells: true,
        trim_spaces: false,
        ..GridRenderOptions::default()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::pane_transcript::PaneTranscript;

    use rmux_core::{input::InputParser, OptionStore, Screen, Session};
    use rmux_proto::{
        OptionName, ScopeSelector, SessionName, SetOptionMode, SplitDirection, TerminalSize,
        WindowTarget,
    };

    use super::{PaneRenderDelta, PaneRenderSnapshot};

    fn session_name(value: &str) -> SessionName {
        SessionName::new(value).expect("valid session name")
    }

    fn screen_with(bytes: &[u8]) -> Screen {
        let mut screen = Screen::new(TerminalSize { cols: 10, rows: 3 }, 100);
        let mut parser = InputParser::new();
        parser.parse(bytes, &mut screen);
        screen
    }

    fn render_lines(lines: &[&[u8]]) -> Vec<Arc<Vec<u8>>> {
        lines
            .iter()
            .map(|line| Arc::new((*line).to_vec()))
            .collect()
    }

    const ROW_WIDTH: u16 = 40;

    /// The row an application's output renders to, in the exact serialized
    /// form the delta compares.
    fn rendered_row(output: &str) -> Vec<u8> {
        let mut screen = Screen::new(
            TerminalSize {
                cols: ROW_WIDTH,
                rows: 1,
            },
            0,
        );
        InputParser::new().parse(output.as_bytes(), &mut screen);
        screen
            .render_visible_line_independent(0, super::pane_render_options())
            .expect("row 0 renders")
    }

    /// Paint `previous` on a client screen, apply the span delta, and require
    /// the result to equal `next` painted from scratch.
    fn assert_span_delta_replays(previous: &str, next: &str) -> Vec<u8> {
        let (before, after) = (rendered_row(previous), rendered_row(next));
        let delta = super::styled_row_span_delta(&before, &after, 0, 0).unwrap_or_else(|| {
            panic!(
                "styled rows of equal width use the span delta: {:?} -> {:?}",
                String::from_utf8_lossy(&before),
                String::from_utf8_lossy(&after)
            )
        });
        let mut client = Screen::new(
            TerminalSize {
                cols: ROW_WIDTH,
                rows: 1,
            },
            0,
        );
        let mut parser = InputParser::new();
        parser.parse(b"\x1b[1;1H\x1b[0m", &mut client);
        parser.parse(&before, &mut client);
        parser.parse(&delta, &mut client);
        assert_eq!(
            String::from_utf8_lossy(
                &client
                    .render_visible_line_independent(0, super::pane_render_options())
                    .expect("row 0 renders")
            ),
            String::from_utf8_lossy(&after),
            "delta {:?}",
            String::from_utf8_lossy(&delta)
        );
        delta
    }

    #[test]
    fn a_spinner_tick_repaints_one_cell_not_the_row() {
        let row = |spinner: &str| {
            format!(
                "\x1b[38;5;8m│\x1b[1;33m{spinner}\x1b[0m rmux:%196 \x1b[1mclaude\x1b[0m   WORK   9m"
            )
        };
        let delta = assert_span_delta_replays(&row("⠧"), &row("⠇"));
        let full = rendered_row(&row("⠇")).len();
        assert!(
            delta.len() * 2 < full,
            "one cell, not the {full}-byte row: {:?}",
            String::from_utf8_lossy(&delta)
        );
    }

    #[test]
    fn wide_and_combining_characters_keep_their_columns() {
        assert_span_delta_replays("배포 \x1b[32m⠧\x1b[0m 확인", "배포 \x1b[32m⠇\x1b[0m 확인");
        assert_span_delta_replays("ab 한글 x", "ab 한국 x");
        assert_span_delta_replays("한 x", "ab x");
        assert_span_delta_replays("ab x", "한 x");
        assert_span_delta_replays("e\u{301} done", "e\u{300} done");
    }

    #[test]
    fn a_style_change_alone_is_repainted() {
        assert_span_delta_replays("\x1b[31mERROR\x1b[0m state", "\x1b[32mERROR\x1b[0m state");
        assert_span_delta_replays("plain ", "\x1b[7mplain\x1b[0m ");
    }

    /// Random rows of ASCII, braille, Korean, combining marks and SGR with
    /// random edits: whenever the span delta applies, it must replay exactly.
    #[test]
    fn random_styled_edits_always_replay_exactly() {
        let pieces = [
            "a",
            "Z",
            " ",
            "│",
            "⠧",
            "⠇",
            "○",
            "한",
            "글",
            "e\u{301}",
            "\x1b[1m",
            "\x1b[22m",
            "\x1b[31m",
            "\x1b[38;5;8m",
            "\x1b[38;2;1;2;3m",
            "\x1b[48;5;236m",
            "\x1b[39m",
            "\x1b[0m",
            "\x1b[7m",
            "\x1b[4:3m",
        ];
        let mut seed = 0x2545_f491_u64;
        let mut next = |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % bound as u64).expect("small")
        };
        let mut applied = 0;
        for _ in 0..2000 {
            let len = 4 + next(14);
            let row: Vec<&str> = (0..len).map(|_| pieces[next(pieces.len())]).collect();
            let mut edited = row.clone();
            for _ in 0..1 + next(3) {
                let at = next(edited.len());
                edited[at] = pieces[next(pieces.len())];
            }
            let (previous, edited) = (row.concat(), edited.concat());
            let (before, after) = (rendered_row(&previous), rendered_row(&edited));
            if super::styled_row_span_delta(&before, &after, 0, 0).is_some() {
                assert_span_delta_replays(&previous, &edited);
                applied += 1;
            }
        }
        assert!(applied > 500, "span delta exercised only {applied} times");
    }

    #[test]
    fn equivalent_sgr_spellings_are_the_same_style() {
        let delta =
            assert_span_delta_replays("\x1b[1;33mok\x1b[0m x", "\x1b[1m\x1b[33mok\x1b[0m y");
        assert!(
            !String::from_utf8_lossy(&delta).contains("ok"),
            "only the changed cell: {:?}",
            String::from_utf8_lossy(&delta)
        );
    }

    #[test]
    fn rows_the_span_delta_cannot_model_keep_the_whole_row_write() {
        assert!(super::styled_row_span_delta(b"\x1b]8;;u\x07a", b"\x1b]8;;u\x07b", 0, 0).is_none());
        assert!(super::styled_row_span_delta(b"\x1b[2Ka", b"\x1b[2Kb", 0, 0).is_none());
        assert!(super::styled_row_span_delta(b"abc", b"abcd", 0, 0).is_none());
    }

    #[test]
    fn pane_delta_does_not_scroll_empty_zero_revision_rows_for_cursor_move() {
        let before = PaneRenderSnapshot {
            x: 0,
            y: 0,
            rows: 3,
            cols: 10,
            terminal_cols: 10,
            terminal_rows: 3,
            lines: render_lines(&[b"", b"", b""]),
            cursor: b"\x1b[1;1H".to_vec(),
            cursor_row: 0,
            cursor_col: 0,
            cursor_style: 0,
            title: String::new(),
            path: String::new(),
            mode: 0,
            line_revisions: vec![0, 0, 0],
            scrollbar: None,
        };
        let after = PaneRenderSnapshot {
            cursor: b"\x1b[1;2H".to_vec(),
            cursor_col: 1,
            ..before.clone()
        };

        let PaneRenderDelta::Incremental(delta) = before.diff_to(&after) else {
            panic!("cursor move should stay incremental");
        };
        let text = String::from_utf8(delta.frame().to_vec()).expect("delta is utf8");

        assert!(
            !text.contains('S'),
            "empty rows must not use scroll: {text:?}"
        );
        assert_eq!(delta.frame(), b"\x1b[1;2H");
    }

    #[test]
    fn pane_snapshot_forwarded_plain_bytes_bumps_revision_once_per_line() {
        let mut snapshot = PaneRenderSnapshot {
            x: 0,
            y: 0,
            rows: 2,
            cols: 10,
            terminal_cols: 10,
            terminal_rows: 2,
            lines: render_lines(&[b"", b""]),
            cursor: b"\x1b[1;1H".to_vec(),
            cursor_row: 0,
            cursor_col: 0,
            cursor_style: 0,
            title: String::new(),
            path: String::new(),
            mode: 0,
            line_revisions: vec![7, 11],
            scrollbar: None,
        };

        assert!(snapshot.apply_forwarded_plain_bytes(b"abc\r\ndef"));

        assert_eq!(snapshot.line_revisions, vec![8, 12]);
    }

    #[test]
    fn pane_delta_renders_only_changed_lines_and_cursor() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let before = screen_with(b"abc");
        let after = screen_with(b"abcd");
        let before = PaneRenderSnapshot::capture(&session, &options, pane, &before)
            .expect("before snapshot");
        let after =
            PaneRenderSnapshot::capture(&session, &options, pane, &after).expect("after snapshot");

        let PaneRenderDelta::Incremental(delta) = before.diff_to(&after) else {
            panic!("line update should not require a full refresh");
        };
        let text = String::from_utf8(delta.frame().to_vec()).expect("delta is utf8");

        assert_eq!(text, "d");
    }

    #[test]
    fn pane_delta_repaints_scrollbar_when_history_changes() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let mut options = OptionStore::new();
        options
            .set(
                ScopeSelector::Window(WindowTarget::with_window(session.name().clone(), 0)),
                OptionName::PaneScrollbars,
                "on".to_owned(),
                SetOptionMode::Replace,
            )
            .expect("scrollbar mode");
        let before = screen_with(b"one\r\ntwo\r\nthree");
        let after = screen_with(b"one\r\ntwo\r\nthree\r\nfour");
        assert_eq!(before.history_size(), 0);
        assert_eq!(after.history_size(), 1);
        let before = PaneRenderSnapshot::capture(&session, &options, pane, &before)
            .expect("before snapshot");
        let after =
            PaneRenderSnapshot::capture(&session, &options, pane, &after).expect("after snapshot");

        let PaneRenderDelta::Incremental(delta) = before.diff_to(&after) else {
            panic!("history update should stay incremental");
        };

        assert!(delta
            .frame()
            .windows(b"\x1b[1;10H\x1b[0;37;40m ".len())
            .any(|window| window == b"\x1b[1;10H\x1b[0;37;40m "));
        assert!(delta
            .frame()
            .windows(b"\x1b[2;10H\x1b[0;30;47m ".len())
            .any(|window| window == b"\x1b[2;10H\x1b[0;30;47m "));
    }

    #[test]
    fn pane_delta_keeps_styled_zwj_text_at_right_edge() {
        let size = TerminalSize { cols: 5, rows: 3 };
        let session = Session::new(session_name("alpha"), size);
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let before = Screen::new(size, 100);
        let mut after = Screen::new(size, 100);
        let mut parser = InputParser::new();
        parser.parse("\x1b[31m👩\u{200d}💻ABC".as_bytes(), &mut after);
        let before = PaneRenderSnapshot::capture(&session, &options, pane, &before)
            .expect("before snapshot");
        let after =
            PaneRenderSnapshot::capture(&session, &options, pane, &after).expect("after snapshot");

        let PaneRenderDelta::Incremental(delta) = before.diff_to(&after) else {
            panic!("composed-cell repaint should stay incremental");
        };
        let frame = String::from_utf8(delta.frame().to_vec()).expect("delta frame is utf-8");

        assert!(
            frame.contains("\u{1b}[31m👩\u{200d}💻ABC"),
            "delta repaint must preserve styled ZWJ cells and following text: {frame:?}"
        );
    }

    #[test]
    fn pane_delta_trims_plain_ascii_padding_for_short_line_growth() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let before = screen_with(b"");
        let after = screen_with(b"abc");
        let before = PaneRenderSnapshot::capture(&session, &options, pane, &before)
            .expect("before snapshot");
        let after =
            PaneRenderSnapshot::capture(&session, &options, pane, &after).expect("after snapshot");

        let PaneRenderDelta::Incremental(delta) = before.diff_to(&after) else {
            panic!("short ASCII output should stay incremental");
        };
        let text = String::from_utf8(delta.frame().to_vec()).expect("delta is utf8");

        assert_eq!(text, "abc");
        assert!(
            !text.contains("abc       "),
            "plain ASCII append should not repaint trailing padding: {text:?}"
        );
        assert!(!text.contains("\u{1b}[K"), "{text:?}");
    }

    #[test]
    fn pane_delta_uses_plain_ascii_cursor_append_for_line_advance() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let before = screen_with(b"");
        let after = screen_with(b"abc\r\n");
        let before = PaneRenderSnapshot::capture(&session, &options, pane, &before)
            .expect("before snapshot");
        let after =
            PaneRenderSnapshot::capture(&session, &options, pane, &after).expect("after snapshot");

        let PaneRenderDelta::Incremental(delta) = before.diff_to(&after) else {
            panic!("line advance should stay incremental");
        };

        assert_eq!(delta.frame(), b"abc\r\n");
    }

    #[test]
    fn pane_snapshot_raw_forwarding_rejects_plain_bottom_scroll_with_status_line() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let screen = screen_with(b"\x1b[3;1H");
        let snapshot =
            PaneRenderSnapshot::capture(&session, &options, pane, &screen).expect("snapshot");

        assert!(!snapshot.can_forward_plain_bytes(b"x\r\n"));
        assert!(snapshot.can_forward_plain_bytes(b"x"));
        assert!(!snapshot.can_forward_plain_bytes(b"\x1b[31m"));
    }

    #[test]
    fn pane_snapshot_raw_forwarding_allows_plain_bottom_scroll_without_status_line() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 3 });
        let pane = session.window().active_pane().expect("active pane");
        let mut options = OptionStore::new();
        options
            .set(
                ScopeSelector::Global,
                OptionName::Status,
                "off".to_owned(),
                SetOptionMode::Replace,
            )
            .expect("status option can be disabled");
        let screen = screen_with(b"\x1b[3;1H");
        let snapshot =
            PaneRenderSnapshot::capture(&session, &options, pane, &screen).expect("snapshot");

        assert!(snapshot.can_forward_plain_bytes(b"x\r\n"));
        assert!(snapshot.can_forward_plain_bytes(b"x"));
        assert!(!snapshot.can_forward_plain_bytes(b"\x1b[31m"));
    }

    #[test]
    fn pane_snapshot_builds_positioned_echo_for_split_panes() {
        let snapshot = PaneRenderSnapshot {
            x: 10,
            y: 2,
            rows: 3,
            cols: 20,
            terminal_cols: 80,
            terminal_rows: 24,
            lines: render_lines(&[b"prompt>             ", b"", b""]),
            cursor: b"\x1b[3;18H".to_vec(),
            cursor_row: 2,
            cursor_col: 17,
            cursor_style: 0,
            title: String::new(),
            path: String::new(),
            mode: 0,
            line_revisions: vec![1, 0, 0],
            scrollbar: None,
        };

        let frame = snapshot
            .positioned_plain_echo_frame(b"abc")
            .expect("split pane echo can be positioned");

        assert_eq!(frame, b"\x1b[3;18Habc\x1b[3;21H");
    }

    #[test]
    fn pane_snapshot_rejects_positioned_echo_on_styled_or_wrapping_input() {
        let snapshot = PaneRenderSnapshot {
            x: 10,
            y: 2,
            rows: 1,
            cols: 4,
            terminal_cols: 80,
            terminal_rows: 24,
            lines: render_lines(&[b"\x1b[31mred"]),
            cursor: b"\x1b[3;14H".to_vec(),
            cursor_row: 2,
            cursor_col: 13,
            cursor_style: 0,
            title: String::new(),
            path: String::new(),
            mode: 0,
            line_revisions: vec![1],
            scrollbar: None,
        };

        assert!(snapshot.positioned_plain_echo_frame(b"x").is_none());
        assert!(PaneRenderSnapshot {
            lines: render_lines(&[b"    "]),
            ..snapshot
        }
        .positioned_plain_echo_frame(b"xy")
        .is_none());
    }

    #[test]
    fn pane_snapshot_applies_forwarded_plain_bytes_without_recapture() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let screen = screen_with(b"");
        let mut snapshot =
            PaneRenderSnapshot::capture(&session, &options, pane, &screen).expect("snapshot");

        assert!(snapshot.apply_forwarded_plain_bytes(b"abc\r\n"));

        assert_eq!(&snapshot.lines[0][..3], b"abc");
        assert_eq!(snapshot.cursor_row, 1);
        assert_eq!(snapshot.cursor_col, 0);
        assert_eq!(snapshot.cursor, b"\x1b[2;1H");
    }

    #[test]
    fn pane_snapshot_applies_positioned_echo_for_split_panes() {
        let mut snapshot = PaneRenderSnapshot {
            x: 10,
            y: 2,
            rows: 3,
            cols: 20,
            terminal_cols: 80,
            terminal_rows: 24,
            lines: render_lines(&[b"prompt>             ", b"", b""]),
            cursor: b"\x1b[3;18H".to_vec(),
            cursor_row: 2,
            cursor_col: 17,
            cursor_style: 0,
            title: String::new(),
            path: String::new(),
            mode: 0,
            line_revisions: vec![1, 0, 0],
            scrollbar: None,
        };

        assert!(!snapshot.can_forward_plain_bytes(b"abc"));
        assert!(snapshot.apply_forwarded_plain_bytes(b"abc"));

        assert_eq!(&snapshot.lines[0][7..10], b"abc");
        assert_eq!(snapshot.cursor_col, 20);
        assert_eq!(snapshot.cursor, b"\x1b[3;21H");
    }

    #[test]
    fn pane_delta_clears_full_width_line_when_plain_ascii_shrinks() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let before = screen_with(b"abcdefghij");
        let after = screen_with(b"abcdefghij\r\x1b[Kabc");
        let before = PaneRenderSnapshot::capture(&session, &options, pane, &before)
            .expect("before snapshot");
        let after =
            PaneRenderSnapshot::capture(&session, &options, pane, &after).expect("after snapshot");

        let PaneRenderDelta::Incremental(delta) = before.diff_to(&after) else {
            panic!("line shrink should stay incremental");
        };
        let text = String::from_utf8(delta.frame().to_vec()).expect("delta is utf8");

        assert!(text.contains("\u{1b}[1;1H\u{1b}[0mabc\u{1b}[K"), "{text:?}");
        assert!(
            !text.contains("abc       "),
            "full-width shrink should clear instead of repainting padding: {text:?}"
        );
    }

    #[test]
    fn pane_delta_keeps_padding_for_plain_ascii_shrink_inside_split_pane() {
        let before = PaneRenderSnapshot {
            x: 2,
            y: 0,
            rows: 1,
            cols: 5,
            terminal_cols: 10,
            terminal_rows: 1,
            lines: render_lines(&[b"abcde"]),
            cursor: b"\x1b[1;6H".to_vec(),
            cursor_row: 0,
            cursor_col: 5,
            cursor_style: 0,
            title: String::new(),
            path: String::new(),
            mode: 0,
            line_revisions: vec![1],
            scrollbar: None,
        };
        let after = PaneRenderSnapshot {
            lines: render_lines(&[b"ab   "]),
            cursor: b"\x1b[1;4H".to_vec(),
            cursor_col: 3,
            line_revisions: vec![2],
            ..before.clone()
        };

        let PaneRenderDelta::Incremental(delta) = before.diff_to(&after) else {
            panic!("split pane shrink should stay incremental");
        };
        let text = String::from_utf8(delta.frame().to_vec()).expect("delta is utf8");

        assert!(text.contains("\u{1b}[1;3H\u{1b}[0mab   "), "{text:?}");
        assert!(
            !text.contains("\u{1b}[K"),
            "split pane shrink must not clear outside pane bounds: {text:?}"
        );
    }

    #[test]
    fn pane_delta_keeps_title_changes_incremental() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let before = screen_with(b"abc");
        let mut after = screen_with(b"abc");
        after.set_title("new title");
        let before = PaneRenderSnapshot::capture(&session, &options, pane, &before)
            .expect("before snapshot");
        let after =
            PaneRenderSnapshot::capture(&session, &options, pane, &after).expect("after snapshot");

        assert_eq!(
            before.diff_to(&after),
            PaneRenderDelta::Incremental(super::PaneRenderDeltaFrame {
                frame: Vec::new(),
                cursor_style: None,
            })
        );
    }

    #[test]
    fn pane_delta_renders_new_prompt_lines_incrementally() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let before = screen_with(b"abc");
        let after = screen_with(b"abc\r\ndef");
        let before = PaneRenderSnapshot::capture(&session, &options, pane, &before)
            .expect("before snapshot");
        let after =
            PaneRenderSnapshot::capture(&session, &options, pane, &after).expect("after snapshot");

        let PaneRenderDelta::Incremental(delta) = before.diff_to(&after) else {
            panic!("new shell prompt lines should not force a full refresh");
        };
        let text = String::from_utf8(delta.frame().to_vec()).expect("delta is utf8");

        assert!(text.contains("\u{1b}[2;1H"));
        assert!(text.contains("def"));
        assert!(text.ends_with("\u{1b}[2;4H\u{1b}[?25h"));
    }

    #[test]
    fn pane_delta_scrolls_full_width_panes_instead_of_repainting_shifted_rows() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let mut screen = Screen::new(TerminalSize { cols: 10, rows: 3 }, 100);
        let mut parser = InputParser::new();
        parser.parse(b"one\r\ntwo\r\nthree", &mut screen);
        let before =
            PaneRenderSnapshot::capture(&session, &options, pane, &screen).expect("snapshot");

        parser.parse(b"\r\nfour", &mut screen);
        let after =
            PaneRenderSnapshot::capture(&session, &options, pane, &screen).expect("snapshot");

        let PaneRenderDelta::Incremental(delta) = before.diff_to(&after) else {
            panic!("scrolling a full-width pane should stay incremental");
        };
        let text = String::from_utf8(delta.frame().to_vec()).expect("delta is utf8");

        assert!(text.contains("\u{1b}[1;3r"), "{text:?}");
        assert!(text.contains("\u{1b}[1S"), "{text:?}");
        assert!(text.contains("\u{1b}[3;1H\u{1b}[0mfour"), "{text:?}");
        assert!(
            !text.contains("\u{1b}[1;1H\u{1b}[0mtwo"),
            "shifted rows should be moved by terminal scroll, not repainted: {text:?}"
        );
    }

    #[test]
    fn pane_snapshot_plain_forwarding_handles_bottom_scroll() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 3 });
        let pane = session.window().active_pane().expect("active pane");
        let mut options = OptionStore::new();
        options
            .set(
                ScopeSelector::Global,
                OptionName::Status,
                "off".to_owned(),
                SetOptionMode::Replace,
            )
            .expect("status option can be disabled");
        let mut screen = Screen::new(TerminalSize { cols: 10, rows: 3 }, 100);
        let mut parser = InputParser::new();
        parser.parse(b"one\r\ntwo\r\nthree", &mut screen);
        let mut forwarded =
            PaneRenderSnapshot::capture(&session, &options, pane, &screen).expect("snapshot");
        let before_revisions = forwarded.line_revisions.clone();

        assert!(forwarded.can_forward_plain_bytes(b"\r\nfour"));
        assert!(forwarded.apply_forwarded_plain_bytes(b"\r\nfour"));

        parser.parse(b"\r\nfour", &mut screen);
        let recaptured =
            PaneRenderSnapshot::capture(&session, &options, pane, &screen).expect("snapshot");

        assert_eq!(forwarded.lines, recaptured.lines);
        assert_eq!(forwarded.cursor, recaptured.cursor);
        assert_eq!(forwarded.cursor_row, recaptured.cursor_row);
        assert_eq!(forwarded.cursor_col, recaptured.cursor_col);
        assert_eq!(
            &forwarded.line_revisions[..2],
            &recaptured.line_revisions[..2],
            "scrolled rows must retain their original line identities"
        );
        assert_ne!(
            forwarded.line_revisions[2] & super::LINE_REVISION_ID_MASK,
            before_revisions[2] & super::LINE_REVISION_ID_MASK,
            "bottom scroll must allocate a fresh line identity"
        );
        assert_eq!(
            forwarded.line_revisions[2] & super::LINE_REVISION_GENERATION_MASK,
            1,
            "writing to the fresh bottom line bumps its generation once"
        );
    }

    #[test]
    fn pane_snapshot_positioned_plain_output_rejects_split_pane_scroll() {
        let mut session = Session::new(session_name("alpha"), TerminalSize { cols: 20, rows: 4 });
        session
            .split_active_pane_with_direction(SplitDirection::Vertical)
            .expect("split pane");
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let mut screen = Screen::new(TerminalSize { cols: 10, rows: 3 }, 100);
        let mut parser = InputParser::new();
        parser.parse(b"one\r\ntwo\r\nthree", &mut screen);
        let mut forwarded =
            PaneRenderSnapshot::capture(&session, &options, pane, &screen).expect("snapshot");

        assert!(
            forwarded
                .positioned_plain_output_frame(b"\r\nfour")
                .is_none(),
            "split-pane scroll must not emit DECSTBM/SU because that scrolls all terminal columns"
        );
        assert_eq!(
            forwarded,
            PaneRenderSnapshot::capture(&session, &options, pane, &screen).expect("snapshot"),
            "rejected positioned plain output must not mutate the reusable snapshot"
        );
    }

    #[test]
    fn pane_delta_reuses_rendered_lines_shifted_by_scroll() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let mut transcript = PaneTranscript::new(100, TerminalSize { cols: 10, rows: 3 });
        transcript.append_bytes(b"one\r\ntwo\r\nthree");
        let before = PaneRenderSnapshot::capture_unstyled_transcript_reusing(
            &session,
            &options,
            pane,
            &transcript,
            None,
        )
        .expect("before snapshot");

        transcript.append_bytes(b"\r\nfour");
        let after = PaneRenderSnapshot::capture_unstyled_transcript_reusing(
            &session,
            &options,
            pane,
            &transcript,
            Some(&before),
        )
        .expect("after snapshot");

        assert_eq!(after.lines[0], before.lines[1]);
        assert_eq!(after.lines[1], before.lines[2]);
        assert!(Arc::ptr_eq(&after.lines[0], &before.lines[1]));
        assert!(Arc::ptr_eq(&after.lines[1], &before.lines[2]));
        assert!(String::from_utf8(after.lines[2].as_ref().clone())
            .expect("line is utf8")
            .contains("four"));

        let PaneRenderDelta::Incremental(delta) = before.diff_to(&after) else {
            panic!("scrolling a full-width pane should stay incremental");
        };
        let text = String::from_utf8(delta.frame().to_vec()).expect("delta is utf8");
        assert!(text.contains("\u{1b}[1;3r"), "{text:?}");
        assert!(
            !text.contains("\u{1b}[1;1H\u{1b}[0mtwo"),
            "shifted rows should be moved by terminal scroll, not repainted: {text:?}"
        );
    }

    #[test]
    fn pane_delta_live_capture_applies_window_style_without_full_screen_clone() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let window = WindowTarget::with_window(session.name().clone(), 0);
        let pane = session.window().active_pane().expect("active pane");
        let mut options = OptionStore::new();
        options
            .set(
                ScopeSelector::Window(window),
                OptionName::WindowActiveStyle,
                "bg=red".to_owned(),
                SetOptionMode::Replace,
            )
            .expect("window style set succeeds");
        let mut transcript = PaneTranscript::new(100, TerminalSize { cols: 10, rows: 3 });
        transcript.append_bytes(b"one\r\ntwo\r\nthree");

        let snapshot = PaneRenderSnapshot::capture_unstyled_transcript_reusing(
            &session,
            &options,
            pane,
            &transcript,
            None,
        )
        .expect("styled live snapshot should not require full screen clone");

        let line = String::from_utf8(snapshot.lines[0].as_ref().clone()).expect("line is utf8");
        assert!(line.contains("\u{1b}[41mone"), "{line:?}");
    }

    #[test]
    fn pane_snapshot_full_frame_repaints_all_visible_lines() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let screen = screen_with(b"abc\r\ndef");
        let snapshot =
            PaneRenderSnapshot::capture(&session, &options, pane, &screen).expect("snapshot");

        let text = String::from_utf8(snapshot.full_frame()).expect("frame is utf8");

        assert!(text.contains("\u{1b}[1;1H\u{1b}[0mabc"));
        assert!(text.contains("\u{1b}[2;1H\u{1b}[0mdef"));
        assert!(text.starts_with("\u{1b}[s\u{1b}[?25l\u{1b}[0m"));
        assert!(text.ends_with("\u{1b}[2;4H\u{1b}[?25h"));
    }

    #[test]
    fn pane_snapshot_full_frame_restores_hidden_cursor_state() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let screen = screen_with(b"\x1b[?25labc");
        let snapshot =
            PaneRenderSnapshot::capture(&session, &options, pane, &screen).expect("snapshot");

        let text = String::from_utf8(snapshot.full_frame()).expect("frame is utf8");

        assert!(text.starts_with("\u{1b}[s\u{1b}[?25l\u{1b}[0m"));
        assert!(text.ends_with("\u{1b}[1;4H\u{1b}[?25l"));
    }

    #[test]
    fn pane_delta_emits_cursor_visibility_only_change() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let before = screen_with(b"abc");
        let after = screen_with(b"abc\x1b[?25l");
        let before = PaneRenderSnapshot::capture(&session, &options, pane, &before)
            .expect("before snapshot");
        let after =
            PaneRenderSnapshot::capture(&session, &options, pane, &after).expect("after snapshot");

        let PaneRenderDelta::Incremental(delta) = before.diff_to(&after) else {
            panic!("cursor visibility change should stay incremental");
        };

        assert_eq!(delta.frame(), b"\x1b[1;4H\x1b[?25l");
    }

    #[test]
    fn pane_snapshot_full_frame_resets_before_default_row_after_styled_full_width_row() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let screen = screen_with(b"\x1b[48;5;255m          \r\n\x1b[0mplain");
        let snapshot =
            PaneRenderSnapshot::capture(&session, &options, pane, &screen).expect("snapshot");

        let text = String::from_utf8(snapshot.full_frame()).expect("frame is utf8");

        assert!(
            text.contains("\u{1b}[1;1H\u{1b}[0m\u{1b}[48;5;255m          "),
            "{text:?}"
        );
        assert!(
            text.contains("\u{1b}[2;1H\u{1b}[0mplain"),
            "default rows must not inherit the previous row's background: {text:?}"
        );
    }

    #[test]
    fn pane_delta_repaints_blank_lines_when_background_changes() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let before = screen_with(b"alpha\r\n          ");
        let after = screen_with(b"\x1b[48;2;20;20;20malpha\r\n          ");
        let before = PaneRenderSnapshot::capture(&session, &options, pane, &before)
            .expect("before snapshot");
        let after =
            PaneRenderSnapshot::capture(&session, &options, pane, &after).expect("after snapshot");

        let PaneRenderDelta::Incremental(delta) = before.diff_to(&after) else {
            panic!("style-only row changes should repaint incrementally");
        };
        let text = String::from_utf8(delta.frame().to_vec()).expect("delta is utf8");

        assert!(text.contains("\u{1b}[1;1H"));
        assert!(text.contains("\u{1b}[48;2;20;20;20m"));
        assert!(text.contains("\u{1b}[2;1H"));
    }

    #[test]
    fn pane_delta_resets_each_repainted_row_independently() {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 10, rows: 4 });
        let pane = session.window().active_pane().expect("active pane");
        let options = OptionStore::new();
        let before = screen_with(b"0123456789\r\nbase");
        let after = screen_with(b"\x1b[48;2;20;20;20mabcdefghij\r\n\x1b[0mnext");
        let before = PaneRenderSnapshot::capture(&session, &options, pane, &before)
            .expect("before snapshot");
        let after =
            PaneRenderSnapshot::capture(&session, &options, pane, &after).expect("after snapshot");

        let PaneRenderDelta::Incremental(delta) = before.diff_to(&after) else {
            panic!("changed rows should repaint incrementally");
        };
        let text = String::from_utf8(delta.frame().to_vec()).expect("delta is utf8");

        assert!(
            text.contains("\u{1b}[1;1H\u{1b}[0m\u{1b}[48;2;20;20;20mabcdefghij"),
            "{text:?}"
        );
        assert!(
            text.contains("\u{1b}[2;1H\u{1b}[0mnext"),
            "the second row must not inherit row one's background: {text:?}"
        );
    }
}
