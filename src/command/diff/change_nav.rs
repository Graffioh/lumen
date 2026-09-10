use std::ops::Range;

use ratatui::layout::Rect;

/// Navigation targets are side-by-side row ranges, sized for the current viewport.
#[derive(Default)]
pub struct ChangeNavigation {
    pub groups: Vec<Range<usize>>,
    pub selected: Option<usize>,
    pub scroll_targets: Vec<usize>,
    pub area: Rect,
}

impl ChangeNavigation {
    pub fn new(groups: Vec<Range<usize>>, anchor: Option<usize>, footer: Rect) -> Self {
        let selected = anchor.and_then(|row| groups.iter().position(|g| g.contains(&row)));
        let label = format!(" {} / {} ", selected.map_or(0, |i| i + 1), groups.len());
        let width = (label.len() + 6) as u16;
        let area = if footer.width >= width {
            Rect::new(footer.x + (footer.width - width) / 2, footer.y, width, 1)
        } else {
            Rect::default()
        };
        let scroll_targets = groups.iter().map(|group| group.start).collect();
        Self {
            scroll_targets,
            groups,
            selected,
            area,
        }
    }

    pub fn target(&self, forward: bool) -> Option<usize> {
        if self.groups.is_empty() {
            return None;
        }
        match (self.selected, forward) {
            (None, true) => Some(0),
            (None, false) => Some(self.groups.len() - 1),
            (Some(i), true) => (i + 1 < self.groups.len()).then_some(i + 1),
            (Some(i), false) => i.checked_sub(1),
        }
    }

    pub fn hit(&self, x: u16, y: u16) -> Option<bool> {
        if self.area.width == 0 || y != self.area.y {
            return None;
        }
        if x >= self.area.x && x < self.area.x + 3 {
            return Some(false);
        }
        if x >= self.area.right() - 3 && x < self.area.right() {
            return Some(true);
        }
        None
    }
}

/// Place the group near the viewport center using rendered row heights, so
/// wrapped context and annotation overlays cannot push the changes off screen.
pub fn centered_scroll(rows: &[(bool, usize)], group: &Range<usize>, viewport: usize) -> usize {
    let height: usize = rows[group.clone()].iter().map(|row| row.1.max(1)).sum();
    let mut space_above = viewport.saturating_sub(height) / 2;
    let mut start = group.start;
    while start > 0 {
        let previous_height = rows[start - 1].1.max(1);
        if previous_height > space_above {
            break;
        }
        space_above -= previous_height;
        start -= 1;
    }
    start
}

/// macOS terminals can send Option+arrows as the readline word motions Esc+b/f.
/// Only use these aliases in the diff view, never while editing or searching.
pub fn navigation_key(key: crossterm::event::KeyEvent) -> Option<bool> {
    use crossterm::event::{KeyCode, KeyModifiers};
    if key.modifiers != KeyModifiers::ALT {
        return None;
    }
    match key.code {
        KeyCode::Right | KeyCode::Char('f') => Some(true),
        KeyCode::Left | KeyCode::Char('b') => Some(false),
        _ => None,
    }
}

pub fn flash_strength(elapsed: std::time::Duration) -> f32 {
    (1.0 - elapsed.as_secs_f32()).clamp(0.0, 1.0)
}

