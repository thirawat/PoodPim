//! Tray / menu-bar app: hold the talk key to record, release to transcribe and paste.
//! A quick tap starts hands-free recording; tap again to finish. Esc cancels.
//!
//! Two backends: Local (model on this computer) or Serverless (AWS Lambda Function URL).
//! In serverless mode the app wakes the Lambda when the key goes down, so a cold start
//! overlaps with speaking, and waits for that wake-up before sending the audio.
//!
//! Long dictation: the recording is cut at pauses while it is still going (see `segment`),
//! and finished segments are transcribed in the background (see `pipeline`), so letting go
//! of the key only waits for the last segment, and no request gets too large.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::asr::OpenThaiAsr;
use crate::audio::{Recorder, has_sound, resample_fast};
use crate::config::{Backend, Config};
use crate::features::SAMPLE_RATE;
use crate::pipeline::{Outcome, Pipeline};
use crate::overlay::{Overlay, Phase};
use crate::platform::{self, HotkeyEvent, TALK_KEY_LABEL};
use crate::segment::Segmenter;
use crate::corrections::{self, Corrections};
use crate::hotwords::{self, Hotwords};
use crate::filejob::{self, Progress};
use crate::{inserter, model, update};

const TAP: Duration = Duration::from_millis(300);
/// How long after a hold ended unseen (`App::after_hidden`) a press can't turn hands-free.
const AFTER_HIDDEN: Duration = Duration::from_secs(30);
/// Safety cap on one recording; it is finished automatically after this.
const MAX_RECORDING: Duration = Duration::from_secs(10 * 60);
/// How often the recording is checked for a finished segment (sooner = typed sooner while
/// speaking hands-free; the event loop already wakes each overlay frame while recording).
const POLL: Duration = Duration::from_millis(100);
/// How many transcripts the Recent menu keeps (in memory only, gone when the app quits).
const RECENT: usize = 10;
/// Recording goes on this long after the talk key is let go: people let go while saying the
/// last syllable (fast speakers especially), and the sound card still holds a bit of audio.
const TAIL: Duration = Duration::from_millis(200);
/// With "Free memory when idle": the model is freed after this long without dictation.
const UNLOAD_AFTER: Duration = Duration::from_secs(15 * 60);
/// Overlay animation frame interval.
const FRAME: Duration = Duration::from_millis(33);
/// The microphone has sent nothing for this long since the recording started (Bluetooth
/// headsets can take a second or two to switch over) or since its last sound: it is gone.
const MIC_FIRST: Duration = Duration::from_secs(5);
const MIC_GAP: Duration = Duration::from_secs(2);
/// How often, at most, the unfinished part is transcribed again for the pill (`live_preview`);
/// also no sooner than 3x the time the last one took, so a slower PC previews less often. Each
/// preview takes all the model's threads: every 0.7 s it cost ~0.8 of a core while speaking,
/// now ~0.3 (with `has_sound`: none in silence).
const PREVIEW_EVERY: Duration = Duration::from_millis(1200);
/// The preview transcribes at most this much of the end of the unfinished part: the pill shows
/// ~42 characters (3-5 s of speech), and a whole 16 s part took 660 ms each time (M3), using
/// the CPU all the time and making the last part wait after the key was let go.
const PREVIEW_WINDOW: Duration = Duration::from_secs(6);
/// Skip the wake-up call if the server answered this recently (Lambda keeps instances ~5–15 min).
const ASSUME_WARM: Duration = Duration::from_secs(180);
/// First update check after launch, then one a day.
const UPDATE_FIRST_CHECK: Duration = Duration::from_secs(20);
const UPDATE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

enum UserEvent {
    /// A key event and when it happened (stamped on the keyboard thread: the event loop may be
    /// busy, e.g. starting the microphone, and must not mistake a hold for a tap).
    Hotkey(HotkeyEvent, Instant),
    Menu(MenuEvent),
    ModelLoaded(u64, Result<Arc<Mutex<OpenThaiAsr>>, String>),
    /// A message from the Settings window's page (JSON, see assets/settings.html).
    Settings(String),
    /// The model freed when idle was loaded again for a recording (see `asleep`).
    ModelWoke(u64, Result<Arc<Mutex<OpenThaiAsr>>, String>),
    Transcribed(u64, Outcome),
    /// Text of recording n couldn't be pasted (no Accessibility permission; Windows: a key
    /// still held or another window in front) and was left on the clipboard, or it failed:
    /// what the pill says.
    Delivered(u64, Phase),
    /// One segment's text, as soon as it is transcribed (the recording may still go on).
    Part(u64, String),
    /// What the unfinished part (the n-th of the recording, from 0) sounds like so far;
    /// true if only its end was transcribed (PREVIEW_WINDOW).
    Preview(u64, usize, String, bool),
    /// The int8 decoder was added to a model downloaded before it existed.
    DecoderInt8(Result<(), String>),
    Warmed(Result<u64, String>),
    ServerSettings(Option<(String, String)>),
    ApiKey(Option<String>),
    DownloadAnswer(bool),
    Download(u64, u64),
    DownloadDone(Result<(), String>),
    /// The daily check found this newer release tag.
    UpdateFound(String),
    /// Result of "Check for updates…".
    UpdateChecked(Result<Option<String>, String>),
    UpdateAnswer(String, bool),
    /// The new version is in place (Windows) or its installer has started (macOS).
    UpdateInstalled(Result<(), String>),
    /// None: cancelled; Some(delete the model too).
    UninstallAnswer(Option<bool>),
    /// From "Add correction…": (heard, wanted), or None if cancelled.
    CorrectionAnswer(Option<(String, String)>),
    /// Answer to "replace the old model with the smaller one?".
    ModelUpgradeAnswer(bool),
    ModelUpgradeProgress(u64, u64),
    ModelUpgradeDone(Result<(), String>),
    /// The Serverless-backend notice before "Transcribe a file…" was answered: true = go on.
    FileGoAhead(bool),
    /// From "Transcribe a file…": the file picked, or None if cancelled.
    FileChosen(Option<std::path::PathBuf>),
    /// How far file job n is (sent when reading starts and when a part is done).
    FileProgress(u64, Progress),
    /// File job n has ended: the file, and the report or why it failed.
    FileDone(u64, std::path::PathBuf, Result<filejob::Report, String>),
    /// The "done" dialog was closed: the text file to open, if asked for.
    FileOpenAnswer(Option<std::path::PathBuf>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Loading,
    NeedsModel,
    NeedsServer,
    Downloading,
    Idle,
    Recording,
    Transcribing,
    /// Transcribing a file from "Transcribe a file…" (the talk key does nothing meanwhile).
    TranscribingFile,
    Error,
}

struct Ui {
    tray: TrayIcon,
    menu: Menu,
    status: MenuItem,
    permission: MenuItem,
    paste_last: MenuItem,
    recent: Submenu,
    add_correction: MenuItem,
    /// "Transcribe a file…", or Cancel while a file is being transcribed.
    transcribe_file: MenuItem,
    settings: MenuItem,
    download: MenuItem,
    quit: MenuItem,
    /// Whether `permission` / `download` are in the menu now (only while they are needed).
    shown_permission: std::cell::Cell<bool>,
    shown_download: std::cell::Cell<bool>,
    /// What the icon and menu items show now: `refresh` runs on every event and every 100 ms
    /// while recording, and only changes what differs (a new icon is drawn and, on macOS,
    /// encoded to PNG; on Windows each change is a call into Explorer).
    shown: std::cell::RefCell<Shown>,
}

#[derive(Default)]
struct Shown {
    icon: Option<State>,
    status: String,
    paste_last: Option<bool>,
    transcribe_file: Option<(String, bool)>,
}

struct App {
    cfg: Config,
    state: State,
    asr: Option<Arc<Mutex<OpenThaiAsr>>>,
    /// The model was freed after UNLOAD_AFTER without dictation: the state stays Idle and the
    /// next recording loads it again while the user speaks.
    asleep: bool,
    /// Loading the model again after it was freed (see `asleep`).
    waking: Option<Arc<Waking>>,
    /// The last dictation (or model load): what UNLOAD_AFTER counts from.
    last_used: Instant,
    load_gen: u64,
    recorder: Recorder,
    pressed_at: Instant,
    /// The talk key is (or was last) pressed to start this recording, not to carry it on or
    /// finish it: only such a press may drop it (HotkeyEvent::TalkAborted).
    press_started: bool,
    /// The talk key was let go: finish the recording at this time (see TAIL).
    stop_at: Option<Instant>,
    handsfree: bool,
    last_text: String,
    /// The last RECENT transcripts, newest first (menu › Recent). Never written anywhere.
    recent: std::collections::VecDeque<String>,
    /// The Recent menu must be rebuilt (a transcript was added).
    recent_dirty: bool,
    /// Recent menu entries: (Paste id, Fix a word id), in the order of `recent`.
    recent_ids: Vec<(MenuId, MenuId)>,
    recent_entries: Vec<Submenu>,
    /// The Settings window, while open.
    settings: Option<crate::settings::SettingsWindow>,
    /// Open / close the Settings window on the next turn of the event loop (it needs the
    /// event loop's window target).
    want_settings: bool,
    settings_close: bool,
    /// macOS: the app menu with Edit (⌘C / ⌘V in the Settings fields) while Settings is open.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    edit_menu: Option<Menu>,
    /// Entries in the user's vocabulary and corrections files (shown in Settings).
    vocab_count: usize,
    corrections_count: usize,
    status_text: String,
    warm: Option<JoinHandle<()>>,
    segmenter: Option<Segmenter>,
    pipeline: Option<Pipeline>,
    /// Bumped per recording so results of a cancelled one are ignored.
    rec_gen: u64,
    /// Typing while the user speaks (hands-free, `live_typing`): parts of this recording
    /// transcribed so far (after corrections), the parts typed, and the typer with the window
    /// it types into. A part is typed only if every part before it was.
    heard: Vec<String>,
    typed: Vec<String>,
    /// `live_preview`: the unfinished part as heard so far (which part, from 0; text; only its
    /// end), when the last preview was started, and whether one is running.
    preview: (usize, String, bool),
    preview_at: Instant,
    preview_busy: Arc<std::sync::atomic::AtomicBool>,
    /// How long the last preview took, in ms.
    preview_cost: Arc<std::sync::atomic::AtomicU64>,
    /// The part and the recording length at the last preview: the next one needs new sound.
    preview_from: (usize, usize),
    /// The microphone watchdog: samples recorded at the last check, when that number last
    /// grew, and whether the recording was ended because the microphone stopped.
    mic_len: usize,
    mic_seen: Instant,
    mic_lost: bool,
    /// The last recording was all digital zeros: the microphone is muted (or the wrong one).
    mic_silent: bool,
    /// Microphones for the Settings page: (id, name), read when it opens.
    mics: Vec<(String, String)>,
    /// The system's default microphone's name, looked up with `mics` (not on every loop pass).
    default_mic: String,
    /// Windows: the last hold was ended by the app, its release unseen (`check_talk_key`). If
    /// the user was in fact still holding the key, its auto-repeat comes as the next press:
    /// letting go of that one soon is the end of a hold, not a tap for hands-free.
    after_hidden: Option<Instant>,
    /// This recording's press was that one (`after_hidden`): it can't turn hands-free.
    hold_only: bool,
    /// Checks in a row (POLL apart) that found the talk key up while the app thought it held
    /// (`check_talk_key`).
    key_up_seen: u32,
    /// The check found the talk key down during this hold: until then it doesn't act, as a key
    /// that macOS doesn't see as held (one sent by a remapping app or remote control) never is.
    key_down_seen: bool,
    /// Secure Input was on at some point of this recording (a password field): its words are
    /// not shown in the pill, and its text is not kept in Recent / "Paste last".
    secure_seen: bool,
    typer: Option<(inserter::LiveTyper, u64)>,
    last_server_ok: Option<Instant>,
    dialog_open: bool,
    /// The OS hasn't let us watch the keyboard yet (macOS Accessibility).
    needs_permission: bool,
    /// The server answered 401: the key in the config is wrong.
    key_rejected: bool,
    overlay: Overlay,
    last_poll: Instant,
    proxy: EventLoopProxy<UserEvent>,
    /// The "ready" message has been shown since launch.
    announced: bool,
    /// A newer release to offer once the app is idle and no dialog is open.
    pending_update: Option<String>,
    /// Already offered by the daily check this session (not asked again until the next launch).
    offered_update: Option<String>,
    /// An update is being downloaded and put in place.
    updating: bool,
    /// The update is in place: restart into it once the user isn't recording.
    restart_pending: bool,
    /// Quit at the end of this event (an update or uninstall has taken over).
    quit: bool,
    /// Held while running so a second copy of the app can tell.
    _instance_lock: std::fs::File,
    /// The user's word list (hotwords::user_file), reloaded when the file changes.
    vocabulary: Arc<Hotwords>,
    vocabulary_header: Arc<String>,
    vocabulary_modified: Option<std::time::SystemTime>,
    /// Replacements applied before pasting (corrections::user_file), reloaded when it changes.
    corrections: Corrections,
    corrections_modified: Option<std::time::SystemTime>,
    /// The smaller model is being downloaded / is ready to replace the loaded one.
    model_upgrading: bool,
    model_swap_pending: bool,
    remove_fp32_after_load: bool,
    /// Fetching the int8 decoder was started this session (tried again at the next launch).
    decoder_int8_started: bool,
    /// Bumped per file job, so the events of an older one are ignored.
    file_gen: u64,
    /// The file job's Cancel switch, while one runs.
    file_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// The name of the file being transcribed (for the menu).
    file_name: String,
    /// "Check for updates" is waiting for GitHub (a second click does nothing meanwhile).
    update_checking: bool,
}

enum Instance {
    /// This copy holds the lock: keep the file open while running.
    Only(std::fs::File),
    /// Another copy of the app holds it.
    Running,
    /// The lock file can't be opened (the settings folder can't be written): why, for the user.
    Broken(String),
}

/// Locks a file in the settings folder, so a second copy of the app can tell.
fn single_instance() -> Instance {
    let path = model::app_dir(dirs::config_dir()).join("app.lock");
    let file = match std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path) {
        Ok(f) => f,
        Err(e) => return Instance::Broken(format!("PoodPim เปิดไฟล์ {} ไม่ได้ ({e})", path.display())),
    };
    match file.try_lock() {
        Ok(()) => Instance::Only(file),
        Err(std::fs::TryLockError::WouldBlock) => Instance::Running,
        Err(std::fs::TryLockError::Error(e)) => {
            eprintln!("instance lock: {e}"); // can't tell; start anyway
            Instance::Only(file)
        }
    }
}

