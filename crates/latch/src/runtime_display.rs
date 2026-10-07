//! A display mirror of the runtime's authoritative viewport.

use anyhow::Result;
use libghostty_vt::{Terminal, style::RgbColor, terminal::CursorStyle};

use crate::runtime_protocol::{Cursor, Dimensions, Snapshot};

#[derive(Default)]
pub struct DisplayState {
    dimensions: Option<Dimensions>,
    rows: Vec<String>,
    background: Option<[u8; 3]>,
    cursor: Option<Cursor>,
}

impl DisplayState {
    #[tracing::instrument(skip_all)]
    pub fn apply(
        &mut self,
        terminal: &mut Terminal<'static, 'static>,
        frame: &Snapshot,
    ) -> Result<()> {
        let dimensions = frame.dimensions;
        let resized = self.dimensions != Some(dimensions);
        if resized {
            terminal.resize(
                dimensions.cols,
                dimensions.rows,
                dimensions.cell_width.into(),
                dimensions.cell_height.into(),
            )?;
            terminal.vt_write(b"\x1b[0m\x1b[2J\x1b[?7l");
            self.rows.clear();
            self.dimensions = Some(dimensions);
        }
        if self.background != Some(frame.background) {
            terminal.set_default_bg_color(Some(rgb(frame.background)))?;
            self.background = Some(frame.background);
        }
        if self.cursor.as_ref().map(|cursor| cursor.color) != Some(frame.cursor.color) {
            terminal.set_default_cursor_color(Some(rgb(frame.cursor.color)))?;
        }
        let mut painted = resized;
        for (index, row) in frame.rows.iter().enumerate().take(dimensions.rows as usize) {
            if self.rows.get(index) == Some(row) {
                continue;
            }
            terminal.vt_write(format!("\x1b[{};1H\x1b[0m\x1b[2K", index + 1).as_bytes());
            terminal.vt_write(row.as_bytes());
            painted = true;
        }
        self.rows.clone_from(&frame.rows);
        if painted || self.cursor.as_ref() != Some(&frame.cursor) {
            if frame.cursor.style == 0 {
                terminal.set_default_cursor_style(Some(CursorStyle::BlockHollow))?;
            }
            terminal.vt_write(
                format!(
                    "\x1b[0m\x1b[{};{}H\x1b[{} q\x1b[?25{}",
                    frame.cursor.row.saturating_add(1),
                    frame.cursor.col.saturating_add(1),
                    frame.cursor.style,
                    if frame.cursor.visible { 'h' } else { 'l' }
                )
                .as_bytes(),
            );
            self.cursor = Some(frame.cursor.clone());
        }
        Ok(())
    }
}

fn rgb([r, g, b]: [u8; 3]) -> RgbColor {
    RgbColor { r, g, b }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{runtime_protocol::SessionInfo, search};
    use libghostty_vt::render::{CursorVisualStyle, RenderState};
    use libghostty_vt::terminal::Options;

    #[test]
    fn complete_viewports_replace_missed_output_and_restore_cursor() {
        let mut terminal = Terminal::new(Options {
            cols: 6,
            rows: 2,
            max_scrollback: 0,
        })
        .unwrap();
        let mut display = DisplayState::default();
        let mut frame = Snapshot {
            revision: 1,
            info: SessionInfo::default(),
            dimensions: Dimensions {
                cols: 6,
                rows: 2,
                cell_width: 8,
                cell_height: 18,
            },
            background: [0, 0, 0],
            rows: vec!["abcdef".into(), "stale".into()],
            cursor: Cursor {
                row: 0,
                col: 2,
                visible: true,
                style: 6,
                color: [255, 255, 255],
            },
            mouse_tracking: false,
            scroll_offset: 0,
            blocks: None,
        };
        display.apply(&mut terminal, &frame).unwrap();
        assert!(
            search::screen_text(&terminal)
                .unwrap()
                .starts_with("abcdef\nstale")
        );
        frame.revision = 100;
        frame.rows = vec!["fresh".into(), String::new()];
        display.apply(&mut terminal, &frame).unwrap();
        assert_eq!(search::screen_text(&terminal).unwrap().trim_end(), "fresh");
        terminal.vt_write(b"!");
        assert!(search::screen_text(&terminal).unwrap().starts_with("fr!sh"));
        frame.cursor.style = 0;
        display.apply(&mut terminal, &frame).unwrap();
        let mut render = RenderState::new().unwrap();
        assert_eq!(
            render
                .update(&terminal)
                .unwrap()
                .cursor_visual_style()
                .unwrap(),
            CursorVisualStyle::BlockHollow
        );
    }
}
