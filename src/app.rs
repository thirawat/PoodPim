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
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::asr::OpenThaiAsr;
use crate::audio::{Recorder, resample};
use crate::config::{Backend, Config};
use crate::features::SAMPLE_RATE;
use crate::pipeline::{Outcome, Pipeline};
use crate::overlay::{Overlay, Phase};
use crate::platform::{self, HotkeyEvent, TALK_KEY_LABEL};
use crate::segment::Segmenter;
use crate::corrections::{self, Corrections};
use crate::hotwords::{self, Hotwords};
use crate::{inserter, model, update};

const TAP: Duration = Duration::from_millis(300);
/// Safety cap on one recording; it is finished automatically after this.
const MAX_RECORDING: Duration = Duration::from_secs(10 * 60);
/// How often the recording is checked for a finished segment.
const POLL: Duration = Duration::from_millis(250);
/// Recording goes on this long after the talk key is let go: people let go while saying the
/// last syllable (fast speakers especially), and the sound card still holds a bit of audio.
const TAIL: Duration = Duration::from_millis(200);
/// With "Free memory when idle": the model is freed after this long without dictation.
const UNLOAD_AFTER: Duration = Duration::from_secs(15 * 60);
/// Overlay animation frame interval.
const FRAME: Duration = Duration::from_millis(33);
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
    /// The model freed when idle was loaded again for a recording (see `asleep`).
    ModelWoke(u64, Result<Arc<Mutex<OpenThaiAsr>>, String>),
    Transcribed(u64, Outcome),
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
    Error,
}

struct Ui {
    tray: TrayIcon,
    status: MenuItem,
    local: CheckMenuItem,
    lambda: CheckMenuItem,
    server_settings: MenuItem,
    mixed: CheckMenuItem,
    vocabulary: MenuItem,
    corrections: MenuItem,
    add_correction: MenuItem,
    restore: CheckMenuItem,
    unload: CheckMenuItem,
    autostart: CheckMenuItem,
    paste_last: MenuItem,
    download: MenuItem,
    permission: MenuItem,
    check_update: MenuItem,
    uninstall: MenuItem,
    quit: MenuItem,
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
    /// The talk key was let go: finish the recording at this time (see TAIL).
    stop_at: Option<Instant>,
    handsfree: bool,
    last_text: String,
    status_text: String,
    warm: Option<JoinHandle<()>>,
    segmenter: Option<Segmenter>,
    pipeline: Option<Pipeline>,
    /// Bumped per recording so results of a cancelled one are ignored.
    rec_gen: u64,
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
}