/// Started from Finder / the Start menu, standard error goes nowhere: keep it in a file
/// (replaced on each launch) so problems on someone else's computer can be looked into.
fn log_to_file() {
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() {
        return;
    }
    let Some(dir) = platform::log_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(file) = std::fs::File::create(dir.join("poodpim.log")) {
        platform::redirect_stderr(file);
    }
}

/// What was said, for the terminal only: the log file must not collect people's dictation.
pub(crate) fn said(text: &str) -> &str {
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() { text } else { "(text not logged)" }
}

pub fn run() -> Result<()> {
    // The settings and model folders, which may be links to another drive (see ensure_dir).
    let mut remade = Vec::new();
    for dir in [model::app_dir(dirs::config_dir()), model::app_dir(dirs::data_dir())] {
        match model::ensure_dir(&dir) {
            Ok(Some(target)) => remade.push(format!("{} pointed to {}, which was gone: made it again", dir.display(), target.display())),
            Ok(None) => {}
            Err(why) => {
                eprintln!("{why}");
                platform::alert(&why);
                return Ok(());
            }
        }
    }
    // Just updated: the old copy is still quitting, so wait for its lock rather than report it.
    let mut lock = single_instance();
    if std::env::var_os(update::UPDATED_ENV).is_some() {
        for _ in 0..75 {
            if !matches!(lock, Instance::Running) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
            lock = single_instance();
        }
    }
    let instance_lock = match lock {
        Instance::Only(file) => file,
        // Opened again while running (often because the menu-bar icon is hidden): say so.
        Instance::Running => {
            platform::alert(platform::ALREADY_RUNNING);
            return Ok(());
        }
        Instance::Broken(why) => {
            eprintln!("{why}");
            platform::alert(&why);
            return Ok(());
        }
    };
    // Only now: a second copy started by mistake must not empty the running one's log.
    log_to_file();
    eprintln!("PoodPim {} ({} {})", update::CURRENT, std::env::consts::OS, std::env::consts::ARCH);
    for note in remade {
        eprintln!("{note}");
    }
    update::clean_up_after_update();
    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory); // no Dock icon
    }
    let proxy = event_loop.create_proxy();
    {
        let p = proxy.clone();
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UserEvent::Menu(e));
        }));
    }
    let mut app = App {
        cfg: Config::load(),
        state: State::Loading,
        asr: None,
        asleep: false,
        waking: None,
        last_used: Instant::now(),
        load_gen: 0,
        recorder: Recorder::new(),
        pressed_at: Instant::now(),
        press_started: false,
        stop_at: None,
        handsfree: false,
        last_text: String::new(),
        recent: Default::default(),
        recent_dirty: false,
        recent_ids: Vec::new(),
        recent_entries: Vec::new(),
        settings: None,
        want_settings: false,
        settings_close: false,
        edit_menu: None,
        vocab_count: 0,
        corrections_count: 0,
        status_text: String::new(),
        warm: None,
        segmenter: None,
        pipeline: None,
        rec_gen: 0,
        heard: Vec::new(),
        typed: Vec::new(),
        preview: (0, String::new(), false),
        preview_at: Instant::now(),
        preview_busy: Default::default(),
        preview_cost: Default::default(),
        preview_from: (0, 0),
        mic_len: 0,
        mic_seen: Instant::now(),
        mic_lost: false,
        mic_silent: false,
        mics: Vec::new(),
        default_mic: String::new(),
        after_hidden: None,
        hold_only: false,
        key_up_seen: 0,
        key_down_seen: false,
        secure_seen: false,
        typer: None,
        last_server_ok: None,
        dialog_open: false,
        needs_permission: false,
        key_rejected: false,
        overlay: Overlay::new(),
        last_poll: Instant::now(),
        proxy: proxy.clone(),
        announced: false,
        pending_update: None,
        offered_update: None,
        updating: false,
        restart_pending: false,
        quit: false,
        _instance_lock: instance_lock,
        vocabulary: Arc::new(Hotwords::default()),
        vocabulary_header: Arc::new(String::new()),
        vocabulary_modified: None,
        corrections: Corrections::default(),
        corrections_modified: None,
        model_upgrading: false,
        model_swap_pending: false,
        remove_fp32_after_load: false,
        decoder_int8_started: false,
        file_gen: 0,
        file_cancel: None,
        file_name: String::new(),
        update_checking: false,
    };
    app.reload_vocabulary();
    let mut ui: Option<Ui> = None;

    event_loop.run(move |event, target, control_flow| {
        if app.state == State::Recording && app.stop_at.is_some_and(|t| Instant::now() >= t) {
            app.stop_at = None;
            app.finish();
        }
        if app.state == State::Recording {
            app.update_level();
            if app.last_poll.elapsed() >= POLL {
                app.last_poll = Instant::now();
                app.poll_segments();
                if let Some(ui) = &ui {
                    app.refresh(ui);
                }
            }
        }
        app.unload_if_idle();
        let animating = app.overlay.tick();
        match event {
            Event::NewEvents(StartCause::Init) => {
                ui = Some(build_ui());
                let p = proxy.clone();
                platform::spawn_hotkey_listener(Box::new(move |e| {
                    let _ = p.send_event(UserEvent::Hotkey(e, Instant::now()));
                }));
                if !platform::accessibility_trusted(true) {
                    eprintln!("Accessibility permission is needed: System Settings › Privacy & Security › Accessibility");
                }
                app.apply_autostart();
                // For testing the Settings window without the tray menu.
                app.want_settings = std::env::var_os("POODPIM_OPEN_SETTINGS").is_some();
                Recorder::warm_up(app.cfg.microphone.clone());
                app.start_backend();
                // First launch in Local mode: ask about the model now, not on the first key press.
                if app.state == State::NeedsModel {
                    app.offer_download();
                } else if app.state == State::Idle || app.state == State::Loading {
                    if app.state == State::Idle {
                        app.announce_ready();
                    }
                    app.offer_model_upgrade();
                }
                if update::supported() {
                    let p = proxy.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(UPDATE_FIRST_CHECK);
                        loop {
                            match update::check() {
                                Ok(Some(tag)) => {
                                    let _ = p.send_event(UserEvent::UpdateFound(tag));
                                }
                                Ok(None) => {}
                                Err(e) => eprintln!("update check: {e:#}"),
                            }
                            std::thread::sleep(UPDATE_EVERY);
                        }
                    });
                }
            }
            Event::UserEvent(ev) => {
                if let UserEvent::Menu(MenuEvent { id }) = &ev {
                    let ui = ui.as_ref().unwrap();
                    if id == ui.quit.id() {
                        *control_flow = ControlFlow::Exit;
                        return;
                    } else if id == ui.settings.id() {
                        app.want_settings = true;
                    } else if let Some(k) = app.recent_ids.iter().position(|(p, f)| p == id || f == id) {
                        let text = app.recent[k].clone();
                        if app.recent_ids[k].0 == *id {
                            if let phase @ Phase::Copied { .. } = app.paste(text) {
                                app.overlay.set(phase);
                            }
                        } else {
                            app.ask_correction(&text);
                        }
                    } else if id == ui.add_correction.id() {
                        let last = app.last_text.clone();
                        app.ask_correction(&last);
                    } else if id == ui.transcribe_file.id() {
                        app.transcribe_file_clicked();
                    } else if id == ui.paste_last.id() {
                        if let phase @ Phase::Copied { .. } = app.paste(app.last_text.clone()) {
                            app.overlay.set(phase);
                        }
                    } else if id == ui.download.id() {
                        app.start_download();
                    } else if id == ui.permission.id() {
                        platform::open_permission_settings();
                    }
                } else {
                    app.handle(ev);
                }
                if let Some(ui) = &ui {
                    app.refresh(ui);
                }
            }
            Event::WindowEvent { window_id, event: WindowEvent::CloseRequested, .. }
                if app.settings.as_ref().is_some_and(|w| w.id() == window_id) =>
            {
                app.settings_close = true;
            }
            _ => {}
        }
        if app.want_settings {
            app.want_settings = false;
            app.open_settings(target);
        }
        if app.settings_close {
            app.settings_close = false;
            app.close_settings(target);
        }
        app.show_settings_state();
        if app.quit {
            *control_flow = ControlFlow::Exit;
            return;
        }
        // While recording, wake up regularly to cut finished segments off the recording.
        *control_flow = if animating {
            ControlFlow::WaitUntil(Instant::now() + FRAME)
        } else if app.state == State::Recording {
            ControlFlow::WaitUntil(app.stop_at.unwrap_or(Instant::now() + POLL).min(Instant::now() + POLL))
        } else if let Some(t) = app.unload_at() {
            ControlFlow::WaitUntil(t)
        } else {
            ControlFlow::Wait
        };
    })
}

fn build_ui() -> Ui {
    let status = MenuItem::new("Loading…", false, None);
    let permission = MenuItem::new("⚠ Open Accessibility settings…", true, None);
    let paste_last = MenuItem::new("Paste last transcript", false, None);
    let recent = Submenu::new("Recent", false);
    let add_correction = MenuItem::new("Add correction…", true, None);
    let transcribe_file = MenuItem::new("Transcribe a file…", false, None);
    let settings = MenuItem::new("Settings…", true, None);
    let download = MenuItem::new("Download model (1 GB)", true, None);
    let hint = MenuItem::new(format!("Hold {TALK_KEY_LABEL} to talk · tap = hands-free · Esc = cancel"), false, None);
    let quit = MenuItem::new("Quit PoodPim", true, None);
    let menu = Menu::new();
    let seps: Vec<PredefinedMenuItem> = (0..4).map(|_| PredefinedMenuItem::separator()).collect();
    // The permission and download items go in only while needed (see refresh).
    menu.append_items(&[
        &status,
        &seps[0],
        &paste_last,
        &recent,
        &add_correction,
        &seps[1],
        &transcribe_file,
        &seps[2],
        &settings,
        &seps[3],
        &hint,
        &quit,
    ])
    .expect("menu");
    let (icon, template) = icon_for(State::Loading);
    let builder = TrayIconBuilder::new().with_menu(Box::new(menu.clone())).with_tooltip("PoodPim — Thai dictation");
    #[cfg(target_os = "macos")]
    let builder = if template { builder.with_icon_templated(icon) } else { builder.with_icon(icon) };
    #[cfg(not(target_os = "macos"))]
    let builder = {
        let _ = template;
        builder.with_icon(icon)
    };
    let tray = builder.build().expect("tray icon");
    Ui {
        tray,
        menu,
        status,
        permission,
        paste_last,
        recent,
        add_correction,
        transcribe_file,
        settings,
        download,
        quit,
        shown_permission: Default::default(),
        shown_download: Default::default(),
        shown: Default::default(),
    }
}

