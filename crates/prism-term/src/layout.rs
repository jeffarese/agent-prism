//! Dock height computation. See docs/DESIGN.md §7.6.

use prism_core::model::DockMode;

/// Minimum number of rows the child always keeps.
pub const MIN_CHILD_ROWS: u16 = 5;

/// The effective (non-auto) mode for a terminal of `rows` rows.
pub fn resolve_mode(mode: DockMode, rows: u16, shown_accounts: usize) -> DockMode {
    match mode {
        DockMode::Auto => {
            let full = full_height(shown_accounts);
            if rows >= 30 && full <= rows / 5 {
                DockMode::Full
            } else {
                DockMode::Compact
            }
        }
        m => m,
    }
}

fn full_height(shown_accounts: usize) -> u16 {
    u16::try_from(shown_accounts)
        .unwrap_or(u16::MAX)
        .saturating_add(2)
}

/// Dock height for `rows` terminal rows. `panel_height` is the preferred
/// height of an open panel and overrides the mode. The child always keeps at
/// least [`MIN_CHILD_ROWS`] rows (the dock shrinks to 0 when that is
/// impossible).
pub fn dock_height(
    mode: DockMode,
    rows: u16,
    shown_accounts: usize,
    panel_height: Option<u16>,
) -> u16 {
    let want = match panel_height {
        Some(h) => h,
        None => match resolve_mode(mode, rows, shown_accounts) {
            DockMode::Hidden => 0,
            DockMode::Compact => 1,
            _ => full_height(shown_accounts),
        },
    };
    want.min(rows.saturating_sub(MIN_CHILD_ROWS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        assert_eq!(dock_height(DockMode::Hidden, 40, 2, None), 0);
        assert_eq!(dock_height(DockMode::Compact, 40, 2, None), 1);
        assert_eq!(dock_height(DockMode::Full, 40, 2, None), 4);
        assert_eq!(dock_height(DockMode::Full, 40, 0, None), 2);
    }

    #[test]
    fn auto() {
        assert_eq!(dock_height(DockMode::Auto, 40, 2, None), 4);
        assert_eq!(dock_height(DockMode::Auto, 29, 2, None), 1);
        assert_eq!(dock_height(DockMode::Auto, 30, 5, None), 1);
        assert_eq!(dock_height(DockMode::Auto, 35, 5, None), 7);
    }

    #[test]
    fn child_keeps_five_rows() {
        assert_eq!(dock_height(DockMode::Full, 8, 5, None), 3);
        assert_eq!(dock_height(DockMode::Compact, 5, 1, None), 0);
        assert_eq!(dock_height(DockMode::Compact, 3, 1, None), 0);
        assert_eq!(dock_height(DockMode::Hidden, 40, 1, Some(24)), 24);
        assert_eq!(dock_height(DockMode::Hidden, 20, 1, Some(24)), 15);
    }
}
