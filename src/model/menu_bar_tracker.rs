//! Decides whether an app has hidden the macOS menu bar on one display.
//!
//! The window server's menu bar window slides above the display when an app hides
//! the menu bar (e.g. Ghostty's non-native fullscreen) and slides back when it
//! returns. Hovering at the top edge produces the same slide-in, so the cursor
//! position tells the two apart.

use std::time::{Duration, Instant};

use objc2_core_foundation::{CGPoint, CGRect};

/// How long the cursor must stay out of the menu bar strip, with the menu bar still
/// shown, before a hover reveal counts as the menu bar being back.
pub const HOVER_GRACE: Duration = Duration::from_secs(1);
/// How often a hover-revealed menu bar is re-checked.
pub const HOVER_TICK: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Visible,
    Hidden,
    HoverRevealed { cursor_left_at: Option<Instant> },
}

#[derive(Debug, Clone)]
pub struct MenuBarTracker {
    display_top: f64,
    phase: Phase,
}

impl MenuBarTracker {
    pub fn new(display_top: f64, bar: CGRect) -> Self {
        let phase = if fully_hidden(display_top, bar) {
            Phase::Hidden
        } else {
            Phase::Visible
        };
        Self { display_top, phase }
    }

    pub fn app_hidden(&self) -> bool { self.phase != Phase::Visible }

    pub fn needs_tick(&self) -> bool { matches!(self.phase, Phase::HoverRevealed { .. }) }

    /// Feeds the current menu bar frame and cursor; returns true when `app_hidden` changed.
    pub fn observe(&mut self, bar: CGRect, cursor: Option<CGPoint>, now: Instant) -> bool {
        let was_hidden = self.app_hidden();
        let hidden = fully_hidden(self.display_top, bar);
        // An unknown cursor counts as outside, so a returning menu bar is never covered.
        let in_strip =
            cursor.is_some_and(|cursor| in_menu_bar_strip(self.display_top, bar, cursor));

        self.phase = match self.phase {
            _ if hidden => Phase::Hidden,
            // Sliding out from Visible: wait until it is fully off-screen.
            Phase::Visible => Phase::Visible,
            Phase::Hidden if in_strip => Phase::HoverRevealed { cursor_left_at: None },
            Phase::Hidden => Phase::Visible,
            Phase::HoverRevealed { .. } if in_strip => {
                Phase::HoverRevealed { cursor_left_at: None }
            }
            Phase::HoverRevealed { cursor_left_at } => {
                let left_at = cursor_left_at.unwrap_or(now);
                if now.duration_since(left_at) >= HOVER_GRACE {
                    Phase::Visible
                } else {
                    Phase::HoverRevealed { cursor_left_at: Some(left_at) }
                }
            }
        };

        was_hidden != self.app_hidden()
    }
}

/// The menu bar's bottom edge is at or above the display's top edge.
fn fully_hidden(display_top: f64, bar: CGRect) -> bool {
    bar.origin.y + bar.size.height <= display_top + 0.5
}

