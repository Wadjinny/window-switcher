//! The controller thread: runs every switcher command the hook posts, one at
//! a time and in key order (see `input/mod.rs`).

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{atomic::AtomicU32, atomic::Ordering, mpsc, Mutex, OnceLock},
    thread,
    time::Instant,
};

use windows::Win32::{
    Foundation::{LPARAM, WPARAM},
    System::{
        Com::{CoInitializeEx, COINIT_APARTMENTTHREADED},
        Threading::GetCurrentThreadId,
    },
    UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, GetWindowThreadProcessId, KillTimer, PeekMessageW,
        PostThreadMessageW, SetTimer, TranslateMessage, MSG, PM_NOREMOVE, WM_APP, WM_TIMER,
    },
};

use super::{
    items::target_to_item_cached,
    session::{
        active_switcher, advance_switcher, cancel_generation, commit_switcher, emit_and_show,
    },
    ActiveSwitcher, HookRuntime, SwitchMode, RUNTIME,
};
use crate::{
    hotkey::shortcut_for,
    icons::{cache_lookup, icon_cache, needs_item_work, spawn_item_worker},
    input::{
        alt_key_down,
        hook::{close_hook_session, log_hook_timing, APP_TARGETS_HINT, HOOK_SESSION},
        inject::replay_shortcut,
        win_key_down,
    },
    lock_unpoisoned,
    settings_model::{current_card_style, current_layout},
    settings_window::emit_recorded_hotkey,
    show_error_message,
    targets::{
        enumerate::{process_path_for_window, switch_origin, switchable_windows},
        grouping::{application_targets, windows_for_current_app},
        identity::{target_key, window_key},
    },
    timing,
};

/// Work for the controller thread, posted by the hook (and a few others).
pub(crate) enum Command {
    Open {
        mode: SwitchMode,
        reverse: bool,
        generation: u64,
        /// The swallowed key, replayed if nothing opens.
        vk: u32,
        scan_code: u32,
        /// When the hook saw the key, for the latency log.
        at: Instant,
    },
    Advance {
        generation: u64,
        reverse: bool,
        at: Instant,
    },
    Commit {
        generation: u64,
        at: Instant,
    },
    Cancel {
        generation: u64,
        reason: &'static str,
    },
    RecordKey(RecordedKey),
    /// Show the open switcher again (new overlays were created).
    Reshow,
}

/// The raw key captured while recording; named on the controller thread.
#[derive(Clone, Copy)]
pub(crate) struct RecordedKey {
    pub(crate) vk: u32,
    pub(crate) scan_code: u32,
    pub(crate) extended: bool,
    pub(crate) alt: bool,
    pub(crate) win: bool,
    pub(crate) ctrl: bool,
    pub(crate) shift: bool,
}

/// Wakes the controller's message loop; the commands wait in the channel.
const WM_CONTROLLER_WAKE: u32 = WM_APP + 1;

struct ControllerHandle {
    sender: Mutex<mpsc::Sender<Command>>,
    thread_id: AtomicU32,
}

static CONTROLLER: OnceLock<ControllerHandle> = OnceLock::new();

/// Queues `command` for the controller thread. Never blocks for long: the
/// sender lock is only held for the channel push.
pub(crate) fn post_command(command: Command) {
    let Some(controller) = CONTROLLER.get() else {
        return;
    };
    let sent = lock_unpoisoned(&controller.sender).send(command).is_ok();
    let thread_id = controller.thread_id.load(Ordering::SeqCst);
    if sent && thread_id != 0 {
        unsafe {
            let _ = PostThreadMessageW(thread_id, WM_CONTROLLER_WAKE, WPARAM(0), LPARAM(0));
        }
    }
}

/// How often the watchdog checks that the switcher's modifier is still held.
const WATCHDOG_INTERVAL_MS: u32 = 200;
/// How often the controller recounts the apps for `APP_TARGETS_HINT`.
const APP_HINT_INTERVAL_MS: u32 = 2_000;

/// Starts the controller thread. Commands posted before it runs wait in the
/// channel. Must be called before the hook is installed.
pub(crate) fn start_controller() {
    let (sender, receiver) = mpsc::channel();
    let _ = CONTROLLER.set(ControllerHandle {
        sender: Mutex::new(sender),
        thread_id: AtomicU32::new(0),
    });
    let spawned = thread::Builder::new()
        .name("switcher-controller".into())
        .spawn(move || run_controller(receiver));
    if let Err(error) = spawned {
        let message = format!("Window Switcher could not start its switcher thread.\n\n{error}");
        log::error!("{message}");
        show_error_message(&message);
        std::process::exit(1);
    }
}