/// Paint in the surrounding gutters and available whitespace only. Code, line
/// numbers, and titles remain intact; no rows are inserted while the flash fades.
pub fn draw_flash(
    frame: &mut ratatui::Frame,
    area: Rect,
    strength: f32,
    bg: ratatui::style::Color,
) {
    use ratatui::{layout::Position, style::Color};
    if strength <= 0.0 || area.width < 2 || area.height < 2 {
        return;
    }
    let bounds = frame.area();
    let area = area.intersection(bounds);
    if area.width < 2 || area.height < 2 {
        return;
    }
    let paint = |frame: &mut ratatui::Frame, x, y, symbol: &str| {
        let Some(cell) = frame.buffer_mut().cell_mut(Position::new(x, y)) else {
            return;
        };
        let blank = cell.symbol().chars().all(char::is_whitespace);
        let border = matches!(
            cell.symbol(),
            "│" | "─" | "┌" | "┐" | "└" | "┘" | "┬" | "┴" | "├" | "┤" | "▎"
        );
        if !blank && !border {
            return;
        }
        let base = if blank { cell.bg } else { cell.fg };
        let (r, g, b) = match base {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => match bg {
                Color::Rgb(r, g, b) => (r, g, b),
                _ => (30, 30, 30),
            },
        };
        let mix = |from: u8, to: u8| (from as f32 + (to as f32 - from as f32) * strength) as u8;
        cell.set_symbol(symbol)
            .set_fg(Color::Rgb(mix(r, 214), mix(g, 178), mix(b, 87)));
    };
    for y in [area.y, area.bottom() - 1] {
        let clear = (area.x + 1..area.right() - 1).all(|x| {
            frame
                .buffer_mut()
                .cell(Position::new(x, y))
                .is_some_and(|cell| {
                    cell.symbol().chars().all(char::is_whitespace)
                        || matches!(cell.symbol(), "─" | "┬" | "┴")
                })
        });
        if clear {
            for x in area.x + 1..area.right() - 1 {
                paint(frame, x, y, "─");
            }
        }
    }
    for y in area.y + 1..area.bottom() - 1 {
        paint(frame, area.x, y, "│");
        paint(frame, area.right() - 1, y, "│");
    }
    paint(frame, area.x, area.y, "╭");
    paint(frame, area.right() - 1, area.y, "╮");
    paint(frame, area.x, area.bottom() - 1, "╰");
    paint(frame, area.right() - 1, area.bottom() - 1, "╯");
}

