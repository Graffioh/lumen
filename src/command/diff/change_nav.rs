use std::ops::Range;

use ratatui::layout::Rect;

/// Navigation targets are side-by-side row ranges, sized for the current viewport.
#[derive(Default)]
pub struct ChangeNavigation {
    pub groups: Vec<Range<usize>>,
    pub selected: Option<usize>,
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
        Self {
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