impl App {
    fn apply_autostart(&self) {
        if let Err(e) = platform::set_autostart(self.cfg.start_at_login) {
            eprintln!("start at login: {e:#}");
        }
    }

    fn busy(&self) -> bool {
        matches!(self.state, State::Recording | State::Transcribing | State::Downloading | State::TranscribingFile)
    }

    fn ready_text(&self) -> String {
        match self.cfg.backend {
            Backend::Local => "Ready · Local (OpenThai 2.0 ASR)".into(),
            Backend::Lambda => "Ready · Serverless (AWS Lambda)".into(),
        }
    }

    fn start_backend(&mut self) {
        self.load_gen += 1;
        match self.cfg.backend {
            Backend::Local => self.start_loading(),
            Backend::Lambda => {
                self.asr = None; // free the ~3 GB local model
                self.asleep = false;
                self.waking = None;
                if self.cfg.server().url.is_empty() {
                    self.state = State::NeedsServer;
                    self.status_text = format!("Enter the API URL && key: press {TALK_KEY_LABEL}");
                    return;
                }
                if self.cfg.server().api_key.is_empty() {
                    self.state = State::NeedsServer;
                    self.status_text = format!("Enter the API key: press {TALK_KEY_LABEL}");
                    return;
                }
                self.state = State::Idle;
                self.status_text = format!("{} · waking server…", self.ready_text());
                self.spawn_warm(true);
            }
        }
    }

    fn switch_backend(&mut self, backend: Backend) {
        if self.busy() {
            return; // refresh() puts the check marks back
        }
        if backend == Backend::Lambda {
            // Pick up URL / key edited in the config file (the Windows settings flow).
            let disk = Config::load();
            if !disk.server_url.is_empty() {
                self.cfg.server_url = disk.server_url;
                self.cfg.api_key = disk.api_key;
            }
        }
        self.cfg.backend = backend;
        self.cfg.save();
        self.last_server_ok = None;
        self.start_backend();
        if self.state == State::NeedsModel {
            self.offer_download();
        }
    }