/// Locks a file in the settings folder; None if another copy of the app holds it.
fn single_instance() -> Option<std::fs::File> {
    let dir = model::app_dir(dirs::config_dir());
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("app.lock")).ok()?;
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(std::fs::TryLockError::WouldBlock) => None,
        Err(std::fs::TryLockError::Error(e)) => {
            eprintln!("instance lock: {e}"); // can't tell; start anyway
            Some(file)
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
    log_to_file();
    eprintln!("PoodPim {} ({} {})", update::CURRENT, std::env::consts::OS, std::env::consts::ARCH);
    update::clean_up_after_update();
    // Just updated: the old copy is still quitting, so wait for its lock rather than report it.
    let mut lock = single_instance();
    if std::env::var_os(update::UPDATED_ENV).is_some() {
        for _ in 0..75 {
            if lock.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
            lock = single_instance();
        }
    }
    let Some(instance_lock) = lock else {
        // Opened again while running (often because the menu-bar icon is hidden): say so.
        platform::alert(platform::ALREADY_RUNNING);
        return Ok(());
    };
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
        stop_at: None,
        handsfree: false,
        last_text: String::new(),
        status_text: String::new(),
        warm: None,
        segmenter: None,
        pipeline: None,
        rec_gen: 0,
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
    };
    app.reload_vocabulary();
    let mut ui: Option<Ui> = None;

    event_loop.run(move |event, _, control_flow| {
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
                ui = Some(build_ui(&app.cfg));
                let p = proxy.clone();
                platform::spawn_hotkey_listener(Box::new(move |e| {
                    let _ = p.send_event(UserEvent::Hotkey(e, Instant::now()));
                }));
                if !platform::accessibility_trusted(true) {
                    eprintln!("Accessibility permission is needed: System Settings › Privacy & Security › Accessibility");
                }
                app.apply_autostart();
                Recorder::warm_up();
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
                    } else if id == ui.local.id() {
                        app.switch_backend(Backend::Local);
                    } else if id == ui.lambda.id() {
                        app.switch_backend(Backend::Lambda);
                    } else if id == ui.server_settings.id() {
                        app.open_server_settings();
                    } else if id == ui.vocabulary.id() {
                        app.reload_vocabulary(); // creates the file on first use
                        platform::open_in_editor(&hotwords::user_file());
                    } else if id == ui.add_correction.id() {
                        app.ask_correction();
                    } else if id == ui.corrections.id() {
                        app.reload_vocabulary();
                        platform::open_in_editor(&corrections::user_file());
                    } else if id == ui.mixed.id() {
                        app.cfg.mixed_english = ui.mixed.is_checked();
                        app.cfg.save();
                    } else if id == ui.autostart.id() {
                        app.cfg.start_at_login = ui.autostart.is_checked();
                        app.cfg.save();
                        app.apply_autostart();
                    } else if id == ui.unload.id() {
                        app.cfg.unload_when_idle = ui.unload.is_checked();
                        app.cfg.save();
                        app.last_used = Instant::now();
                    } else if id == ui.restore.id() {
                        app.cfg.restore_clipboard = ui.restore.is_checked();
                        app.cfg.save();
                    } else if id == ui.paste_last.id() {
                        if app.paste(app.last_text.clone()) == Phase::Copied {
                            app.overlay.set(Phase::Copied);
                        }
                    } else if id == ui.download.id() {
                        app.start_download();
                    } else if id == ui.permission.id() {
                        platform::open_permission_settings();
                    } else if id == ui.check_update.id() {
                        app.check_update();
                    } else if id == ui.uninstall.id() {
                        app.ask_uninstall();
                    }
                } else {
                    app.handle(ev);
                }
                if let Some(ui) = &ui {
                    app.refresh(ui);
                }
            }
            _ => {}
        }
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

fn build_ui(cfg: &Config) -> Ui {
    let status = MenuItem::new("Loading…", false, None);
    let local = CheckMenuItem::new("Local (this computer)", true, cfg.backend == Backend::Local, None);
    let lambda = CheckMenuItem::new("Serverless (AWS Lambda)", true, cfg.backend == Backend::Lambda, None);
    let server_settings = MenuItem::new("Server URL && API key…", true, None);
    let mixed = CheckMenuItem::new("Thai + English (mixed)", true, cfg.mixed_english, None);
    let vocabulary = MenuItem::new("My vocabulary…", true, None);
    let corrections = MenuItem::new("Word corrections…", true, None);
    let add_correction = MenuItem::new("Add correction…", true, None);
    let restore = CheckMenuItem::new("Restore clipboard after paste", true, cfg.restore_clipboard, None);
    let unload = CheckMenuItem::new("Free memory when idle (15 min)", true, cfg.unload_when_idle, None);
    let autostart = CheckMenuItem::new("Start at login", true, cfg.start_at_login, None);
    let paste_last = MenuItem::new("Paste last transcript", false, None);
    let download = MenuItem::new("Download model (1 GB)", false, None);
    let hint = MenuItem::new(format!("Hold {TALK_KEY_LABEL} to talk · tap = hands-free · Esc = cancel"), false, None);
    let permission = MenuItem::new("Open Accessibility settings…", false, None);
    let version = MenuItem::new(format!("PoodPim {}", update::CURRENT), false, None);
    let check_update = MenuItem::new("Check for updates…", true, None);
    let uninstall = MenuItem::new("Uninstall PoodPim…", true, None);
    let quit = MenuItem::new("Quit PoodPim", true, None);
    let menu = Menu::new();
    let seps: Vec<PredefinedMenuItem> = (0..5).map(|_| PredefinedMenuItem::separator()).collect();
    menu.append_items(&[
        &status, &permission, &seps[0], &local, &lambda, &server_settings, &seps[1], &mixed, &vocabulary, &corrections, &add_correction, &restore, &unload, &autostart, &paste_last, &download,
        &seps[2], &hint, &seps[3],
    ])
    .expect("menu");
    if update::supported() {
        menu.append_items(&[&version, &check_update, &uninstall, &seps[4]]).expect("menu");
    }
    menu.append(&quit).expect("menu");
    let (icon, template) = icon_for(State::Loading);
    let builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("PoodPim — Thai dictation");
    #[cfg(target_os = "macos")]
    let builder = if template { builder.with_icon_templated(icon) } else { builder.with_icon(icon) };
    #[cfg(not(target_os = "macos"))]
    let builder = {
        let _ = template;
        builder.with_icon(icon)
    };
    let tray = builder.build().expect("tray icon");
    Ui { tray, status, local, lambda, server_settings, mixed, vocabulary, corrections, add_correction, restore, unload, autostart, paste_last, download, permission, check_update, uninstall, quit }
}