pub(crate) fn run_controller(receiver: mpsc::Receiver<Command>) {
    let Some(runtime) = RUNTIME.get() else {
        log::error!("the switcher controller started without a runtime");
        return;
    };
    let Some(handle) = CONTROLLER.get() else {
        return;
    };
    unsafe {
        // Window identity (AppUserModelID) and shell names use COM. This
        // thread pumps messages, as a single-threaded apartment should.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        // Create the message queue before publishing the thread id, so no
        // wake-up message can be lost; then drain what was already queued.
        let mut message = MSG::default();
        let _ = PeekMessageW(&mut message, None, WM_APP, WM_APP, PM_NOREMOVE);
        handle
            .thread_id
            .store(GetCurrentThreadId(), Ordering::SeqCst);
    }
    let mut controller = Controller {
        runtime,
        receiver,
        pending: VecDeque::new(),
        watchdog_timer: 0,
        watchdog_misses: 0,
        app_hint_timer: unsafe { SetTimer(None, 0, APP_HINT_INTERVAL_MS, None) },
        app_keys: HashMap::new(),
    };
    controller.refresh_app_hint();
    controller.drain();
    let mut message = MSG::default();
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            if message.hwnd.0.is_null() {
                if message.message == WM_CONTROLLER_WAKE {
                    controller.drain();
                    continue;
                }
                if message.message == WM_TIMER {
                    controller.on_timer(message.wParam.0);
                    continue;
                }
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// Owns the switcher session: everything the hook asks for happens here, one
/// command at a time, in the order the keys were pressed.
struct Controller {
    runtime: &'static HookRuntime,
    receiver: mpsc::Receiver<Command>,
    /// Commands taken from the channel but not handled yet, so an opening can
    /// see that its release is already queued.
    pending: VecDeque<Command>,
    watchdog_timer: usize,
    watchdog_misses: u8,
    app_hint_timer: usize,
    /// HWND -> (process id, app key), so recounting apps only queries new
    /// windows.
    app_keys: HashMap<isize, (u32, String)>,
}

impl Controller {
    fn receive(&mut self) {
        while let Ok(command) = self.receiver.try_recv() {
            self.pending.push_back(command);
        }
    }

    fn drain(&mut self) {
        self.receive();
        while let Some(command) = self.pending.pop_front() {
            // A panic in one command must not take the switcher down with it.
            let handled =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle(command)));
            if handled.is_err() {
                log::error!("the switcher controller panicked while handling a command");
            }
            self.receive();
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Open {
                mode,
                reverse,
                generation,
                vk,
                scan_code,
                at,
            } => self.open(mode, reverse, generation, (vk, scan_code), at),
            Command::Advance {
                generation,
                reverse,
                at,
            } => {
                let mut watch = timing::Stopwatch::start_at("advance", at);
                timing::mark(&mut watch, "dequeued");
                advance_switcher(self.runtime, generation, reverse, &mut watch);
                timing::finish(watch, "");
            }
            Command::Commit { generation, at } => {
                self.stop_watchdog();
                commit_switcher(self.runtime, generation, at);
                log_hook_timing();
                self.refresh_app_hint();
            }
            Command::Cancel { generation, reason } => {
                self.stop_watchdog();
                if cancel_generation(self.runtime, generation) {
                    log::info!("switcher closed: {reason}");
                }
                log_hook_timing();
            }
            Command::RecordKey(key) => emit_recorded_hotkey(self.runtime, &key),
            Command::Reshow => emit_and_show(self.runtime, &mut None),
        }
    }

    /// Whether a later command already closes or replaces opening
    /// `generation` (`(closes, commits)`).
    fn queued_close(&self, generation: u64) -> (bool, bool) {
        let mut closes = false;
        let mut commits = false;
        for command in &self.pending {
            match command {
                Command::Commit { generation: g, .. } if *g == generation => {
                    closes = true;
                    commits = true;
                }
                Command::Cancel { generation: g, .. } if *g == generation => closes = true,
                Command::Open { generation: g, .. } if *g != generation => closes = true,
                _ => {}
            }
        }
        (closes, commits)
    }

    fn open(
        &mut self,
        mode: SwitchMode,
        reverse: bool,
        generation: u64,
        key: (u32, u32),
        at: Instant,
    ) {
        let mut watch = timing::Stopwatch::start_at("open", at);
        timing::mark(&mut watch, "dequeued");
        self.receive();
        let (closed, committed) = self.queued_close(generation);
        if closed && !committed {
            // Escape or another opening is already queued: nothing to show.
            return;
        }
        let origin = switch_origin();
        let targets = match (mode, origin) {
            (SwitchMode::Window, Some((active, _))) => windows_for_current_app(active),
            (SwitchMode::App, Some((active, _))) => application_targets(active),
            (_, None) => Vec::new(),
        };
        timing::mark(&mut watch, "enumerated");
        let from_own_window = origin.is_some_and(|(_, own)| own);
        if mode == SwitchMode::App {
            APP_TARGETS_HINT.store(targets.len(), Ordering::SeqCst);
        }
        let min_targets = if mode == SwitchMode::Window || from_own_window {
            1
        } else {
            2
        };
        if targets.len() < min_targets {
            log::debug!("nothing to switch to for mode {}", mode.as_str());
            // The hook swallowed the shortcut on a stale guess; hand it to
            // Windows after all (also after a quick tap whose release is
            // queued), unless Escape or another opening already ended it.
            if close_hook_session(generation) || committed {
                replay_shortcut(shortcut_for(mode.as_str()).modifier, key.0, key.1);
            }
            return;
        }

        let worker_targets = (!committed && needs_item_work(&targets)).then(|| targets.clone());
        let needs_work = worker_targets.is_some();
        let card_style = current_card_style();
        let items = targets
            .iter()
            .map(|target| target_to_item_cached(target, card_style))
            .collect::<Vec<_>>();
        let selected = if reverse {
            targets.len() - 1
        } else if from_own_window {
            0
        } else {
            1 % targets.len()
        };
        let hint = shortcut_for(mode.as_str()).hint();
        timing::mark(&mut watch, "items");
        if let Some(watch) = &watch {
            timing::opened(generation, watch.start_instant());
        }
        let count = targets.len();
        let icons_cached = if watch.is_some() {
            targets
                .iter()
                .filter(|target| cache_lookup(icon_cache(), &target_key(target)).is_some())
                .count()
        } else {
            0
        };
        if let Ok(mut state) = self.runtime.state.lock() {
            state.active = Some(ActiveSwitcher {
                mode: mode.as_str().to_string(),
                hint,
                generation,
                card_style,
                layout: current_layout(),
                targets,
                items,
                selected,
            });
        } else {
            return;
        }
        if committed {
            // A quick tap: the release is already queued, so skip drawing an
            // overlay that would be hidden again right away.
            timing::finish(watch, &format!("#{generation} quick tap, not shown"));
            return;
        }
        emit_and_show(self.runtime, &mut watch);
        timing::finish(
            watch,
            &format!(
                "#{generation} mode={} cards={count} icons_cached={icons_cached} worker={needs_work}",
                mode.as_str()
            ),
        );
        if let Some(targets) = worker_targets {
            spawn_item_worker(self.runtime, generation, targets);
        }
        self.start_watchdog();
    }

    /// Starts the watchdog that closes the overlay if its modifier is
    /// released without the hook seeing it (no key event follows a release
    /// lost to the secure desktop). The timer runs on this thread's loop.
    fn start_watchdog(&mut self) {
        self.watchdog_misses = 0;
        if self.watchdog_timer == 0 {
            self.watchdog_timer = unsafe { SetTimer(None, 0, WATCHDOG_INTERVAL_MS, None) };
        }
    }

    fn stop_watchdog(&mut self) {
        if self.watchdog_timer != 0 {
            unsafe {
                let _ = KillTimer(None, self.watchdog_timer);
            }
            self.watchdog_timer = 0;
        }
    }

    fn on_timer(&mut self, id: usize) {
        if id == self.app_hint_timer {
            self.refresh_app_hint();
        } else if id == self.watchdog_timer {
            self.check_watchdog();
        }
    }

    /// Requires two misses in a row so a single odd read never closes it.
    fn check_watchdog(&mut self) {
        let Some((generation, mode)) = active_switcher(self.runtime) else {
            self.stop_watchdog();
            return;
        };
        if shortcut_for(&mode)
            .modifier
            .is_down(alt_key_down(), win_key_down())
        {
            self.watchdog_misses = 0;
            return;
        }
        self.watchdog_misses += 1;
        // Only close a session the hook still has open; otherwise its own
        // commit or cancel is already queued.
        if self.watchdog_misses >= 2 && close_hook_session(generation) {
            log::warn!("switcher modifier was released unseen; closing the overlay");
            self.stop_watchdog();
            cancel_generation(self.runtime, generation);
        }
    }

    /// Recounts the apps Win+Tab would show, for the hook's swallow decision.
    /// Only windows not seen before are queried for their process and app id.
    fn refresh_app_hint(&mut self) {
        if HOOK_SESSION.load(Ordering::SeqCst) != 0 {
            return;
        }
        let mut seen = HashSet::new();
        let mut apps = HashSet::new();
        for hwnd in switchable_windows() {
            let raw = hwnd.0 as isize;
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            let cached = self
                .app_keys
                .get(&raw)
                .filter(|(cached_pid, _)| *cached_pid == pid)
                .map(|(_, key)| key.clone());
            let key = match cached {
                Some(key) => Some(key),
                None => process_path_for_window(hwnd).map(|path| {
                    let key = window_key(hwnd, &path);
                    self.app_keys.insert(raw, (pid, key.clone()));
                    key
                }),
            };
            if let Some(key) = key {
                seen.insert(raw);
                apps.insert(key);
            }
        }
        self.app_keys.retain(|hwnd, _| seen.contains(hwnd));
        APP_TARGETS_HINT.store(apps.len(), Ordering::SeqCst);
    }
}
