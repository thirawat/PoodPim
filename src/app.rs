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
use crate::{inserter, model};

const TAP: Duration = Duration::from_millis(300);
/// Safety cap on one recording; it is finished automatically after this.
const MAX_RECORDING: Duration = Duration::from_secs(10 * 60);
/// How often the recording is checked for a finished segment.
const POLL: Duration = Duration::from_millis(250);
/// Overlay animation frame interval.
const FRAME: Duration = Duration::from_millis(33);
/// Skip the wake-up call if the server answered this recently (Lambda keeps instances ~5–15 min).
const ASSUME_WARM: Duration = Duration::from_secs(180);

enum UserEvent {
    Hotkey(HotkeyEvent),
    Menu(MenuEvent),
    ModelLoaded(u64, Result<Arc<Mutex<OpenThaiAsr>>, String>),
    Transcribed(u64, Outcome),
    Warmed(Result<u64, String>),
    ServerSettings(Option<(String, String)>),
    ApiKey(Option<String>),
    DownloadAnswer(bool),
    Download(u64, u64),
    DownloadDone(Result<(), String>),
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
    restore: CheckMenuItem,
    autostart: CheckMenuItem,
    paste_last: MenuItem,
    download: MenuItem,
    permission: MenuItem,
    quit: MenuItem,
}

struct App {
    cfg: Config,
    state: State,
    asr: Option<Arc<Mutex<OpenThaiAsr>>>,
    load_gen: u64,
    recorder: Recorder,
    pressed_at: Instant,
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
}

