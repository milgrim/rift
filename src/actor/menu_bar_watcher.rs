//! Watches the window server's menu bar windows and marks displays whose menu bar
//! an app has hidden, so their tiling area includes the menu bar strip.
//! The hover handling lives in [`crate::model::menu_bar_tracker`].

use std::sync::mpsc;
use std::time::Instant;

use objc2_core_foundation::{CGPoint, CGRect};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use tracing::debug;

use super::spaces;
use crate::common::collections::HashMap;
use crate::model::menu_bar_tracker::{HOVER_TICK, MenuBarTracker};
use crate::sys::screen::{self, display_for_menu_bar};
use crate::sys::window_notify;
use crate::sys::window_server::{self, WindowServerId};

/// What the watcher needs from macOS and the rest of Rift; faked in tests.
pub trait MenuBarSystem: Send + 'static {
    fn menu_bar_windows(&self) -> Vec<(WindowServerId, CGRect)>;
    fn window_bounds(&self, id: WindowServerId) -> Option<CGRect>;
    fn display_bounds(&self) -> Vec<(u32, CGRect)>;
    fn cursor(&self) -> Option<CGPoint>;
    /// Returns true when the stored flag changed.
    fn set_app_hidden(&self, display: u32, hidden: bool) -> bool;
    fn pin_notifications(&self, window_ids: &[u32]);
    fn request_screen_refresh(&self);
}

struct Entry {
    display: u32,
    tracker: MenuBarTracker,
}

pub struct MenuBarWatcher<S: MenuBarSystem> {
    system: S,
    entries: HashMap<WindowServerId, Entry>,
}

impl<S: MenuBarSystem> MenuBarWatcher<S> {
    pub fn new(system: S) -> Self {
        Self {
            system,
            entries: HashMap::default(),
        }
    }

    /// Finds the menu bar windows again, e.g. after displays changed.
    pub fn rediscover(&mut self) {
        let displays = self.system.display_bounds();
        let mut entries = HashMap::default();
        for (id, bar) in self.system.menu_bar_windows() {
            let Some((display, bounds)) = display_for_menu_bar(bar, &displays)
                .and_then(|did| displays.iter().find(|(d, _)| *d == did).copied())
            else {
                debug!(?id, ?bar, "menu bar window matches no display");
                continue;
            };
            entries.insert(id, Entry {
                display,
                tracker: MenuBarTracker::new(bounds.origin.y, bar),
            });
        }

        let mut changed = false;
        for old in self.entries.values() {
            if !entries.values().any(|entry: &Entry| entry.display == old.display) {
                changed |= self.system.set_app_hidden(old.display, false);
            }
        }
        for entry in entries.values() {
            changed |= self.system.set_app_hidden(entry.display, entry.tracker.app_hidden());
        }

        self.entries = entries;
        let mut ids: Vec<u32> = self.entries.keys().map(|id| id.as_u32()).collect();
        ids.sort_unstable();
        self.system.pin_notifications(&ids);
        if changed {
            self.system.request_screen_refresh();
        }
    }

    /// Returns false when `id` is not a menu bar window.
    pub fn on_window_moved(&mut self, id: WindowServerId, now: Instant) -> bool {
        if !self.entries.contains_key(&id) {
            return false;
        }
        self.observe(id, now);
        true
    }

    pub fn tick(&mut self, now: Instant) {
        let hovering: Vec<WindowServerId> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.tracker.needs_tick())
            .map(|(id, _)| *id)
            .collect();
        for id in hovering {
            self.observe(id, now);
        }
    }

    fn observe(&mut self, id: WindowServerId, now: Instant) {
        let Some(bar) = self.system.window_bounds(id) else {
            return;
        };
        let cursor = self.system.cursor();
        let Some(entry) = self.entries.get_mut(&id) else { return };
        if entry.tracker.observe(bar, cursor, now)
            && self.system.set_app_hidden(entry.display, entry.tracker.app_hidden())
        {
            debug!(
                display = entry.display,
                app_hidden = entry.tracker.app_hidden(),
                "menu bar changed"
            );
            self.system.request_screen_refresh();
        }
    }

    pub fn needs_tick(&self) -> bool {
        self.entries.values().any(|entry| entry.tracker.needs_tick())
    }
}

struct ActualSystem {
    spaces_tx: spaces::Sender,
}