    /// Asks for the API key only (the server URL is already set).
    fn ask_api_key(&mut self) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let message = if self.key_rejected {
            "API key ไม่ถูกต้อง กรุณาใส่ใหม่"
        } else {
            "ใส่ API key เพื่อใช้เซิร์ฟเวอร์ถอดเสียงของ PoodPim\n(ขอได้จากคนที่ส่งแอปนี้ให้คุณ)"
        };
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let _ = p.send_event(UserEvent::ApiKey(platform::ask_api_key(message)));
        });
    }

    /// Local backend without a model: offer to download it.
    fn offer_download(&mut self) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let yes = platform::confirm(
                "ยังไม่มีโมเดลถอดเสียงในเครื่องนี้\n\nโหมด Local ต้องดาวน์โหลดโมเดลก่อน ขนาด 1 GB (ครั้งเดียว) \
                 ระหว่างดาวน์โหลดดูความคืบหน้าได้ในเมนู 🎙\n\nหรือเลือก \"เซิร์ฟเวอร์ของทีม\" ในเมนู 🎙 › Settings… เพื่อถอดเสียงบนเซิร์ฟเวอร์แทน\n\nดาวน์โหลดเลยไหม?",
                "ดาวน์โหลด",
                "ไว้ทีหลัง",
            );
            let _ = p.send_event(UserEvent::DownloadAnswer(yes));
        });
    }

    fn open_server_settings(&mut self) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let (url, key) = (self.cfg.server_url.clone(), self.cfg.api_key.clone());
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let r = platform::ask_server_settings(&url, &key);
            let _ = p.send_event(UserEvent::ServerSettings(r));
        });
    }

    /// Wakes the Lambda in the background. With `report`, the result updates the status line.
    fn spawn_warm(&mut self, report: bool) {
        let server = self.cfg.server();
        let p = self.proxy.clone();
        self.warm = Some(std::thread::spawn(move || {
            let t0 = Instant::now();
            let r = server.warm().map_err(|e| format!("{e:#}"));
            if let Ok(load_ms) = &r {
                eprintln!("server warm: {:.2}s round trip, model load {load_ms} ms", t0.elapsed().as_secs_f32());
            }
            if report || r.is_err() {
                let _ = p.send_event(UserEvent::Warmed(r));
            }
        }));
    }

    fn start_loading(&mut self) {
        let dir = model::model_dir();
        if !model::is_ready(&dir) {
            self.state = State::NeedsModel;
            self.status_text = "Model not downloaded".into();
            return;
        }
        self.asleep = false;
        self.waking = None;
        if self.asr.is_some() {
            self.state = State::Idle;
            self.status_text = self.ready_text();
            return;
        }
        self.state = State::Loading;
        self.status_text = "Loading OpenThai ASR…".into();
        let (p, generation) = (self.proxy.clone(), self.load_gen);
        std::thread::spawn(move || {
            let t = Instant::now();
            let r = OpenThaiAsr::load(&dir).map(|a| Arc::new(Mutex::new(a))).map_err(|e| format!("{e:#}"));
            eprintln!("model loaded in {:.1} s (with warm-up)", t.elapsed().as_secs_f32());
            let _ = p.send_event(UserEvent::ModelLoaded(generation, r));
        });
    }

    fn start_download(&mut self) {
        if self.state != State::NeedsModel && self.state != State::Error {
            return;
        }
        self.state = State::Downloading;
        self.status_text = "Downloading model…".into();
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let last = std::cell::Cell::new(Instant::now());
            let r = model::download(&model::model_dir(), &|done, total| {
                if last.get().elapsed() > Duration::from_millis(500) {
                    last.set(Instant::now());
                    let _ = p.send_event(UserEvent::Download(done, total));
                }
            });
            let _ = p.send_event(UserEvent::DownloadDone(r.map_err(|e| format!("{e:#}"))));
        });
    }

    fn handle(&mut self, ev: UserEvent) {
        match ev {
            UserEvent::Hotkey(h, at) => self.on_hotkey(h, at),
            UserEvent::ModelLoaded(generation, r) => {
                if generation != self.load_gen || self.cfg.backend != Backend::Local {
                    return; // the user switched backend while it was loading
                }
                match r {
                    Ok(asr) => {
                        self.asr = Some(asr);
                        self.last_used = Instant::now();
                        self.state = State::Idle;
                        self.status_text = self.ready_text();
                        self.announce_ready();
                        // For testing "Transcribe a file…" without the tray menu.
                        if let Some(f) = std::env::var_os("POODPIM_TRANSCRIBE_FILE").filter(|_| self.file_gen == 0) {
                            self.start_file_job(f.into());
                        }
                        // The int8 model loaded fine: the 2.4 GB full-precision one, left from before
                        // 1.0.2 or from a switch that didn't finish, can go (remove_fp32 only acts
                        // when the int8 model is there).
                        let dir = model::model_dir();
                        self.remove_fp32_after_load = false;
                        if dir.join("encoder.onnx.data").exists() && !model::is_fp32_only(&dir) {
                            model::remove_fp32(&dir);
                            eprintln!("using the int8 model; removed the fp32 one");
                        }
                        self.fetch_decoder_int8();
                    }
                    Err(e) => self.fail(&e),
                }
            }
            UserEvent::ModelWoke(generation, r) => {
                if generation != self.load_gen || self.cfg.backend != Backend::Local || !self.asleep {
                    return;
                }
                self.waking = None;
                match r {
                    Ok(asr) => {
                        self.asr = Some(asr);
                        self.asleep = false;
                        self.last_used = Instant::now();
                    }
                    // Stays asleep: the next recording tries again (this one reports the error).
                    Err(e) => eprintln!("loading the model again: {e}"),
                }
            }
            // The server may have been set up during the download (that switches to Lambda):
            // then the download no longer matters, so start whichever backend is chosen now.
            UserEvent::DownloadDone(Err(e)) if self.cfg.backend == Backend::Local => self.fail(&e),
            UserEvent::DownloadDone(Err(e)) => {
                eprintln!("model download: {e}");
                self.start_backend();
            }
            UserEvent::Download(done, total) => {
                self.status_text = format!("Downloading model… {:.1} / {:.1} GB", done as f64 / 1e9, total as f64 / 1e9);
            }
            UserEvent::DownloadDone(Ok(())) => self.start_backend(),
            UserEvent::Warmed(r) => {
                if self.cfg.backend != Backend::Lambda {
                    return;
                }
                match r {
                    Ok(_) => {
                        self.last_server_ok = Some(Instant::now());
                        self.key_rejected = false;
                        if self.state == State::Idle {
                            self.status_text = self.ready_text();
                        }
                    }
                    Err(e) => {
                        eprintln!("server: {e}");
                        self.key_rejected = e.starts_with("server returned 401 ");
                        if !self.busy() {
                            self.status_text = if self.key_rejected {
                                format!("API key was rejected: press {TALK_KEY_LABEL} to enter it")
                            } else {
                                format!("Server error: {}", e.chars().take(70).collect::<String>())
                            };
                        }
                    }
                }
            }
            UserEvent::ApiKey(r) => {
                self.dialog_open = false;
                if let Some(key) = r {
                    self.cfg.api_key = key;
                    self.cfg.save();
                    self.key_rejected = false;
                    self.last_server_ok = None;
                    if !self.busy() && self.cfg.backend == Backend::Lambda {
                        self.start_backend();
                    }
                }
            }
            UserEvent::DownloadAnswer(yes) => {
                self.dialog_open = false;
                if yes {
                    self.start_download();
                }
            }
            UserEvent::ServerSettings(r) => {
                self.dialog_open = false;
                if let Some((url, key)) = r {
                    self.cfg.server_url = url;
                    self.cfg.api_key = key;
                    self.cfg.backend = Backend::Lambda;
                    self.cfg.save();
                    self.last_server_ok = None;
                    if !self.busy() {
                        self.start_backend();
                    }
                }
            }
            UserEvent::Transcribed(generation, out) => {
                if generation != self.rec_gen {
                    return; // cancelled recording
                }
                self.last_used = Instant::now();
                self.state = State::Idle;
                self.status_text = self.ready_text();
                if self.cfg.backend == Backend::Lambda && out.errors.len() < out.segments {
                    self.last_server_ok = Some(Instant::now());
                }
                if let Some(e) = out.errors.first() {
                    eprintln!("{} of {} segments failed: {e}", out.errors.len(), out.segments);
                    self.key_rejected = e.starts_with("server returned 401 ");
                    self.status_text = format!(
                        "{} of {} parts failed: {}",
                        out.errors.len(),
                        out.segments,
                        e.chars().take(60).collect::<String>()
                    );
                }
                // Parts already typed while the user spoke; the rest is pasted now.
                let typed = std::mem::take(&mut self.typed);
                let typer = self.typer.take();
                let rest = crate::pipeline::join_parts(out.parts.get(typed.len()..).unwrap_or_default());
                let rest = if rest.is_empty() { rest } else { corrections::finish(&rest, &self.corrections, self.cfg.numbers_as_digits) };
                let text = crate::pipeline::join_parts(&typed.iter().chain([&rest]).collect::<Vec<_>>());
                if !text.is_empty() {
                    let secure = self.secure_seen || platform::secure_input();
                    let shown = if secure { "(Secure Input: text not logged)" } else { said(&text) };
                    eprintln!("{:.1}s in {} segment(s), {} typed live: {}", out.audio_s, out.segments, typed.len(), shown);
                    if secure {
                        // Likely a password: pasted, but not kept for the menu (nor shown while heard).
                        eprintln!("Secure Input is on: not kept in Recent");
                    } else {
                        self.last_text = text.clone();
                        self.recent.push_front(text.clone());
                        self.recent.truncate(RECENT);
                        self.recent_dirty = true;
                    }
                    // Checked again for the end: copy-only turned on or Accessibility taken away since.
                    let deliver = !self.cfg.copy_only && platform::accessibility_trusted(false);
                    let phase = match typer {
                        Some(_) if rest.is_empty() => Phase::Done,
                        Some((typer, window))
                            if deliver && platform::focused_window() == window && !platform::foreground_is_elevated() =>
                        {
                            typer.type_text(continuation(&typed, &rest));
                            Phase::Done
                        }
                        Some((typer, _)) => {
                            eprintln!("can't paste the rest where the first parts went: copied it");
                            // By the typer, after its queued parts: its own clipboard changes come
                            // first and can't put back the old clipboard over the copy. It says
                            // when the copy is there (Delivered); till then the pill stays Working.
                            typer.finish_copying(continuation(&typed, &rest), deliver && platform::foreground_is_elevated());
                            Phase::Working
                        }
                        None => self.paste(rest),
                    };
                    if phase != Phase::Working {
                        self.overlay.set(if phase == Phase::Done && self.mic_lost { Phase::MicLost } else { phase });
                    }
                } else if !out.errors.is_empty() {
                    self.overlay.set(Phase::Failed);
                } else {
                    eprintln!("no speech in {:.1}s of audio", out.audio_s);
                    self.overlay.set(if self.mic_lost {
                        Phase::MicLost
                    } else if self.mic_silent {
                        Phase::MicSilent
                    } else {
                        Phase::NoSpeech
                    });
                }
                if self.mic_lost {
                    self.status_text = "The microphone stopped sending sound: check it, or pick another one in Settings › Microphone".into();
                }
            }
            UserEvent::UpdateFound(tag) => {
                if self.offered_update.as_ref() != Some(&tag) {
                    self.offered_update = Some(tag.clone());
                    self.pending_update = Some(tag);
                }
            }
            UserEvent::UpdateChecked(r) => {
                self.update_checking = false;
                // Asked from Settings: the answer goes there if it is still open.
                let say = |app: &Self, text: String| if app.settings.is_some() { app.settings_toast(&text) } else { show_alert(text) };
                match r {
                    Ok(Some(tag)) => self.pending_update = Some(tag),
                    Ok(None) => say(self, format!("PoodPim {} เป็นเวอร์ชันล่าสุดแล้ว", update::CURRENT)),
                    Err(e) => say(self, format!("เช็คอัปเดตไม่ได้: {e}")),
                }
            }
            UserEvent::UpdateAnswer(tag, yes) => {
                self.dialog_open = false;
                if !yes {
                    return;
                }
                if self.busy() {
                    self.pending_update = Some(tag); // ask again when the recording is done
                    return;
                }
                if self.updating {
                    return;
                }
                self.updating = true;
                eprintln!("updating to {tag}");
                if !self.busy() {
                    self.status_text = format!("Downloading PoodPim {}…", tag.trim_start_matches('v'));
                }
                let p = self.proxy.clone();
                std::thread::spawn(move || {
                    let r = update::install(&tag).map_err(|e| format!("{e:#}"));
                    let _ = p.send_event(UserEvent::UpdateInstalled(r));
                });
            }
            UserEvent::UpdateInstalled(r) => {
                self.updating = false;
                match r {
                    Ok(()) => self.restart_pending = true,
                    Err(e) => {
                        eprintln!("update failed: {e}");
                        if !self.busy() {
                            self.status_text = self.ready_text();
                        }
                        show_alert(format!("อัปเดตไม่ได้: {e}"));
                    }
                }
            }
            UserEvent::UninstallAnswer(answer) => {
                self.dialog_open = false;
                let Some(delete_model) = answer else { return };
                // Close Settings first: on Windows its WebView2 holds files in the data folder
                // that the uninstall deletes. The window itself goes when the app quits.
                if let Some(w) = self.settings.take() {
                    drop(w);
                    std::thread::sleep(Duration::from_millis(500)); // WebView2 lets go of its files
                }
                match update::uninstall(delete_model) {
                    Ok(()) => {
                        // macOS: uninstall.sh finishes once this app has quit. Windows: done.
                        #[cfg(windows)]
                        platform::alert("ลบ PoodPim ออกจากเครื่องแล้ว");
                        self.quit = true;
                    }
                    Err(e) => {
                        let message = format!("ถอนการติดตั้งไม่สำเร็จทั้งหมด: {e:#}");
                        // Windows quits right after (what it removed included the app): wait for
                        // the box there. macOS keeps running: not on the event loop.
                        if cfg!(windows) { platform::alert(&message) } else { show_alert(message) }
                        // Windows has removed what it could, the app included: don't keep running.
                        self.quit = cfg!(windows);
                    }
                }
            }
            UserEvent::CorrectionAnswer(answer) => {
                self.dialog_open = false;
                let Some((heard, wanted)) = answer else { return };
                match corrections::add_to_user_file(&heard, &wanted) {
                    Ok(()) => {
                        self.reload_vocabulary();
                        eprintln!("added a correction");
                    }
                    Err(e) => show_alert(format!("บันทึกไม่ได้: {e}")),
                }
            }
            UserEvent::ModelUpgradeAnswer(yes) => {
                self.dialog_open = false;
                if !yes {
                    self.cfg.model_upgrade_declined = true;
                    self.cfg.save();
                    return;
                }
                self.model_upgrading = true;
                let p = self.proxy.clone();
                std::thread::spawn(move || {
                    let last = std::cell::Cell::new(Instant::now());
                    let r = model::download(&model::model_dir(), &|done, total| {
                        if last.get().elapsed() > Duration::from_millis(500) {
                            last.set(Instant::now());
                            let _ = p.send_event(UserEvent::ModelUpgradeProgress(done, total));
                        }
                    });
                    let _ = p.send_event(UserEvent::ModelUpgradeDone(r.map_err(|e| format!("{e:#}"))));
                });
            }
            UserEvent::ModelUpgradeProgress(done, total) => {
                if !self.busy() {
                    self.status_text =
                        format!("Downloading the smaller model… {:.1} / {:.1} GB", done as f64 / 1e9, total as f64 / 1e9);
                }
            }
            UserEvent::ModelUpgradeDone(r) => {
                self.model_upgrading = false;
                match r {
                    Ok(()) => self.model_swap_pending = true, // when the user isn't recording
                    Err(e) => {
                        eprintln!("model upgrade: {e}");
                        if !self.busy() {
                            self.status_text = self.ready_text();
                        }
                    }
                }
            }
            UserEvent::Part(generation, text) => self.type_part(generation, text),
            UserEvent::Preview(generation, part, text, clipped) => {
                if generation == self.rec_gen && self.state == State::Recording {
                    self.preview = (part, corrections::finish(&text, &self.corrections, self.cfg.numbers_as_digits), clipped);
                    self.show_heard();
                }
            }
            UserEvent::Delivered(generation, phase) => {
                // Not over a newer recording's pill (a newer one has another generation; Recent
                // pasted while recording or transcribing has this one).
                if generation == self.rec_gen && !matches!(self.state, State::Recording | State::Transcribing) {
                    self.overlay.set(phase);
                }
            }
            UserEvent::DecoderInt8(r) => match r {
                Ok(()) => {
                    eprintln!("downloaded the int8 decoder: switching to it");
                    self.model_swap_pending = true; // when the user isn't recording
                }
                Err(e) => eprintln!("int8 decoder: {e} (keeping the fp32 one)"),
            },
            UserEvent::FileGoAhead(go) => {
                // A recording may have started while the notice was up: then no picker.
                if go && self.state == State::Idle {
                    self.open_file_picker(); // dialog_open stays set until FileChosen
                } else {
                    self.dialog_open = false;
                }
            }
            UserEvent::FileChosen(path) => {
                self.dialog_open = false;
                if let Some(path) = path {
                    self.start_file_job(path);
                }
            }
            UserEvent::FileProgress(generation, progress) => {
                if generation == self.file_gen && self.state == State::TranscribingFile {
                    let cancelling = self.file_cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::SeqCst));
                    self.status_text = match progress {
                        _ if cancelling => format!("Cancelling {}…", self.file_name),
                        Progress::Reading => format!("Reading {}…", self.file_name),
                        Progress::Transcribing { done_s, total_s: Some(total_s) } => {
                            format!("Transcribing {} · {} / {}", self.file_name, clock(done_s), clock(total_s))
                        }
                        Progress::Transcribing { done_s, total_s: None } => {
                            format!("Transcribing {} · {}", self.file_name, clock(done_s))
                        }
                    };
                }
            }
            UserEvent::FileDone(generation, src, r) => {
                if generation != self.file_gen {
                    return;
                }
                self.file_cancel = None;
                self.last_used = Instant::now(); // not freed the moment the job ends
                self.state = State::Idle;
                self.status_text = self.ready_text();
                self.file_done(&src, r);
            }
            UserEvent::FileOpenAnswer(path) => {
                self.dialog_open = false;
                if let Some(path) = path {
                    platform::open_in_editor(&path);
                }
            }
            UserEvent::Menu(_) => {}
            UserEvent::Settings(msg) => self.settings_message(&msg),
        }
    }

    /// Models downloaded before the int8 decoder existed get it in the background (27 MB);
    /// the model is loaded again with it once the user isn't recording.
    fn fetch_decoder_int8(&mut self) {
        let dir = model::model_dir();
        if self.decoder_int8_started || dir.join(model::DECODER_INT8).is_file() || model::is_fp32_only(&dir) {
            return;
        }
        self.decoder_int8_started = true;
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let r = model::download_decoder_int8(&dir).map_err(|e| format!("{e:#}"));
            let _ = p.send_event(UserEvent::DecoderInt8(r));
        });
    }

    /// Versions before 1.0.2 downloaded the full-precision model (2.6 GB of RAM). Offers once
    /// to replace it with the int8 one (1 GB of RAM, as accurate on tools/asr-eval).
    fn offer_model_upgrade(&mut self) {
        if self.cfg.backend != Backend::Local
            || self.cfg.model_upgrade_declined
            || self.model_upgrading
            || self.dialog_open
            || !model::is_fp32_only(&model::model_dir())
        {
            return;
        }
        self.dialog_open = true;
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let yes = platform::confirm(
                "PoodPim มีโมเดลถอดเสียงรุ่นใหม่ที่ใช้หน่วยความจำน้อยลงราว 60% (1 GB แทน 2.6 GB) \
                 ถอดเร็วขึ้น และแม่นเท่าเดิม\n\nดาวน์โหลด 1 GB (ระหว่างนี้ใช้งานได้ตามปกติ) \
                 แล้วลบรุ่นเดิมเพื่อคืนพื้นที่ 2.4 GB?",
                "ดาวน์โหลด",
                "ไว้ทีหลัง",
            );
            let _ = p.send_event(UserEvent::ModelUpgradeAnswer(yes));
        });
    }

    /// Two prompts: the word as it came out, then as it should be; appended to the corrections.
    /// `transcript` (e.g. the last one) fills the first prompt, to cut down to the wrong word:
    /// its spelling has to match exactly, and copying it is easier than typing it.
    fn ask_correction(&mut self, transcript: &str) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let p = self.proxy.clone();
        let transcript = transcript.to_string();
        std::thread::spawn(move || {
            let prompt = if transcript.is_empty() {
                "แก้คำ (1/2): คำที่ PoodPim พิมพ์ออกมาผิด\nเช่น ฟีดแบ็ก หรือ depop\n(ได้ยินผิดหลายแบบ คั่นด้วยจุลภาค ,)"
            } else {
                "แก้คำ (1/2): ลบข้อความให้เหลือแค่คำที่ PoodPim พิมพ์ผิด\nเช่น ฟีดแบ็ก หรือ depop\n(ได้ยินผิดหลายแบบ คั่นด้วยจุลภาค ,)"
            };
            let answer = platform::ask_text_then(prompt, &transcript, "แก้คำ (2/2): ให้เขียน \"", "\" เป็นอะไร\nเช่น feedback หรือ DevOps");
            let _ = p.send_event(UserEvent::CorrectionAnswer(answer));
        });
    }

    /// Reads the word list and the corrections again if their files changed (they are edited
    /// in a text editor). Creates them on first use.
    fn reload_vocabulary(&mut self) {
        let modified = std::fs::metadata(hotwords::user_file()).and_then(|m| m.modified()).ok();
        if modified.is_none() || modified != self.vocabulary_modified {
            let (text, modified) = hotwords::read_user_list();
            self.vocab_count = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).count();
            self.vocabulary = Arc::new(Hotwords::new(&text));
            self.vocabulary_header = Arc::new(hotwords::to_header(&text));
            self.vocabulary_modified = modified;
        }
        let modified = std::fs::metadata(corrections::user_file()).and_then(|m| m.modified()).ok();
        if modified.is_none() || modified != self.corrections_modified {
            let (text, modified) = corrections::read_user_list();
            self.corrections_count = text.lines().map(str::trim).filter(|l| !l.starts_with('#') && l.contains('=')).count();
            self.corrections = Corrections::new(&text);
            self.corrections_modified = modified;
        }
    }

    /// Shows "ready" once per launch: proof the app runs even when its menu-bar icon is hidden.
    fn announce_ready(&mut self) {
        if !self.announced {
            self.announced = true;
            self.overlay.set(Phase::Ready);
        }
    }

    fn check_update(&mut self) {
        if self.update_checking {
            return;
        }
        self.update_checking = true;
        self.settings_toast("กำลังตรวจหาอัปเดต…");
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let _ = p.send_event(UserEvent::UpdateChecked(update::check().map_err(|e| format!("{e:#}"))));
        });
    }

    /// Asks about a newer release, unless the user is busy or another dialog is open (then later).
    /// Also restarts into an installed update once the user isn't recording.
    fn offer_pending_update(&mut self) {
        if self.busy() || self.dialog_open {
            return;
        }
        if self.restart_pending {
            self.restart_pending = false;
            match update::restart() {
                Ok(()) => self.quit = true,
                Err(e) => show_alert(format!("เปิด PoodPim เวอร์ชันใหม่ไม่ได้ กรุณาเปิดเอง: {e:#}")),
            }
            return;
        }
        if self.model_swap_pending && self.cfg.backend == Backend::Local {
            // Load the model again with the new files (int8 encoder or decoder) in place.
            self.model_swap_pending = false;
            if self.asleep {
                return; // freed when idle: the next recording loads the new files anyway
            }
            self.remove_fp32_after_load = true;
            self.asr = None;
            self.load_gen += 1;
            self.start_loading();
            return;
        }
        let Some(tag) = self.pending_update.take() else { return };
        self.dialog_open = true;
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let yes = platform::confirm(
                &format!(
                    "มี PoodPim {} แล้ว (เครื่องนี้ใช้ {})\n\nอัปเดตเลยไหม? PoodPim จะปิดแล้วเปิดใหม่เองภายในไม่กี่วินาที",
                    tag.trim_start_matches('v'),
                    update::CURRENT
                ),
                "อัปเดต",
                "ไว้ทีหลัง",
            );
            let _ = p.send_event(UserEvent::UpdateAnswer(tag, yes));
        });
    }

    fn ask_uninstall(&mut self) {
        if self.busy() || self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let has_model = model::is_ready(&model::model_dir());
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let answer = platform::confirm(
                "ลบ PoodPim ออกจากเครื่องนี้?\n\nจะลบแอป การตั้งค่า และการเปิดเองตอนเปิดเครื่อง",
                "ลบ PoodPim",
                "ยกเลิก",
            )
            .then(|| {
                has_model
                    && platform::confirm(
                        "ลบโมเดลถอดเสียง (2.4 GB) ด้วยไหม?\n\nถ้าเก็บไว้ ติดตั้งใหม่ภายหลังจะไม่ต้องดาวน์โหลดซ้ำ",
                        "ลบโมเดลด้วย",
                        "เก็บโมเดลไว้",
                    )
            });
            let _ = p.send_event(UserEvent::UninstallAnswer(answer));
        });
    }

    fn fail(&mut self, e: &str) {
        eprintln!("error: {e}");
        self.state = State::Error;
        self.status_text = format!("Error: {}", e.chars().take(80).collect::<String>());
    }

    fn on_hotkey(&mut self, h: HotkeyEvent, at: Instant) {
        match h {
            HotkeyEvent::TalkDown => {
                // Only soon after: the repeats come as soon as the hook sees the keyboard again;
                // a press much later is a new one, and may be a tap for hands-free.
                let after_hidden = self.after_hidden.take().is_some_and(|t| t.elapsed() < AFTER_HIDDEN);
                self.press_started = false;
                // Not set up yet: ask for what's missing instead of recording.
                if !self.busy() {
                    if self.cfg.backend == Backend::Lambda
                        && (self.cfg.server().api_key.is_empty() || self.key_rejected)
                    {
                        if self.cfg.server().url.is_empty() {
                            self.open_server_settings();
                        } else {
                            self.ask_api_key();
                        }
                        return;
                    }
                    if self.cfg.backend == Backend::Local && self.state == State::NeedsModel {
                        self.offer_download();
                        return;
                    }
                    // The model failed to load (or to download) once: try again, instead of a
                    // talk key that does nothing until the app is restarted.
                    if self.cfg.backend == Backend::Local && self.state == State::Error {
                        if model::is_ready(&model::model_dir()) {
                            eprintln!("talk key after an error: loading the model again");
                            self.start_backend();
                        } else {
                            self.offer_download();
                        }
                        return;
                    }
                }
                if self.state == State::Recording && self.stop_at.is_some() {
                    // Pressed again within TAIL of letting go: carry on with the same recording.
                    self.stop_at = None;
                    (self.key_up_seen, self.key_down_seen) = (0, platform::talk_key_down() == Some(true));
                } else if self.state == State::Recording && self.handsfree {
                    self.handsfree = false;
                    self.finish();
                } else if self.state == State::Idle {
                    let started = Instant::now();
                    // Before opening the microphone, which can take a second: an Esc meanwhile cancels,
                    // and the key may be let go meanwhile (`check_talk_key`).
                    platform::set_listening(true);
                    self.key_down_seen = platform::talk_key_down() == Some(true);
                    match self.recorder.start(&self.cfg.microphone) {
                        Ok(()) => {
                            // From the key press, not from now: opening the microphone can take
                            // a second or more (Windows, Bluetooth headsets), and timing from here
                            // turned short holds into taps (hands-free mode).
                            self.pressed_at = at;
                            self.hold_only = after_hidden;
                            self.press_started = true;
                            eprintln!("microphone started in {} ms", started.elapsed().as_millis());
                            self.state = State::Recording;
                            (self.mic_len, self.mic_seen, self.mic_lost) = (0, Instant::now(), false);
                            self.key_up_seen = 0;
                            self.secure_seen = platform::secure_input();
                            self.status_text = "Listening…".into();
                            eprintln!("listening");
                            self.overlay.set(Phase::Listening { handsfree: false });
                            self.start_pipeline();
                        }
                        Err(e) => {
                            platform::set_listening(false);
                            eprintln!("microphone: {e:#}");
                            self.status_text = format!("Microphone error: {e}");
                            self.overlay.set(Phase::Failed);
                        }
                    }
                } else if self.state == State::Transcribing {
                    // E.g. a server waking up from a cold start: say why nothing happens.
                    eprintln!("talk key pressed while still transcribing: nothing started");
                    self.overlay.set(Phase::StillWorking);
                } else if self.state == State::TranscribingFile {
                    eprintln!("talk key pressed while transcribing a file: nothing started");
                    self.overlay.set(Phase::FileBusy);
                } else if self.state == State::Loading {
                    // E.g. just after login: say so, instead of a key that seems dead.
                    eprintln!("talk key pressed while the model is loading: nothing started");
                    self.overlay.set(Phase::ModelLoading);
                } else if self.state == State::Downloading {
                    self.overlay.set(Phase::ModelDownloading);
                }
            }
            HotkeyEvent::TalkUp => {
                if self.state == State::Recording && !self.handsfree {
                    let held = at.saturating_duration_since(self.pressed_at);
                    eprintln!("talk key held {} ms", held.as_millis());
                    if held < TAP && !self.hold_only {
                        self.handsfree = true;
                        if self.cfg.live_typing
                            && let Some(seg) = self.segmenter.as_mut()
                        {
                            seg.set_live(); // shorter parts, so text appears sooner
                        }
                        self.status_text = format!("Listening… (tap {TALK_KEY_LABEL} to finish)");
                        self.overlay.set(Phase::Listening { handsfree: true });
                    } else {
                        self.stop_at = Some(at + TAIL);
                    }
                }
            }
            HotkeyEvent::TalkAborted => {
                if self.state == State::Recording && self.press_started {
                    eprintln!("the talk key was part of a key combination: recording dropped");
                    self.drop_recording();
                    self.status_text = self.ready_text();
                    self.overlay.set(Phase::Hidden);
                }
            }
            HotkeyEvent::Escape => {
                platform::set_listening(false); // (the hook took this Esc: it cancels)
                if self.state == State::Recording {
                    self.drop_recording();
                    eprintln!("cancelled with Esc");
                    self.status_text = "Cancelled".into();
                    self.overlay.set(Phase::Cancelled);
                }
            }
            HotkeyEvent::PermissionMissing => {
                eprintln!("Cannot watch the keyboard yet: grant Accessibility permission");
                self.needs_permission = true;
                // Taken away while dictating: neither the talk key nor Esc could end it now.
                if self.state == State::Recording {
                    eprintln!("finishing the recording: it could not be stopped from the keyboard");
                    self.handsfree = false;
                    self.finish();
                }
            }
            HotkeyEvent::Ready => {
                eprintln!("watching the {TALK_KEY_LABEL} key");
                self.needs_permission = false;
            }
        }
    }

    /// Stops the recording and throws it away (Esc, or the talk key was part of a combination).
    fn drop_recording(&mut self) {
        platform::set_listening(false);
        self.stop_at = None;
        self.recorder.stop_raw();
        self.segmenter = None;
        if let Some(p) = self.pipeline.take() {
            // Not joined: on the event loop that would wait for the segment in progress. Its
            // result is ignored (rec_gen), and the next recording's segments queue behind it.
            drop(p.cancel());
        }
        self.rec_gen += 1;
        if let Some((typer, _)) = self.typer.take() {
            typer.cancel(); // what was typed already stays; parts still queued don't
        }
        self.end_live();
        self.handsfree = false;
        (self.press_started, self.hold_only) = (false, false);
        self.state = State::Idle;
    }

    /// The model freed when idle, loading again (a load already under way, e.g. for a recording
    /// cancelled and started again, is shared).
    fn wake_model(&mut self) -> Arc<Waking> {
        match &self.waking {
            Some(w) => w.clone(),
            None => {
                let w = Arc::new(Waking::default());
                self.waking = Some(w.clone());
                w.start(self.proxy.clone(), self.load_gen);
                w
            }
        }
    }

    /// Menu › Transcribe a file…: pick a file; while one is being transcribed, Cancel.
    fn transcribe_file_clicked(&mut self) {
        if self.state == State::TranscribingFile {
            if let Some(c) = &self.file_cancel {
                c.store(true, std::sync::atomic::Ordering::SeqCst);
                self.status_text = format!("Cancelling {}…", self.file_name);
            }
            return;
        }
        if self.state != State::Idle || self.dialog_open {
            return;
        }
        if let Err(why) = self.file_model_check() {
            show_alert(why);
            return;
        }
        self.dialog_open = true;
        if self.cfg.backend != Backend::Lambda {
            return self.open_file_picker();
        }
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let go = platform::confirm(
                "การถอดเสียงจากไฟล์ทำในเครื่องนี้ (ไฟล์ยาวส่งไปเซิร์ฟเวอร์ไม่ได้) PoodPim จะโหลดโมเดลถอดเสียงในเครื่อง \
                 ซึ่งใช้หน่วยความจำเพิ่มราว 1 GB ระหว่างถอด แล้วคืนเมื่อเสร็จ\n\nเลือกไฟล์เลยไหม?",
                "เลือกไฟล์",
                "ยกเลิก",
            );
            let _ = p.send_event(UserEvent::FileGoAhead(go));
        });
    }

    /// The Open panel (macOS: on this thread, the main one; it returns at once). Its answer
    /// comes back as FileChosen; `dialog_open` is set until then.
    fn open_file_picker(&mut self) {
        let p = self.proxy.clone();
        platform::choose_file("เลือกไฟล์เสียงหรือวิดีโอที่จะถอดเสียง", move |path| {
            let _ = p.send_event(UserEvent::FileChosen(path));
        });
    }

    /// Files are transcribed on this computer (a long file is too much for the server, and
    /// costs per request): with the serverless backend that needs the model downloaded too.
    fn file_model_check(&self) -> Result<(), String> {
        if self.cfg.backend == Backend::Lambda && !model::is_ready(&model::model_dir()) {
            return Err("การถอดเสียงจากไฟล์ทำในเครื่องนี้เท่านั้น จึงต้องมีโมเดลถอดเสียงในเครื่องก่อน (1 GB)\n\n\
                        ดาวน์โหลดได้ที่เมนู 🎙 › Settings… › เลือก \"เครื่องนี้\" (เลือกกลับเป็นเซิร์ฟเวอร์ของทีมได้หลังดาวน์โหลดเสร็จ)"
                .into());
        }
        Ok(())
    }

    /// What a file job transcribes with, one part at a time: the loaded model; the model freed
    /// when idle, loading again; or with the serverless backend, the model on this computer,
    /// loaded for this job only and freed after it.
    fn file_transcriber(&mut self) -> Result<Transcriber, String> {
        self.file_model_check()?;
        let (mixed, vocabulary) = (self.cfg.mixed_english, self.vocabulary.clone());
        Ok(match (&self.cfg.backend, &self.asr) {
            (Backend::Local, Some(asr)) => {
                let asr = asr.clone();
                Box::new(move |a: &[f32]| asr.lock().unwrap_or_else(|e| e.into_inner()).transcribe(a, mixed, &vocabulary))
            }
            (Backend::Local, None) => {
                let waking = self.wake_model();
                Box::new(move |a: &[f32]| match waking.wait() {
                    Ok(asr) => asr.lock().unwrap_or_else(|e| e.into_inner()).transcribe(a, mixed, &vocabulary),
                    Err(e) => Err(anyhow::anyhow!("loading the model: {e}")),
                })
            }
            (Backend::Lambda, _) => {
                // Loaded on the first part; a failed load is not tried again for every part.
                let mut own: Option<Result<OpenThaiAsr, String>> = None;
                Box::new(move |a: &[f32]| {
                    let loaded = own.get_or_insert_with(|| {
                        OpenThaiAsr::load_with(&model::model_dir(), false).map_err(|e| format!("loading the model: {e:#}"))
                    });
                    match loaded {
                        Ok(asr) => asr.transcribe(a, mixed, &vocabulary),
                        Err(e) => Err(anyhow::anyhow!("{e}")),
                    }
                })
            }
        })
    }

    /// Starts transcribing `src` into a text file next to it, on a thread of its own.
    fn start_file_job(&mut self, src: std::path::PathBuf) {
        if self.state != State::Idle {
            // A recording started while the picker was open: say so rather than drop the file.
            eprintln!("file picked while busy ({:?}): not started", self.state);
            return show_alert("ยังเริ่มถอดเสียงจากไฟล์ไม่ได้ เพราะ PoodPim กำลังทำงานอื่นอยู่\n\nเสร็จแล้วเลือกไฟล์ใหม่อีกครั้งได้ที่เมนู 🎙 › Transcribe a file…".into());
        }
        let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.reload_vocabulary();
        let transcribe = match self.file_transcriber() {
            Ok(t) => t,
            Err(why) => return show_alert(why),
        };
        let fixes = Corrections::new(&corrections::read_user_list().0);
        let digits = self.cfg.numbers_as_digits;
        let finish = move |t: &str| corrections::finish(t, &fixes, digits);
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.file_gen += 1;
        self.file_cancel = Some(cancel.clone());
        self.file_name = name.clone();
        self.state = State::TranscribingFile;
        self.status_text = format!("Reading {name}…");
        self.overlay.set(Phase::FileStarted);
        eprintln!("transcribing a file");
        let (p, generation) = (self.proxy.clone(), self.file_gen);
        std::thread::spawn(move || {
            let progress = |pr| {
                let _ = p.send_event(UserEvent::FileProgress(generation, pr));
            };
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                filejob::run(&src, transcribe, finish, &cancel, progress)
            }));
            let r = match r {
                Ok(Ok(report)) => Ok(report),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(_) => Err("the transcription crashed".to_string()),
            };
            let _ = p.send_event(UserEvent::FileDone(generation, src, r));
        });
    }

    /// Says how a file job ended; offers to open the text.
    fn file_done(&mut self, src: &std::path::Path, r: Result<filejob::Report, String>) {
        let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let report = match r {
            Ok(report) => report,
            Err(e) => {
                eprintln!("transcribing a file: {e}");
                self.overlay.set(Phase::Failed);
                self.status_text = format!("Couldn't transcribe {name}");
                let hint = if cfg!(target_os = "macos") && (e.contains("not permitted") || e.contains("denied")) {
                    "\n\nmacOS อาจยังไม่ให้ PoodPim เปิดไฟล์ในโฟลเดอร์นี้: ดูที่ System Settings › Privacy & Security › Files and Folders"
                } else {
                    ""
                };
                return show_alert(format!("ถอดเสียงจากไฟล์ {name} ไม่ได้\n\n{e}{hint}"));
            }
        };
        let Some(out) = report.out.clone() else {
            if report.cancelled {
                self.overlay.set(Phase::Cancelled);
                self.status_text = "Cancelled".into();
                return;
            }
            self.overlay.set(if report.errors.is_empty() { Phase::NoSpeech } else { Phase::Failed });
            let mut message = if report.errors.is_empty() {
                format!("ไม่ได้ยินเสียงพูดในไฟล์ {name}")
            } else {
                format!("ถอดเสียงจากไฟล์ {name} ไม่สำเร็จ\n\n{}", report.errors[0])
            };
            if let Some(why) = &report.incomplete {
                message += &format!("\n\n⚠ อ่านไฟล์ได้ไม่ครบ: {why}");
            }
            return show_alert(message);
        };
        eprintln!(
            "file: {:.0} s, {} part(s) with text, {} failed{}{}",
            report.total_s,
            report.parts,
            report.errors.len(),
            if report.cancelled { ", cancelled" } else { "" },
            report.incomplete.as_deref().map(|w| format!(", only partly read: {w}")).unwrap_or_default()
        );
        if report.cancelled {
            self.overlay.set(Phase::Cancelled);
            self.status_text = format!("Cancelled: the text so far is in {}", out.display());
            return;
        }
        let mut message = format!("ถอดเสียงจาก {name} ({}) เสร็จแล้ว\n\nข้อความอยู่ที่\n{}", clock(report.total_s), out.display());
        if let Some(why) = &report.incomplete {
            message += &format!("\n\n⚠ อ่านไฟล์ได้ไม่ครบ: {why}");
        }
        if !report.errors.is_empty() {
            message += &format!("\n\n⚠ ถอดไม่สำเร็จ {} ช่วง: {}", report.errors.len(), report.errors[0]);
        }
        self.overlay.set(Phase::FileDone);
        if self.dialog_open {
            return show_alert(message);
        }
        self.dialog_open = true;
        message += "\n\nเปิดไฟล์ข้อความเลยไหม?";
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let open = platform::confirm(&message, "เปิดไฟล์", "ปิด");
            let _ = p.send_event(UserEvent::FileOpenAnswer(open.then_some(out)));
        });
    }

    /// Sets up the segmenter and the background transcription worker for a new recording.
    fn start_pipeline(&mut self) {
        self.rec_gen += 1;
        self.preview_from = (usize::MAX, 0); // a new recording: its first preview needs no new sound
        self.end_live();
        let (p, generation) = (self.proxy.clone(), self.rec_gen);
        let on_part = move |t: &str| {
            let _ = p.send_event(UserEvent::Part(generation, t.to_string()));
        };
        self.reload_vocabulary();
        self.segmenter = Some(Segmenter::new(self.recorder.rate()));
        let mixed = self.cfg.mixed_english;
        self.last_used = Instant::now();
        self.pipeline = Some(match self.cfg.backend {
            Backend::Local if self.asr.is_none() => {
                // Freed when idle: load it while the user speaks; segments wait for it.
                let waking = self.wake_model();
                let vocabulary = self.vocabulary.clone();
                Pipeline::start(
                    move |a: &[f32]| match waking.wait() {
                        Ok(asr) => asr.lock().unwrap_or_else(|e| e.into_inner()).transcribe(a, mixed, &vocabulary),
                        Err(e) => Err(anyhow::anyhow!("loading the model: {e}")),
                    },
                    None,
                    on_part,
                )
            }
            Backend::Local => {
                let asr = self.asr.clone().expect("model loaded in Idle state");
                let vocabulary = self.vocabulary.clone();
                Pipeline::start(move |a: &[f32]| asr.lock().unwrap_or_else(|e| e.into_inner()).transcribe(a, mixed, &vocabulary), None, on_part)
            }
            Backend::Lambda => {
                let stale = self.last_server_ok.is_none_or(|t| t.elapsed() > ASSUME_WARM);
                if stale && self.warm.is_none() {
                    self.spawn_warm(false); // the cold start overlaps with speaking
                }
                // Segments wait for the wake-up call: one Lambda instance serves one request at a
                // time, so sending earlier would start a second, cold instance.
                let server = self.cfg.server();
                let vocabulary = self.vocabulary_header.clone();
                Pipeline::start(move |a: &[f32]| server.transcribe(a, mixed, &vocabulary), self.warm.take(), on_part)
            }
        });
    }

    /// A part of the recording was transcribed: hands-free with `live_typing`, type it now
    /// (into the window the first part went to) instead of waiting for the end.
    fn type_part(&mut self, generation: u64, text: String) {
        if generation != self.rec_gen {
            return; // cancelled recording
        }
        let text = corrections::finish(&text, &self.corrections, self.cfg.numbers_as_digits);
        self.heard.push(text.clone());
        self.show_heard();
        let live = self.cfg.live_typing && !self.cfg.copy_only && self.handsfree && self.state == State::Recording;
        if !live || self.typed.len() + 1 != self.heard.len() {
            return; // left for the end, with every part after it (they must stay in order)
        }
        let window = platform::focused_window();
        if self.typer.as_ref().is_some_and(|(_, w)| *w != window) || platform::foreground_is_elevated() || !platform::accessibility_trusted(false) {
            eprintln!("the focused window changed: the rest waits for the end");
            return;
        }
        let (restore, p, generation) = (self.cfg.restore_clipboard, self.proxy.clone(), self.rec_gen);
        let ended = move |e: inserter::Ending| {
            let phase = match e {
                inserter::Ending::Copied { elevated } => Phase::Copied { elevated },
                inserter::Ending::Failed => Phase::Failed,
            };
            let _ = p.send_event(UserEvent::Delivered(generation, phase));
        };
        let (typer, _) = self.typer.get_or_insert_with(|| (inserter::LiveTyper::start(restore, window, ended), window));
        typer.type_text(continuation(&self.typed, &text));
        self.typed.push(text);
    }

    /// Forgets this recording's live typing; the typer puts the clipboard back once done.
    fn end_live(&mut self) {
        self.heard.clear();
        self.preview = (0, String::new(), false);
        self.typed.clear();
        if let Some((typer, _)) = self.typer.take() {
            typer.finish();
        }
    }

    /// Whether the microphone has stopped: its stream reported an error, or no sound has come
    /// for MIC_FIRST since the start / MIC_GAP since the last. `len`: samples recorded so far.
    fn mic_stopped(&mut self, len: usize) -> bool {
        if len > self.mic_len {
            (self.mic_len, self.mic_seen) = (len, Instant::now());
        }
        let quiet_for = if self.mic_len == 0 { MIC_FIRST } else { MIC_GAP };
        let why = match self.recorder.error() {
            Some(e) => e,
            None if self.mic_seen.elapsed() >= quiet_for => format!("no sound for {} s", quiet_for.as_secs()),
            None => return false,
        };
        eprintln!("microphone stopped ({why}): finishing with what was heard");
        true
    }

    /// Hold to talk: if the talk key is up although its release never came (the event tap
    /// misses it under Secure Input, e.g. in a password field), let go of it now.
    fn check_talk_key(&mut self) {
        let holding = self.state == State::Recording && !self.handsfree && self.stop_at.is_none();
        let down = if holding { platform::talk_key_down() } else { None };
        self.key_down_seen |= down == Some(true);
        // macOS: the key itself says it is up. Windows: the hook can't see the keyboard now
        // (locked screen, UAC prompt, administrator window in front), so its release would be lost.
        let seen_up = down == Some(false) && self.key_down_seen;
        let up = seen_up || (holding && platform::release_hidden());
        if !up || self.pressed_at.elapsed() < Duration::from_millis(600) {
            self.key_up_seen = 0;
            return;
        }
        // A full second of "up" before acting: cutting off someone who is still holding the key
        // would be worse than recording a little longer.
        self.key_up_seen += 1;
        if self.key_up_seen >= 10 {
            eprintln!("the talk key is up but its release was missed: finishing");
            self.key_up_seen = 0;
            if !seen_up {
                platform::forget_talk_key();
                self.after_hidden = Some(Instant::now());
            }
            self.on_hotkey(HotkeyEvent::TalkUp, Instant::now());
        }
    }

    /// `live_preview`: the pill shows the parts heard so far and the unfinished one as heard
    /// up to now (its end, which is what changes).
    fn show_heard(&mut self) {
        self.secure_seen |= platform::secure_input();
        if !self.cfg.live_preview {
            return;
        }
        if self.secure_seen {
            self.overlay.set_preview(""); // likely a password: not on screen
            return;
        }
        // Shown until its part is transcribed (also once cut), so the words don't flicker.
        let done = self.pipeline.as_ref().map_or(0, |p| p.progress().0);
        let (part, text, clipped) = &self.preview;
        let pending = if *part >= done.max(self.heard.len()) { text.as_str() } else { "" };
        let shown = if *clipped && !pending.is_empty() {
            format!("…{pending}") // the middle of the part wasn't transcribed: don't join across it
        } else {
            let all: Vec<&str> = self.heard.iter().map(String::as_str).chain([pending]).collect();
            crate::pipeline::join_parts(&all)
        };
        self.overlay.set_preview(&shown);
    }

    /// `live_preview` with the local model: transcribes the unfinished part again in the
    /// background, at most every PREVIEW_EVERY, only while the model has nothing else to do
    /// (typing a finished part never waits for a preview to start).
    fn start_preview(&mut self) {
        if !self.cfg.live_preview || self.cfg.backend != Backend::Local || self.state != State::Recording || self.stop_at.is_some() {
            return; // (stop_at: the key was let go, the last part comes next)
        }
        use std::sync::atomic::Ordering;
        let (Some(asr), Some(seg), Some(pipe)) = (self.asr.clone(), self.segmenter.as_ref(), self.pipeline.as_ref()) else { return };
        let (done, sent) = pipe.progress();
        let every = PREVIEW_EVERY.max(Duration::from_millis(3 * self.preview_cost.load(Ordering::SeqCst)));
        if done < sent || self.preview_busy.load(Ordering::SeqCst) || self.preview_at.elapsed() < every {
            return;
        }
        let (start, rate, part) = (seg.start(), self.recorder.rate(), sent);
        let window = (PREVIEW_WINDOW.as_secs_f32() * rate as f32) as usize;
        // New sound since the last preview of this part (all of it, for a new part)?
        let seen = if self.preview_from.0 == part { self.preview_from.1.max(start) } else { start };
        let (raw, clipped, fresh, len) = self.recorder.with_buffer(|b| {
            let from = start.max(b.len().saturating_sub(window));
            let fresh = b.get(seen..).is_some_and(|new| has_sound(new, rate));
            (b.get(from..).map(<[f32]>::to_vec).unwrap_or_default(), from > start, fresh, b.len())
        });
        if raw.len() < rate as usize || !fresh {
            return; // under a second: nothing worth showing yet; or nothing new was said
        }
        self.preview_at = Instant::now();
        self.preview_from = (part, len);
        self.preview_busy.store(true, Ordering::SeqCst);
        let (busy, cost, p, generation) = (self.preview_busy.clone(), self.preview_cost.clone(), self.proxy.clone(), self.rec_gen);
        let (mixed, vocabulary) = (self.cfg.mixed_english, self.vocabulary.clone());
        std::thread::spawn(move || {
            let took = Instant::now();
            let audio = resample_fast(&raw, rate, SAMPLE_RATE);
            // A finished part may have just been queued: it goes first. (Greedy decoding instead
            // of beam search saved only ~10% of the preview's CPU, and wrote the English words
            // in Thai letters, so the pill would show other words than the ones typed.)
            let text = match asr.try_lock() {
                Ok(mut asr) => asr.transcribe(&audio, mixed, &vocabulary).ok(),
                Err(_) => None,
            };
            cost.store(took.elapsed().as_millis() as u64, Ordering::SeqCst);
            busy.store(false, Ordering::SeqCst);
            if let Some(text) = text {
                let _ = p.send_event(UserEvent::Preview(generation, part, text, clipped));
            }
        });
    }

    /// When the idle model is due to be freed (None: not applicable).
    fn unload_at(&self) -> Option<Instant> {
        let applies = self.cfg.unload_when_idle && self.cfg.backend == Backend::Local && self.state == State::Idle;
        // POODPIM_UNLOAD_AFTER_S: a shorter wait, for testing.
        let after = std::env::var("POODPIM_UNLOAD_AFTER_S").ok().and_then(|v| v.parse().ok()).map_or(UNLOAD_AFTER, Duration::from_secs);
        (applies && self.asr.is_some() && self.pipeline.is_none()).then(|| self.last_used + after)
    }

    fn unload_if_idle(&mut self) {
        if self.unload_at().is_some_and(|t| Instant::now() >= t) {
            self.asr = None;
            self.asleep = true;
            eprintln!("freed the model: no dictation for a while");
        }
    }

    /// Feeds the microphone level of the last 50 ms to the overlay.
    fn update_level(&mut self) {
        let n = (self.recorder.rate() / 20) as usize;
        let rms = self.recorder.with_buffer(|b| {
            let tail = &b[b.len().saturating_sub(n)..];
            if tail.is_empty() { 0.0 } else { (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt() }
        });
        self.overlay.set_level(rms);
    }

    /// Sends finished segments of the ongoing recording to the worker.
    fn poll_segments(&mut self) {
        let (Some(seg), Some(pipe)) = (self.segmenter.as_mut(), self.pipeline.as_mut()) else { return };
        let rate = self.recorder.rate();
        let (cuts, len) = self.recorder.with_buffer(|b| {
            let mut cuts = Vec::new();
            while let Some((s, e)) = seg.poll(b) {
                cuts.push(b[s..e].to_vec());
            }
            (cuts, b.len())
        });
        for raw in cuts {
            pipe.push(resample_fast(&raw, rate, SAMPLE_RATE));
        }
        let (done, sent) = pipe.progress();
        if sent > 0 {
            let hint = if self.handsfree { format!(" · tap {TALK_KEY_LABEL} to finish") } else { String::new() };
            self.status_text = format!("Listening… {done}/{sent} parts transcribed{hint}");
        }
        self.show_heard(); // a part may have finished since (unchanged text costs nothing)
        if self.mic_stopped(len) {
            self.mic_lost = true;
            self.handsfree = false;
            self.finish();
            return;
        }
        self.check_talk_key();
        if len as f64 / rate as f64 > MAX_RECORDING.as_secs_f64() {
            eprintln!("recording reached {} min: finishing", MAX_RECORDING.as_secs() / 60);
            self.handsfree = false;
            self.finish();
            return;
        }
        self.start_preview();
    }

    fn finish(&mut self) {
        if !platform::stop_listening() {
            return; // an Esc was taken from the keyboard and its event comes next: it cancels
        }
        let t_finish = Instant::now();
        self.stop_at = None;
        let raw = self.recorder.stop_raw();
        let rate = self.recorder.rate();
        eprintln!("recorded {:.2}s at {rate} Hz", raw.len() as f64 / rate.max(1) as f64);
        // Real silence still has a little noise; nothing at all means a muted microphone.
        self.mic_silent = raw.len() > rate as usize / 2 && raw.iter().all(|&v| v == 0.0);
        if self.mic_silent {
            eprintln!("the microphone sent only silence (muted?)");
        }
        let (Some(seg), Some(mut pipe)) = (self.segmenter.take(), self.pipeline.take()) else {
            self.state = State::Idle;
            self.overlay.set(Phase::Hidden);
            return;
        };
        if seg.start() < raw.len() {
            pipe.push(resample_fast(&raw[seg.start()..], rate, SAMPLE_RATE));
        }
        self.state = State::Transcribing;
        self.overlay.set(Phase::Working);
        // The event loop's share of release → text (stopping the microphone, the last part).
        eprintln!("finish: {} ms on the event loop", t_finish.elapsed().as_millis());
        self.status_text = match self.cfg.backend {
            Backend::Local => "Transcribing…".into(),
            Backend::Lambda => "Transcribing on server…".into(),
        };
        let (p, generation) = (self.proxy.clone(), self.rec_gen);
        let worker = pipe.finish();
        std::thread::spawn(move || {
            let out = worker.join().unwrap_or_else(|_| Outcome {
                text: String::new(),
                parts: Vec::new(),
                segments: 0,
                audio_s: 0.0,
                errors: vec!["transcription worker crashed".into()],
            });
            let _ = p.send_event(UserEvent::Transcribed(generation, out));
        });
    }

    /// Pastes in the background; returns what the overlay should say.
    fn paste(&self, text: String) -> Phase {
        if text.is_empty() {
            return Phase::Done;
        }
        let trusted = platform::accessibility_trusted(false);
        if self.cfg.copy_only || !trusted || crate::platform::foreground_is_elevated() {
            if !trusted {
                eprintln!("no Accessibility permission: copied instead of pasting");
            } else if !self.cfg.copy_only {
                eprintln!("focused app runs as administrator: copied instead of pasting");
            }
            return match inserter::copy_text(&text) {
                Ok(()) => Phase::Copied { elevated: trusted && !self.cfg.copy_only },
                Err(e) => {
                    eprintln!("copy error: {e:#}");
                    Phase::Failed
                }
            };
        }
        let (restore, p, generation) = (self.cfg.restore_clipboard, self.proxy.clone(), self.rec_gen);
        let window = platform::focused_window(); // here: on macOS it is current on the main thread
        std::thread::spawn(move || match inserter::paste_text(&text, restore, window) {
            Ok(true) => {}
            Ok(false) => {
                let _ = p.send_event(UserEvent::Delivered(generation, Phase::Copied { elevated: false }));
            }
            Err(e) => {
                eprintln!("paste error: {e:#}");
                let _ = p.send_event(UserEvent::Delivered(generation, Phase::Failed));
            }
        });
        Phase::Done
    }

    fn open_settings(&mut self, target: &EventLoopWindowTarget<UserEvent>) {
        if let Some(w) = &self.settings {
            w.focus();
            return;
        }
        // A menu-bar app has no Dock icon or menu bar; while Settings is open it gets both, so
        // the window comes to the front and ⌘C / ⌘V work in its fields.
        #[cfg(target_os = "macos")]
        {
            use tao::platform::macos::{ActivationPolicy, EventLoopWindowTargetExtMacOS};
            target.set_activation_policy_at_runtime(ActivationPolicy::Regular);
            self.edit_menu.get_or_insert_with(edit_menu).init_for_nsapp();
        }
        #[cfg(windows)]
        if let Some(e) = crate::settings::webview_missing() {
            eprintln!("settings window: WebView2 runtime missing: {e}");
            std::thread::spawn(|| {
                if platform::confirm(
                    "หน้าตั้งค่าต้องใช้ Microsoft Edge WebView2 Runtime ซึ่งเครื่องนี้ยังไม่มี\n\n\
                     (มีมากับ Windows 11 และ Windows 10 ที่อัปเดตแล้ว) ติดตั้งฟรีจาก Microsoft\n\nเปิดหน้าดาวน์โหลดไหม?",
                    "เปิดหน้าดาวน์โหลด",
                    "ไว้ทีหลัง",
                ) {
                    platform::open_url("https://developer.microsoft.com/microsoft-edge/webview2/");
                }
            });
            return;
        }
        let p = self.proxy.clone();
        let t = Instant::now();
        match crate::settings::SettingsWindow::open(target, move |m| {
            let _ = p.send_event(UserEvent::Settings(m));
        }) {
            Ok(w) => {
                // Built on the event loop (webview included): how long the app was held up.
                eprintln!("settings window built in {} ms", t.elapsed().as_millis());
                self.settings = Some(w);
            }
            Err(e) => {
                eprintln!("settings window: {e:#}");
                show_alert(format!("เปิดหน้าตั้งค่าไม่ได้: {e:#}"));
            }
        }
    }

    fn close_settings(&mut self, target: &EventLoopWindowTarget<UserEvent>) {
        self.settings = None;
        #[cfg(target_os = "macos")]
        {
            use tao::platform::macos::{ActivationPolicy, EventLoopWindowTargetExtMacOS};
            target.set_activation_policy_at_runtime(ActivationPolicy::Accessory);
        }
        let _ = target;
    }

    /// What the Settings page shows (JSON for its render()).
    fn settings_state(&self) -> String {
        serde_json::json!({
            "version": update::CURRENT,
            "status": if self.needs_permission { "ต้องให้สิทธิ์ Accessibility ก่อน" } else { self.status_text.as_str() },
            "backend": if self.cfg.backend == Backend::Lambda { "lambda" } else { "local" },
            "serverUrl": self.cfg.server_url,
            "hasKey": !self.cfg.api_key.is_empty(),
            "mixed": self.cfg.mixed_english,
            "digits": self.cfg.numbers_as_digits,
            "restore": self.cfg.restore_clipboard,
            "copyOnly": self.cfg.copy_only,
            "mics": self.mics,
            "mic": self.cfg.microphone,
            "defaultMic": self.default_mic,
            "unload": self.cfg.unload_when_idle,
            "live": self.cfg.live_typing,
            "preview": self.cfg.live_preview,
            "autostart": self.cfg.start_at_login,
            "vocabCount": self.vocab_count,
            "correctionsCount": self.corrections_count,
            "talkKey": TALK_KEY_LABEL,
            "needsModel": self.cfg.backend == Backend::Local && self.state == State::NeedsModel,
            "needsPermission": self.needs_permission,
            "updates": update::supported(),
        })
        .to_string()
    }

    fn refresh_mics(&mut self) {
        self.mics = crate::audio::microphones();
        self.default_mic = crate::audio::default_microphone().unwrap_or_default();
    }

    fn show_settings_state(&mut self) {
        if self.settings.is_some() {
            let state = self.settings_state();
            if let Some(w) = &mut self.settings {
                w.show_state(&state);
            }
        }
    }

    fn settings_toast(&self, text: &str) {
        if let Some(w) = &self.settings {
            w.toast(text);
        }
    }

    /// A message from the Settings page.
    fn settings_message(&mut self, msg: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(msg) else { return };
        let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
        eprintln!("settings: {} {}", v["cmd"].as_str().unwrap_or("?"), v["key"].as_str().unwrap_or(""));
        match v["cmd"].as_str().unwrap_or("") {
            "mics" => {
                self.refresh_mics();
                if let Some(w) = &mut self.settings {
                    w.forget_state();
                }
            }
            "mic" => {
                self.cfg.microphone = text("value");
                self.cfg.save();
                eprintln!("microphone: {}", if self.cfg.microphone.is_empty() { "the system's" } else { "chosen" });
                Recorder::warm_up(self.cfg.microphone.clone());
            }
            "ready" => {
                self.refresh_mics();
                self.reload_vocabulary();
                if let Some(w) = &mut self.settings {
                    w.forget_state();
                }
            }
            "close" => self.settings_close = true,
            "set" => {
                let on = v["value"].as_bool().unwrap_or(false);
                match v["key"].as_str().unwrap_or("") {
                    "mixed" => self.cfg.mixed_english = on,
                    "digits" => self.cfg.numbers_as_digits = on,
                    "live" => self.cfg.live_typing = on,
                    "preview" => self.cfg.live_preview = on,
                    "restore" => self.cfg.restore_clipboard = on,
                    "copyOnly" => self.cfg.copy_only = on,
                    "unload" => {
                        self.cfg.unload_when_idle = on;
                        self.last_used = Instant::now();
                    }
                    "autostart" => self.cfg.start_at_login = on,
                    _ => return,
                }
                self.cfg.save();
                if v["key"] == "autostart" {
                    self.apply_autostart();
                }
            }
            "backend" => {
                let backend = if text("value") == "lambda" { Backend::Lambda } else { Backend::Local };
                if backend != self.cfg.backend {
                    if self.busy() {
                        self.settings_toast("รอให้ถอดเสียงเสร็จก่อน");
                    }
                    self.switch_backend(backend);
                }
            }
            "server" => {
                let url = text("url");
                let key = match text("key") {
                    k if k.is_empty() => self.cfg.api_key.clone(),
                    k => k,
                };
                if !url.starts_with("https://") || key.is_empty() {
                    return;
                }
                self.cfg.server_url = url;
                self.cfg.api_key = key;
                self.cfg.backend = Backend::Lambda;
                self.cfg.save();
                self.last_server_ok = None;
                self.key_rejected = false;
                if !self.busy() {
                    self.start_backend();
                }
                self.settings_toast("บันทึกแล้ว");
            }
            "open" => match text("what").as_str() {
                "vocabulary" => {
                    self.reload_vocabulary();
                    platform::open_in_editor(&hotwords::user_file());
                }
                "corrections" => {
                    self.reload_vocabulary();
                    platform::open_in_editor(&corrections::user_file());
                }
                "logs" => {
                    if let Some(dir) = platform::log_dir() {
                        platform::open_folder(&dir);
                    }
                }
                "accessibility" => platform::open_permission_settings(),
                _ => {}
            },
            "correction" => {
                let (heard, wanted) = (text("heard"), text("wanted"));
                if heard.is_empty() || wanted.is_empty() {
                    return;
                }
                match corrections::add_to_user_file(&heard, &wanted) {
                    Ok(()) => {
                        self.reload_vocabulary();
                        self.settings_toast(&format!("เพิ่มแล้ว: {heard} → {wanted}"));
                    }
                    Err(e) => self.settings_toast(&format!("บันทึกไม่ได้: {e}")),
                }
            }
            "download" => self.start_download(),
            "update" => self.check_update(),
            "uninstall" => self.ask_uninstall(),
            _ => {}
        }
    }

    /// Fills menu › Recent: one submenu per transcript, with Paste and Fix a word….
    fn rebuild_recent(&mut self, menu: &Submenu) {
        for entry in self.recent_entries.drain(..) {
            let _ = menu.remove(&entry);
        }
        self.recent_ids.clear();
        for text in &self.recent {
            let label: String = if text.chars().count() > 40 { text.chars().take(40).chain("…".chars()).collect() } else { text.clone() };
            let entry = Submenu::new(label.replace('&', "&&"), true);
            let paste = MenuItem::new("Paste", true, None);
            let fix = MenuItem::new("Fix a word…", true, None);
            let _ = entry.append_items(&[&paste, &fix]);
            let _ = menu.append(&entry);
            self.recent_ids.push((paste.id().clone(), fix.id().clone()));
            self.recent_entries.push(entry);
        }
        menu.set_enabled(!self.recent.is_empty());
    }

    fn refresh(&mut self, ui: &Ui) {
        self.offer_pending_update();
        if self.warm.as_ref().is_some_and(|w| w.is_finished()) && self.state != State::Recording {
            self.warm = None;
        }
        let mut shown = ui.shown.borrow_mut();
        let status = if self.needs_permission { "⚠ Allow PoodPim in Accessibility to use the talk key" } else { &self.status_text };
        if shown.status != status {
            shown.status = status.to_string();
            ui.status.set_text(status);
        }
        let need_download = self.cfg.backend == Backend::Local
            && matches!(self.state, State::NeedsModel | State::Error)
            && !model::is_ready(&model::model_dir());
        // Menu: status, [permission], separator, Paste last, Recent, Add correction, separator,
        // Transcribe a file, separator, Settings, [download], …
        if self.needs_permission != ui.shown_permission.get() {
            ui.shown_permission.set(self.needs_permission);
            let _ = if self.needs_permission { ui.menu.insert(&ui.permission, 1) } else { ui.menu.remove(&ui.permission) };
        }
        if need_download != ui.shown_download.get() {
            ui.shown_download.set(need_download);
            let at = 9 + usize::from(ui.shown_permission.get());
            let _ = if need_download { ui.menu.insert(&ui.download, at) } else { ui.menu.remove(&ui.download) };
        }
        let paste_last = !self.last_text.is_empty();
        if shown.paste_last != Some(paste_last) {
            shown.paste_last = Some(paste_last);
            ui.paste_last.set_enabled(paste_last);
        }
        let transcribe_file = if self.state == State::TranscribingFile {
            let cancelling = self.file_cancel.as_ref().is_none_or(|c| c.load(std::sync::atomic::Ordering::SeqCst));
            (format!("Cancel transcribing {}", self.file_name.replace('&', "&&")), !cancelling)
        } else {
            ("Transcribe a file…".to_string(), self.state == State::Idle && !self.dialog_open)
        };
        if shown.transcribe_file.as_ref() != Some(&transcribe_file) {
            ui.transcribe_file.set_text(&transcribe_file.0);
            ui.transcribe_file.set_enabled(transcribe_file.1);
            shown.transcribe_file = Some(transcribe_file);
        }
        if self.recent_dirty {
            self.recent_dirty = false;
            self.rebuild_recent(&ui.recent);
        }
        if shown.icon != Some(self.state) {
            shown.icon = Some(self.state);
            let (icon, template) = icon_for(self.state);
            #[cfg(target_os = "macos")]
            let _ = if template { ui.tray.set_icon_templated(Some(icon)) } else { ui.tray.set_icon(Some(icon)) };
            #[cfg(not(target_os = "macos"))]
            let _ = (template, ui.tray.set_icon(Some(icon)));
        }
    }
}