impl App {
    fn apply_autostart(&self) {
        if let Err(e) = platform::set_autostart(self.cfg.start_at_login) {
            eprintln!("start at login: {e:#}");
        }
    }

    fn busy(&self) -> bool {
        matches!(self.state, State::Recording | State::Transcribing | State::Downloading)
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
                 ระหว่างดาวน์โหลดดูความคืบหน้าได้ในเมนู 🎙\n\nหรือเลือก Serverless ในเมนูเพื่อถอดเสียงบนเซิร์ฟเวอร์แทน\n\nดาวน์โหลดเลยไหม?",
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
            let r = OpenThaiAsr::load(&dir).map(|a| Arc::new(Mutex::new(a))).map_err(|e| format!("{e:#}"));
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
                        // The int8 model loaded fine: the 2.4 GB full-precision one, left from before
                        // 1.0.2 or from a switch that didn't finish, can go (remove_fp32 only acts
                        // when the int8 model is there).
                        let dir = model::model_dir();
                        self.remove_fp32_after_load = false;
                        if dir.join("encoder.onnx.data").exists() && !model::is_fp32_only(&dir) {
                            model::remove_fp32(&dir);
                            eprintln!("using the int8 model; removed the fp32 one");
                        }
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
                        self.key_rejected = e.contains("401");
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
                    self.key_rejected = e.contains("401");
                    self.status_text = format!(
                        "{} of {} parts failed: {}",
                        out.errors.len(),
                        out.segments,
                        e.chars().take(60).collect::<String>()
                    );
                }
                if !out.text.is_empty() {
                    let text = corrections::space_thai_latin(&self.corrections.apply(&out.text));
                    eprintln!("{:.1}s in {} segment(s): {}", out.audio_s, out.segments, said(&text));
                    self.last_text = text.clone();
                    let phase = self.paste(text);
                    self.overlay.set(phase);
                } else if !out.errors.is_empty() {
                    self.overlay.set(Phase::Failed);
                } else {
                    eprintln!("no speech in {:.1}s of audio", out.audio_s);
                    self.overlay.set(Phase::NoSpeech);
                }
            }
            UserEvent::UpdateFound(tag) => {
                if self.offered_update.as_ref() != Some(&tag) {
                    self.offered_update = Some(tag.clone());
                    self.pending_update = Some(tag);
                }
            }
            UserEvent::UpdateChecked(r) => match r {
                Ok(Some(tag)) => self.pending_update = Some(tag),
                Ok(None) => show_alert(format!("PoodPim {} เป็นเวอร์ชันล่าสุดแล้ว", update::CURRENT)),
                Err(e) => show_alert(format!("เช็คอัปเดตไม่ได้: {e}")),
            },
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
                match update::uninstall(delete_model) {
                    Ok(()) => {
                        // macOS: uninstall.sh finishes once this app has quit. Windows: done.
                        #[cfg(windows)]
                        platform::alert("ลบ PoodPim ออกจากเครื่องแล้ว");
                        self.quit = true;
                    }
                    Err(e) => {
                        platform::alert(&format!("ถอนการติดตั้งไม่สำเร็จทั้งหมด: {e:#}"));
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
            UserEvent::Menu(_) => {}
        }
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
    fn ask_correction(&mut self) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let p = self.proxy.clone();
        std::thread::spawn(move || {
            let answer = platform::ask_text(
                "แก้คำ (1/2): คำที่ PoodPim พิมพ์ออกมาผิด\nเช่น ฟีดแบ็ก หรือ depop\n(ได้ยินผิดหลายแบบ คั่นด้วยจุลภาค ,)",
                "",
            )
            .and_then(|heard| {
                let wanted = platform::ask_text(&format!("แก้คำ (2/2): ให้เขียน \"{heard}\" เป็นอะไร\nเช่น feedback หรือ DevOps"), "")?;
                Some((heard, wanted))
            });
            let _ = p.send_event(UserEvent::CorrectionAnswer(answer));
        });
    }

    /// Reads the word list and the corrections again if their files changed (they are edited
    /// in a text editor). Creates them on first use.
    fn reload_vocabulary(&mut self) {
        let modified = std::fs::metadata(hotwords::user_file()).and_then(|m| m.modified()).ok();
        if modified.is_none() || modified != self.vocabulary_modified {
            let (text, modified) = hotwords::read_user_list();
            self.vocabulary = Arc::new(Hotwords::new(&text));
            self.vocabulary_header = Arc::new(hotwords::to_header(&text));
            self.vocabulary_modified = modified;
        }
        let modified = std::fs::metadata(corrections::user_file()).and_then(|m| m.modified()).ok();
        if modified.is_none() || modified != self.corrections_modified {
            let (text, modified) = corrections::read_user_list();
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
            // Load the int8 model in place of the full-precision one.
            self.model_swap_pending = false;
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
                }
                if self.state == State::Recording && self.stop_at.is_some() {
                    // Pressed again within TAIL of letting go: carry on with the same recording.
                    self.stop_at = None;
                } else if self.state == State::Recording && self.handsfree {
                    self.handsfree = false;
                    self.finish();
                } else if self.state == State::Idle {
                    let started = Instant::now();
                    match self.recorder.start() {
                        Ok(()) => {
                            // From the key press, not from now: opening the microphone can take
                            // a second or more (Windows, Bluetooth headsets), and timing from here
                            // turned short holds into taps (hands-free mode).
                            self.pressed_at = at;
                            eprintln!("microphone started in {} ms", started.elapsed().as_millis());
                            self.state = State::Recording;
                            self.status_text = "Listening…".into();
                            eprintln!("listening");
                            self.overlay.set(Phase::Listening { handsfree: false });
                            self.start_pipeline();
                        }
                        Err(e) => {
                            eprintln!("microphone: {e:#}");
                            self.status_text = format!("Microphone error: {e}");
                            self.overlay.set(Phase::Failed);
                        }
                    }
                }
            }
            HotkeyEvent::TalkUp => {
                if self.state == State::Recording && !self.handsfree {
                    let held = at.saturating_duration_since(self.pressed_at);
                    eprintln!("talk key held {} ms", held.as_millis());
                    if held < TAP {
                        self.handsfree = true;
                        self.status_text = format!("Listening… (tap {TALK_KEY_LABEL} to finish)");
                        self.overlay.set(Phase::Listening { handsfree: true });
                    } else {
                        self.stop_at = Some(at + TAIL);
                    }
                }
            }
            HotkeyEvent::Escape => {
                if self.state == State::Recording {
                    self.stop_at = None;
                    self.recorder.stop_raw();
                    self.segmenter = None;
                    if let Some(p) = self.pipeline.take() {
                        p.cancel();
                    }
                    self.rec_gen += 1;
                    self.handsfree = false;
                    self.state = State::Idle;
                    self.status_text = "Cancelled".into();
                    self.overlay.set(Phase::Cancelled);
                }
            }
            HotkeyEvent::PermissionMissing => {
                eprintln!("Cannot watch the keyboard yet: grant Accessibility permission");
                self.needs_permission = true;
            }
            HotkeyEvent::Ready => {
                eprintln!("watching the {TALK_KEY_LABEL} key");
                self.needs_permission = false;
            }
        }
    }

    /// Sets up the segmenter and the background transcription worker for a new recording.
    fn start_pipeline(&mut self) {
        self.rec_gen += 1;
        self.reload_vocabulary();
        self.segmenter = Some(Segmenter::new(self.recorder.rate()));
        let mixed = self.cfg.mixed_english;
        self.last_used = Instant::now();
        self.pipeline = Some(match self.cfg.backend {
            Backend::Local if self.asr.is_none() => {
                // Freed when idle: load it while the user speaks; segments wait for it. A load
                // already under way (recording cancelled and started again) is shared.
                let waking = match &self.waking {
                    Some(w) => w.clone(),
                    None => {
                        let w = Arc::new(Waking::default());
                        self.waking = Some(w.clone());
                        w.start(self.proxy.clone(), self.load_gen);
                        w
                    }
                };
                let vocabulary = self.vocabulary.clone();
                Pipeline::start(
                    move |a: &[f32]| match waking.wait() {
                        Ok(asr) => asr.lock().unwrap().transcribe(a, mixed, &vocabulary),
                        Err(e) => Err(anyhow::anyhow!("loading the model: {e}")),
                    },
                    None,
                )
            }
            Backend::Local => {
                let asr = self.asr.clone().expect("model loaded in Idle state");
                let vocabulary = self.vocabulary.clone();
                Pipeline::start(move |a: &[f32]| asr.lock().unwrap().transcribe(a, mixed, &vocabulary), None)
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
                Pipeline::start(move |a: &[f32]| server.transcribe(a, mixed, &vocabulary), self.warm.take())
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
            pipe.push(resample(&raw, rate, SAMPLE_RATE));
        }
        let (done, sent) = pipe.progress();
        if sent > 0 {
            let hint = if self.handsfree { format!(" · tap {TALK_KEY_LABEL} to finish") } else { String::new() };
            self.status_text = format!("Listening… {done}/{sent} parts transcribed{hint}");
        }
        if len as f64 / rate as f64 > MAX_RECORDING.as_secs_f64() {
            eprintln!("recording reached {} min: finishing", MAX_RECORDING.as_secs() / 60);
            self.handsfree = false;
            self.finish();
        }
    }

    fn finish(&mut self) {
        self.stop_at = None;
        let raw = self.recorder.stop_raw();
        let rate = self.recorder.rate();
        eprintln!("recorded {:.2}s at {rate} Hz", raw.len() as f64 / rate.max(1) as f64);
        let (Some(seg), Some(mut pipe)) = (self.segmenter.take(), self.pipeline.take()) else {
            self.state = State::Idle;
            self.overlay.set(Phase::Hidden);
            return;
        };
        if seg.start() < raw.len() {
            pipe.push(resample(&raw[seg.start()..], rate, SAMPLE_RATE));
        }
        self.state = State::Transcribing;
        self.overlay.set(Phase::Working);
        self.status_text = match self.cfg.backend {
            Backend::Local => "Transcribing…".into(),
            Backend::Lambda => "Transcribing on server…".into(),
        };
        let (p, generation) = (self.proxy.clone(), self.rec_gen);
        let worker = pipe.finish();
        std::thread::spawn(move || {
            let out = worker.join().unwrap_or_else(|_| Outcome {
                text: String::new(),
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
        if crate::platform::foreground_is_elevated() {
            eprintln!("focused app runs as administrator: copied instead of pasting");
            return match inserter::copy_text(&text) {
                Ok(()) => Phase::Copied,
                Err(e) => {
                    eprintln!("copy error: {e:#}");
                    Phase::Failed
                }
            };
        }
        let restore = self.cfg.restore_clipboard;
        std::thread::spawn(move || {
            if let Err(e) = inserter::paste_text(&text, restore) {
                eprintln!("paste error: {e:#}");
            }
        });
        Phase::Done
    }

    fn refresh(&mut self, ui: &Ui) {
        self.offer_pending_update();
        if self.warm.as_ref().is_some_and(|w| w.is_finished()) && self.state != State::Recording {
            self.warm = None;
        }
        if self.needs_permission {
            ui.status.set_text("⚠ Allow PoodPim in Accessibility to use the talk key");
        } else {
            ui.status.set_text(&self.status_text);
        }
        ui.permission.set_enabled(self.needs_permission);
        ui.local.set_checked(self.cfg.backend == Backend::Local);
        ui.lambda.set_checked(self.cfg.backend == Backend::Lambda);
        ui.paste_last.set_enabled(!self.last_text.is_empty());
        ui.download.set_enabled(
            self.cfg.backend == Backend::Local
                && matches!(self.state, State::NeedsModel | State::Error)
                && !model::is_ready(&model::model_dir()),
        );
        let (icon, template) = icon_for(self.state);
        #[cfg(target_os = "macos")]
        let _ = if template { ui.tray.set_icon_templated(Some(icon)) } else { ui.tray.set_icon(Some(icon)) };
        #[cfg(not(target_os = "macos"))]
        let _ = (template, ui.tray.set_icon(Some(icon)));
    }
}

/// A message box that doesn't block the event loop.
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
        State::Transcribing => ([0xF5, 0x9E, 0x0B], 1.0, false),
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