impl MenuBarSystem for ActualSystem {
    fn menu_bar_windows(&self) -> Vec<(WindowServerId, CGRect)> {
        window_server::menu_bar_windows()
    }

    fn window_bounds(&self, id: WindowServerId) -> Option<CGRect> {
        window_server::window_bounds(id)
    }

    fn display_bounds(&self) -> Vec<(u32, CGRect)> { screen::active_display_bounds() }

    fn cursor(&self) -> Option<CGPoint> { window_server::current_cursor_location().ok() }

    fn set_app_hidden(&self, display: u32, hidden: bool) -> bool {
        screen::set_menu_bar_app_hidden(display, hidden)
    }

    fn pin_notifications(&self, window_ids: &[u32]) {
        window_notify::set_pinned_window_notifications(window_ids);
    }

    fn request_screen_refresh(&self) { self.spaces_tx.send(spaces::Event::ScreenRefreshRequested); }
}

static WATCHER: OnceCell<Mutex<MenuBarWatcher<ActualSystem>>> = OnceCell::new();
static TICK_WAKE: OnceCell<mpsc::SyncSender<()>> = OnceCell::new();

/// Starts watching; call once after the spaces actor exists.
pub fn init(spaces_tx: spaces::Sender) {
    if WATCHER
        .set(Mutex::new(MenuBarWatcher::new(ActualSystem { spaces_tx })))
        .is_err()
    {
        return;
    }
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    let _ = TICK_WAKE.set(wake_tx);
    std::thread::Builder::new()
        .name("menu-bar-hover".to_string())
        .spawn(move || run_hover_ticks(wake_rx))
        .expect("failed to spawn menu bar hover thread");
    rediscover();
}

pub fn rediscover() {
    let Some(watcher) = WATCHER.get() else { return };
    let mut watcher = watcher.lock();
    watcher.rediscover();
    wake_ticks_if_needed(&watcher);
}

/// Returns false when `id` is not a menu bar window; such moves are ignored.
pub fn on_window_moved(id: WindowServerId) -> bool {
    let Some(watcher) = WATCHER.get() else { return false };
    let mut watcher = watcher.lock();
    let handled = watcher.on_window_moved(id, Instant::now());
    wake_ticks_if_needed(&watcher);
    handled
}

fn wake_ticks_if_needed(watcher: &MenuBarWatcher<ActualSystem>) {
    if watcher.needs_tick()
        && let Some(wake) = TICK_WAKE.get()
    {
        let _ = wake.try_send(());
    }
}