pub fn run() -> Result<()> {
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
        load_gen: 0,
        recorder: Recorder::new(),
        pressed_at: Instant::now(),
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
    };
    let mut ui: Option<Ui> = None;

    event_loop.run(move |event, _, control_flow| {
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
        let animating = app.overlay.tick();
        match event {
            Event::NewEvents(StartCause::Init) => {
                ui = Some(build_ui(&app.cfg));
                let p = proxy.clone();
                platform::spawn_hotkey_listener(Box::new(move |e| {
                    let _ = p.send_event(UserEvent::Hotkey(e));
                }));
                if !platform::accessibility_trusted(true) {
                    eprintln!("Accessibility permission is needed: System Settings › Privacy & Security › Accessibility");
                }
                app.apply_autostart();
                app.start_backend();
                // First launch in Local mode: ask about the model now, not on the first key press.
                if app.state == State::NeedsModel {
                    app.offer_download();
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
                    } else if id == ui.mixed.id() {
                        app.cfg.mixed_english = ui.mixed.is_checked();
                        app.cfg.save();
                    } else if id == ui.autostart.id() {
                        app.cfg.start_at_login = ui.autostart.is_checked();
                        app.cfg.save();
                        app.apply_autostart();
                    } else if id == ui.restore.id() {
                        app.cfg.restore_clipboard = ui.restore.is_checked();
                        app.cfg.save();
                    } else if id == ui.paste_last.id() {
                        app.paste(app.last_text.clone());
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
            _ => {}
        }
        // While recording, wake up regularly to cut finished segments off the recording.
        *control_flow = if animating {
            ControlFlow::WaitUntil(Instant::now() + FRAME)
        } else if app.state == State::Recording {
            ControlFlow::WaitUntil(Instant::now() + POLL)
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
    let restore = CheckMenuItem::new("Restore clipboard after paste", true, cfg.restore_clipboard, None);
    let autostart = CheckMenuItem::new("Start at login", true, cfg.start_at_login, None);
    let paste_last = MenuItem::new("Paste last transcript", false, None);
    let download = MenuItem::new("Download model (2.6 GB)", false, None);
    let hint = MenuItem::new(format!("Hold {TALK_KEY_LABEL} to talk · tap = hands-free · Esc = cancel"), false, None);
    let permission = MenuItem::new("Open Accessibility settings…", false, None);
    let quit = MenuItem::new("Quit PoodPim", true, None);
    let menu = Menu::new();
    let seps: Vec<PredefinedMenuItem> = (0..4).map(|_| PredefinedMenuItem::separator()).collect();
    menu.append_items(&[
        &status, &permission, &seps[0], &local, &lambda, &server_settings, &seps[1], &mixed, &restore, &autostart, &paste_last, &download,
        &seps[2], &hint, &seps[3], &quit,
    ])
    .expect("menu");
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
    Ui { tray, status, local, lambda, server_settings, mixed, restore, autostart, paste_last, download, permission, quit }
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
                "ยังไม่มีโมเดลถอดเสียงในเครื่องนี้\n\nโหมด Local ต้องดาวน์โหลดโมเดลก่อน ขนาด 2.6 GB (ครั้งเดียว) \
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
            UserEvent::Hotkey(h) => self.on_hotkey(h),
            UserEvent::ModelLoaded(generation, r) => {
                if generation != self.load_gen || self.cfg.backend != Backend::Local {
                    return; // the user switched backend while it was loading
                }
                match r {
                    Ok(asr) => {
                        self.asr = Some(asr);
                        self.state = State::Idle;
                        self.status_text = self.ready_text();
                    }
                    Err(e) => self.fail(&e),
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
                    eprintln!("{:.1}s in {} segment(s): {}", out.audio_s, out.segments, out.text);
                    self.last_text = out.text.clone();
                    self.paste(out.text);
                    self.overlay.set(Phase::Done);
                } else if !out.errors.is_empty() {
                    self.overlay.set(Phase::Failed);
                } else {
                    eprintln!("no speech in {:.1}s of audio", out.audio_s);
                    self.overlay.set(Phase::NoSpeech);
                }
            }
            UserEvent::Menu(_) => {}
        }
    }

    fn fail(&mut self, e: &str) {
        eprintln!("error: {e}");
        self.state = State::Error;
        self.status_text = format!("Error: {}", e.chars().take(80).collect::<String>());
    }

    fn on_hotkey(&mut self, h: HotkeyEvent) {
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
                if self.state == State::Recording && self.handsfree {
                    self.handsfree = false;
                    self.finish();
                } else if self.state == State::Idle {
                    match self.recorder.start() {
                        Ok(()) => {
                            self.pressed_at = Instant::now();
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
                    if self.pressed_at.elapsed() < TAP {
                        self.handsfree = true;
                        self.status_text = format!("Listening… (tap {TALK_KEY_LABEL} to finish)");
                        self.overlay.set(Phase::Listening { handsfree: true });
                    } else {
                        self.finish();
                    }
                }
            }
            HotkeyEvent::Escape => {
                if self.state == State::Recording {
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
        self.segmenter = Some(Segmenter::new(self.recorder.rate()));
        let mixed = self.cfg.mixed_english;
        self.pipeline = Some(match self.cfg.backend {
            Backend::Local => {
                let asr = self.asr.clone().expect("model loaded in Idle state");
                Pipeline::start(move |a: &[f32]| asr.lock().unwrap().transcribe(a, mixed), None)
            }
            Backend::Lambda => {
                let stale = self.last_server_ok.is_none_or(|t| t.elapsed() > ASSUME_WARM);
                if stale && self.warm.is_none() {
                    self.spawn_warm(false); // the cold start overlaps with speaking
                }
                // Segments wait for the wake-up call: one Lambda instance serves one request at a
                // time, so sending earlier would start a second, cold instance.
                let server = self.cfg.server();
                Pipeline::start(move |a: &[f32]| server.transcribe(a, mixed), self.warm.take())
            }
        });
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
        let raw = self.recorder.stop_raw();
        let rate = self.recorder.rate();
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

    fn paste(&self, text: String) {
        if text.is_empty() {
            return;
        }
        let restore = self.cfg.restore_clipboard;
        std::thread::spawn(move || {
            if let Err(e) = inserter::paste_text(&text, restore) {
                eprintln!("paste error: {e:#}");
            }
        });
    }

    fn refresh(&mut self, ui: &Ui) {
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