/// macOS app menu while Settings is open: Edit gives the text fields ⌘X / ⌘C / ⌘V / ⌘A / ⌘Z.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn edit_menu() -> Menu {
    let menu = Menu::new();
    let app = Submenu::with_items("PoodPim", true, &[&PredefinedMenuItem::close_window(Some("Close Settings"))]).expect("menu");
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
        ],
    )
    .expect("menu");
    let _ = menu.append_items(&[&app, &edit]);
    menu
}

/// A message box that doesn't block the event loop.
/// `next` as typed after the parts already typed: with the space `join_parts` would put there.
fn continuation(typed: &[String], next: &str) -> String {
    let before = typed.iter().rfind(|t| !t.is_empty());
    let space = before.is_some_and(|b| !b.ends_with('\n'))
        && !next.is_empty()
        && !next.starts_with('\n')
        && !crate::asr::starts_with_particle(next);
    if space { format!(" {next}") } else { next.to_string() }
}

/// Turns 16 kHz audio into text (one part of a file).
type Transcriber = Box<dyn FnMut(&[f32]) -> anyhow::Result<String> + Send>;

/// 75 s → "1:15", an hour and more → "1:02:08".
fn clock(secs: f32) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

fn show_alert(message: String) {
    std::thread::spawn(move || platform::alert(&message));
}