/// Sleeps until a hover starts, then ticks until no display is hover-revealed.
fn run_hover_ticks(wake_rx: mpsc::Receiver<()>) {
    while wake_rx.recv().is_ok() {
        loop {
            std::thread::sleep(HOVER_TICK);
            let Some(watcher) = WATCHER.get() else { return };
            let mut watcher = watcher.lock();
            watcher.tick(Instant::now());
            if !watcher.needs_tick() {
                break;
            }
        }
        while wake_rx.try_recv().is_ok() {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use parking_lot::Mutex;

    use super::{MenuBarSystem, MenuBarWatcher};
    use crate::common::collections::{HashMap, HashSet};
    use crate::model::menu_bar_tracker::HOVER_GRACE;
    use crate::sys::window_server::WindowServerId;

    const MENU_BAR: u32 = 26;
    const DISPLAY: u32 = 1;

    #[derive(Default)]
    struct State {
        bars: HashMap<u32, CGRect>,
        displays: Vec<(u32, CGRect)>,
        cursor: Option<CGPoint>,
        hidden: HashSet<u32>,
        pinned: Vec<u32>,
        refreshes: usize,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<State>>);

    impl MenuBarSystem for Fake {
        fn menu_bar_windows(&self) -> Vec<(WindowServerId, CGRect)> {
            let state = self.0.lock();
            state.bars.iter().map(|(id, bar)| (WindowServerId::new(*id), *bar)).collect()
        }

        fn window_bounds(&self, id: WindowServerId) -> Option<CGRect> {
            self.0.lock().bars.get(&id.as_u32()).copied()
        }

        fn display_bounds(&self) -> Vec<(u32, CGRect)> { self.0.lock().displays.clone() }

        fn cursor(&self) -> Option<CGPoint> { self.0.lock().cursor }

        fn set_app_hidden(&self, display: u32, hidden: bool) -> bool {
            let mut state = self.0.lock();
            if hidden {
                state.hidden.insert(display)
            } else {
                state.hidden.remove(&display)
            }
        }

        fn pin_notifications(&self, window_ids: &[u32]) {
            self.0.lock().pinned = window_ids.to_vec();
        }

        fn request_screen_refresh(&self) { self.0.lock().refreshes += 1; }
    }

    fn bar_at(y: f64) -> CGRect { CGRect::new(CGPoint::new(0.0, y), CGSize::new(1920.0, 38.0)) }

    fn setup(bar_y: f64) -> (Fake, MenuBarWatcher<Fake>) {
        let fake = Fake::default();
        {
            let mut state = fake.0.lock();
            state.bars.insert(MENU_BAR, bar_at(bar_y));
            state.displays = vec![(
                DISPLAY,
                CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1920.0, 1243.0)),
            )];
            state.cursor = Some(CGPoint::new(500.0, 600.0));
        }
        let mut watcher = MenuBarWatcher::new(fake.clone());
        watcher.rediscover();
        (fake, watcher)
    }

    fn move_bar(fake: &Fake, y: f64) { fake.0.lock().bars.insert(MENU_BAR, bar_at(y)); }

    #[test]
    fn rediscover_pins_menu_bars_and_applies_their_state() {
        let (fake, _watcher) = setup(-38.0);
        let state = fake.0.lock();
        assert_eq!(state.pinned, vec![MENU_BAR]);
        assert!(state.hidden.contains(&DISPLAY));
        assert_eq!(state.refreshes, 1);
    }

    #[test]
    fn rediscover_with_shown_menu_bar_requests_no_refresh() {
        let (fake, _watcher) = setup(0.0);
        assert_eq!(fake.0.lock().refreshes, 0);
        assert!(fake.0.lock().hidden.is_empty());
    }

    #[test]
    fn moves_of_other_windows_are_ignored() {
        let (fake, mut watcher) = setup(0.0);
        assert!(!watcher.on_window_moved(WindowServerId::new(999), Instant::now()));
        assert_eq!(fake.0.lock().refreshes, 0);
    }

    #[test]
    fn hiding_and_returning_menu_bar_toggle_the_display_flag() {
        let (fake, mut watcher) = setup(0.0);
        let now = Instant::now();

        move_bar(&fake, -38.0);
        assert!(watcher.on_window_moved(WindowServerId::new(MENU_BAR), now));
        assert!(fake.0.lock().hidden.contains(&DISPLAY));
        assert_eq!(fake.0.lock().refreshes, 1);

        move_bar(&fake, -20.0);
        watcher.on_window_moved(WindowServerId::new(MENU_BAR), now);
        assert!(!fake.0.lock().hidden.contains(&DISPLAY));
        assert_eq!(fake.0.lock().refreshes, 2);
    }

    #[test]
    fn hover_keeps_the_flag_until_the_grace_period_passes() {
        let (fake, mut watcher) = setup(-38.0);
        let t0 = Instant::now();

        fake.0.lock().cursor = Some(CGPoint::new(500.0, 1.0));
        move_bar(&fake, 0.0);
        watcher.on_window_moved(WindowServerId::new(MENU_BAR), t0);
        assert!(watcher.needs_tick());
        assert!(fake.0.lock().hidden.contains(&DISPLAY));

        fake.0.lock().cursor = Some(CGPoint::new(500.0, 600.0));
        watcher.tick(t0 + Duration::from_millis(100));
        assert!(fake.0.lock().hidden.contains(&DISPLAY));
        watcher.tick(t0 + Duration::from_millis(100) + HOVER_GRACE);
        assert!(!fake.0.lock().hidden.contains(&DISPLAY));
        assert!(!watcher.needs_tick());
    }

    #[test]
    fn rediscover_clears_flags_of_vanished_displays() {
        let (fake, mut watcher) = setup(-38.0);
        {
            let mut state = fake.0.lock();
            state.bars.clear();
            state.displays.clear();
        }
        watcher.rediscover();
        let state = fake.0.lock();
        assert!(state.hidden.is_empty());
        assert!(state.pinned.is_empty());
        assert_eq!(state.refreshes, 2);
    }
}