/// Keep complete change runs together when possible. Merge neighboring runs only
/// when their intervening context also fits; split oversized runs into pages.
pub fn group_changes(rows: &[(bool, usize)], budget: usize) -> Vec<Range<usize>> {
    let budget = budget.max(1);
    let mut offsets = vec![0usize];
    for &(_, height) in rows {
        offsets.push(offsets.last().unwrap() + height.max(1));
    }
    let mut groups: Vec<Range<usize>> = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        if !rows[i].0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && rows[i].0 {
            i += 1;
        }
        let end = i;
        if offsets[end] - offsets[start] <= budget {
            if let Some(last) = groups.last_mut() {
                if offsets[end] - offsets[last.start] <= budget {
                    last.end = end;
                    continue;
                }
            }
            groups.push(start..end);
        } else {
            let mut page = start;
            for row in start..end {
                if row > page && offsets[row + 1] - offsets[page] > budget {
                    groups.push(page..row);
                    page = row;
                }
            }
            groups.push(page..end);
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_word_motion_aliases_and_arrow_sequences_navigate() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        for (code, forward) in [
            (KeyCode::Right, true),
            (KeyCode::Left, false),
            (KeyCode::Char('f'), true),
            (KeyCode::Char('b'), false),
        ] {
            assert_eq!(
                navigation_key(KeyEvent::new(code, KeyModifiers::ALT)),
                Some(forward)
            );
            assert_eq!(
                navigation_key(KeyEvent::new(code, KeyModifiers::NONE)),
                None
            );
            assert_eq!(
                navigation_key(KeyEvent::new(code, KeyModifiers::CONTROL)),
                None
            );
        }
    }

    #[test]
    fn flash_fades_to_zero_in_one_second() {
        use std::time::Duration;
        assert_eq!(flash_strength(Duration::ZERO), 1.0);
        assert_eq!(flash_strength(Duration::from_millis(500)), 0.5);
        assert_eq!(flash_strength(Duration::from_secs(1)), 0.0);
        assert_eq!(flash_strength(Duration::from_secs(5)), 0.0);
    }

    #[test]
    fn border_fades_without_overwriting_code_or_moving_rows() {
        use ratatui::{backend::TestBackend, style::Color, widgets::Paragraph, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(24, 6)).unwrap();
        let render = |terminal: &mut Terminal<TestBackend>, strength| {
            terminal
                .draw(|frame| {
                    frame.render_widget(Paragraph::new(" let value = 42;"), Rect::new(0, 0, 24, 1));
                    draw_flash(
                        frame,
                        Rect::new(0, 0, 24, 6),
                        strength,
                        Color::Rgb(30, 30, 30),
                    );
                })
                .unwrap();
        };
        render(&mut terminal, 1.0);
        let text: String = (1..16)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect();
        assert_eq!(text, "let value = 42;");
        assert_eq!(terminal.backend().buffer()[(0, 5)].symbol(), "╰");
        let bright = terminal.backend().buffer()[(0, 5)].fg;
        render(&mut terminal, 0.5);
        assert_ne!(terminal.backend().buffer()[(0, 5)].fg, bright);
        render(&mut terminal, 0.0);
        assert_eq!(terminal.backend().buffer()[(0, 5)].symbol(), " ");
    }

    #[test]
    fn centers_groups_and_clamps_at_start_of_file() {
        let rows = vec![(false, 1); 100];
        assert_eq!(centered_scroll(&rows, &(40..50), 30), 30);
        assert_eq!(centered_scroll(&rows, &(3..5), 30), 0);
        assert_eq!(centered_scroll(&rows, &(40..70), 30), 40);
        assert_eq!(centered_scroll(&rows, &(40..80), 30), 40);
    }

    #[test]
    fn centering_accounts_for_wrapped_context_and_overlays() {
        let rows = [(false, 1), (false, 8), (false, 3), (true, 2), (true, 2)];
        assert_eq!(centered_scroll(&rows, &(3..5), 12), 2);
        assert_eq!(centered_scroll(&rows, &(3..5), 8), 3);
    }

    #[test]
    fn nearby_runs_fit_but_distant_changes_stay_separate() {
        let rows: Vec<_> = (0..100)
            .map(|i| ([2, 3, 9, 10, 80].contains(&i), 1))
            .collect();
        assert_eq!(group_changes(&rows, 12), vec![2..11, 80..81]);
        assert_eq!(group_changes(&rows, 8), vec![2..4, 9..11, 80..81]);
    }

    #[test]
    fn long_runs_and_wrapped_rows_are_split_without_losing_changes() {
        assert_eq!(
            group_changes(&vec![(true, 1); 12], 5),
            vec![0..5, 5..10, 10..12]
        );
        assert_eq!(
            group_changes(&[(true, 3), (true, 3), (false, 2), (true, 2)], 5),
            vec![0..1, 1..2, 3..4]
        );
        assert_eq!(group_changes(&[(true, 20), (true, 1)], 5), vec![0..1, 1..2]);
        assert!(group_changes(&[(false, 1); 8], 0).is_empty());
    }

    #[test]
    fn arrows_are_centered_and_stop_at_file_boundaries() {
        let nav = ChangeNavigation::new(vec![3..5, 80..90], Some(4), Rect::new(0, 39, 120, 1));
        assert_eq!(nav.target(false), None);
        assert_eq!(nav.target(true), Some(1));
        assert_eq!(nav.hit(nav.area.x, 39), Some(false));
        assert_eq!(nav.hit(nav.area.right() - 1, 39), Some(true));
        assert_eq!(nav.hit(60, 38), None);
        assert!((nav.area.x * 2 + nav.area.width).abs_diff(120) <= 1);
        let empty = ChangeNavigation::new(vec![], None, Rect::new(0, 0, 4, 1));
        assert_eq!(empty.target(true), None);
        assert_eq!(empty.hit(0, 0), None);
    }
}