/// The model loading again after it was freed when idle; recordings wait for it.
#[derive(Default)]
struct Waking {
    result: Mutex<Option<Result<Arc<Mutex<OpenThaiAsr>>, String>>>,
    done: std::sync::Condvar,
}

impl Waking {
    fn start(self: &Arc<Self>, proxy: EventLoopProxy<UserEvent>, generation: u64) {
        eprintln!("loading the model again (freed when idle)");
        let w = self.clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            let r = std::panic::catch_unwind(|| OpenThaiAsr::load_with(&model::model_dir(), false))
                .map_err(|_| "the model loader crashed".to_string())
                .and_then(|r| r.map(|a| Arc::new(Mutex::new(a))).map_err(|e| format!("{e:#}")));
            eprintln!("model loaded again in {:.1} s", t.elapsed().as_secs_f32());
            *w.result.lock().unwrap() = Some(r.clone());
            w.done.notify_all();
            let _ = proxy.send_event(UserEvent::ModelWoke(generation, r));
        });
    }

    fn wait(&self) -> Result<Arc<Mutex<OpenThaiAsr>>, String> {
        let mut g = self.result.lock().unwrap();
        loop {
            if let Some(r) = &*g {
                return r.clone();
            }
            g = self.done.wait(g).unwrap();
        }
    }
}