/// The cursor is within the band a shown menu bar occupies on this display.
fn in_menu_bar_strip(display_top: f64, bar: CGRect, cursor: CGPoint) -> bool {
    cursor.x >= bar.origin.x
        && cursor.x < bar.origin.x + bar.size.width
        && cursor.y >= display_top
        && cursor.y < display_top + bar.size.height
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    use super::{HOVER_GRACE, MenuBarTracker};

    const TOP: f64 = 0.0;
    const BAR_HEIGHT: f64 = 38.0;

    fn bar_at(y: f64) -> CGRect {
        CGRect::new(CGPoint::new(0.0, y), CGSize::new(1920.0, BAR_HEIGHT))
    }
    fn shown() -> CGRect { bar_at(0.0) }
    fn hidden() -> CGRect { bar_at(-BAR_HEIGHT) }
    fn sliding() -> CGRect { bar_at(-20.0) }
    fn in_strip() -> Option<CGPoint> { Some(CGPoint::new(500.0, 2.0)) }
    fn below_strip() -> Option<CGPoint> { Some(CGPoint::new(500.0, 600.0)) }

    fn hidden_tracker() -> MenuBarTracker { MenuBarTracker::new(TOP, hidden()) }

    #[test]
    fn starts_from_the_current_menu_bar_position() {
        assert!(MenuBarTracker::new(TOP, hidden()).app_hidden());
        assert!(!MenuBarTracker::new(TOP, shown()).app_hidden());
    }

    #[test]
    fn hides_only_once_the_bar_is_fully_off_screen() {
        let now = Instant::now();
        let mut t = MenuBarTracker::new(TOP, shown());
        assert!(!t.observe(sliding(), below_strip(), now));
        assert!(!t.app_hidden());
        assert!(t.observe(hidden(), below_strip(), now));
        assert!(t.app_hidden());
    }

    #[test]
    fn slide_in_with_cursor_elsewhere_is_visible_immediately() {
        let mut t = hidden_tracker();
        assert!(t.observe(sliding(), below_strip(), Instant::now()));
        assert!(!t.app_hidden());
        assert!(!t.needs_tick());
    }

    #[test]
    fn slide_in_with_cursor_in_strip_is_a_hover() {
        let mut t = hidden_tracker();
        assert!(!t.observe(sliding(), in_strip(), Instant::now()));
        assert!(t.app_hidden());
        assert!(t.needs_tick());
    }

    #[test]
    fn hover_ends_when_macos_hides_the_bar_again() {
        let now = Instant::now();
        let mut t = hidden_tracker();
        t.observe(sliding(), in_strip(), now);
        assert!(!t.observe(hidden(), below_strip(), now + Duration::from_millis(400)));
        assert!(t.app_hidden());
        assert!(!t.needs_tick());
    }

    #[test]
    fn bar_staying_after_cursor_leaves_shows_it_after_the_grace_period() {
        let t0 = Instant::now();
        let mut t = hidden_tracker();
        t.observe(sliding(), in_strip(), t0);
        t.observe(shown(), in_strip(), t0);

        assert!(!t.observe(shown(), below_strip(), t0 + Duration::from_millis(100)));
        assert!(!t.observe(shown(), below_strip(), t0 + Duration::from_millis(600)));
        assert!(t.app_hidden());
        assert!(t.observe(
            shown(),
            below_strip(),
            t0 + Duration::from_millis(100) + HOVER_GRACE
        ));
        assert!(!t.app_hidden());
        assert!(!t.needs_tick());
    }

    #[test]
    fn returning_to_the_strip_restarts_the_grace_period() {
        let t0 = Instant::now();
        let mut t = hidden_tracker();
        t.observe(shown(), in_strip(), t0);
        t.observe(shown(), below_strip(), t0 + Duration::from_millis(100));
        t.observe(shown(), in_strip(), t0 + Duration::from_millis(900));
        t.observe(shown(), below_strip(), t0 + Duration::from_millis(1000));
        assert!(!t.observe(shown(), below_strip(), t0 + Duration::from_millis(1500)));
        assert!(t.app_hidden());
        assert!(t.observe(
            shown(),
            below_strip(),
            t0 + Duration::from_millis(1000) + HOVER_GRACE
        ));
    }

    #[test]
    fn unknown_cursor_counts_as_outside_the_strip() {
        let mut t = hidden_tracker();
        assert!(t.observe(sliding(), None, Instant::now()));
        assert!(!t.app_hidden());
    }

    #[test]
    fn cursor_in_another_displays_strip_is_not_a_hover() {
        // Display to the right starts at x=1920; its top strip is y in [0, 38).
        let mut t = hidden_tracker();
        let other_display_strip = Some(CGPoint::new(2500.0, 2.0));
        assert!(t.observe(sliding(), other_display_strip, Instant::now()));
        assert!(!t.app_hidden());
    }

    #[test]
    fn respects_the_display_origin() {
        // A display below the main one, whose top edge is at y=1243.
        let top = 1243.0;
        let bar = |y: f64| CGRect::new(CGPoint::new(0.0, y), CGSize::new(1920.0, BAR_HEIGHT));
        let mut t = MenuBarTracker::new(top, bar(top - BAR_HEIGHT));
        assert!(t.app_hidden());
        assert!(!t.observe(
            bar(top - 10.0),
            Some(CGPoint::new(10.0, top + 1.0)),
            Instant::now()
        ));
        assert!(t.needs_tick());
    }
}