/// Draws a microphone glyph. Idle/loading icons are macOS "template" images (adapt to light/dark menu bars).
fn icon_for(state: State) -> (Icon, bool) {
    let (rgb, alpha, template): ([u8; 3], f32, bool) = match state {
        State::Recording => ([0xE5, 0x39, 0x35], 1.0, false),
        State::Transcribing | State::TranscribingFile => ([0xF5, 0x9E, 0x0B], 1.0, false),
        State::Idle => (idle_rgb(), 1.0, true),
        _ => (idle_rgb(), 0.45, true),
    };
    const N: u32 = 36;
    let mut px = vec![0u8; (N * N * 4) as usize];
    for y in 0..N {
        for x in 0..N {
            let (u, v) = ((x as f32 + 0.5) / N as f32, (y as f32 + 0.5) / N as f32);
            let cover = mic_coverage(u, v);
            let i = ((y * N + x) * 4) as usize;
            px[i..i + 3].copy_from_slice(&rgb);
            px[i + 3] = (cover * alpha * 255.0) as u8;
        }
    }
    (Icon::from_rgba(px, N, N).expect("icon"), template)
}

fn idle_rgb() -> [u8; 3] {
    if cfg!(windows) { [0xF0, 0xF0, 0xF0] } else { [0, 0, 0] }
}

fn mic_coverage(u: f32, v: f32) -> f32 {
    let aa = 0.03;
    let fill = |d: f32| (0.5 - d / aa).clamp(0.0, 1.0);
    // capsule (mic head)
    let (cx, top, bot, r) = (0.5, 0.22, 0.46, 0.13);
    let cy = v.clamp(top, bot);
    let head = ((u - cx).powi(2) + (v - cy).powi(2)).sqrt() - r;
    // holder: lower half of a ring
    let ring = if v >= 0.46 { (((u - 0.5).powi(2) + (v - 0.46).powi(2)).sqrt() - 0.25).abs() - 0.035 } else { 1.0 };
    // stem and base
    let stem = if (0.71..=0.86).contains(&v) { (u - 0.5).abs() - 0.035 } else { 1.0 };
    let base = if (0.31..=0.69).contains(&u) { (v - 0.86).abs() - 0.035 } else { 1.0 };
    fill(head.min(ring).min(stem).min(base))
}
