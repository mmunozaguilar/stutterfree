//! Shaderless desktop app: a native egui window over the Rust core. It draws at the monitor's own refresh rate (the
//! WebKitGTK window it replaces was held to a 60 Hz timer), needs no WebKit, and keeps the HTML UI's look and actions:
//! game cards with artwork, launcher / needs-compile filters, search, Record, Compile / Re-Compile with live progress,
//! Clear Cache, Settings and Diagnose.

mod theme;
mod widgets;

use eframe::egui;
use egui::{Align, Color32, Id, Layout, Margin, RichText, Stroke, Vec2};
use stutterfree_core::coordinator::Coordinator;
use stutterfree_core::drivers::{self, DriverBackend};
use stutterfree_core::game_manager::GameManager;
use stutterfree_core::models::{CacheState, CompilationProgress, DiscoveryReport, DriverInfo, GameInfo, GameSource};
use stutterfree_core::monitor::GameExitMonitor;
use stutterfree_core::proton::{self, CancelToken};
use stutterfree_core::settings::{self, AppSettings};
use stutterfree_core::util::format_bytes;
use stutterfree_core::{paths, recorder};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use theme::*;
use widgets::*;

const ICON_PNG: &[u8] = include_bytes!("../../../assets/stutterfree-256.png");
const KOFI_URL: &str = "https://ko-fi.com/maritokura";

// ---- background state ------------------------------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Shared {
    games: Vec<GameInfo>,
    driver: Option<DriverInfo>,
    settings: AppSettings,
    loaded: bool,
    scanning: bool,
    progress: HashMap<String, CompilationProgress>,
    diag: Option<DiscoveryReport>,
    diag_running: bool,
    errors: Vec<String>,
    /// armed games that are running: when they were first seen running and (time, pipelines recorded) samples
    live: HashMap<String, (Instant, Vec<(Instant, i32)>)>,
}

/// What the card shows of a recording in progress (the samples cover the last ~70 s).
fn live_rec(started: Instant, samples: &[(Instant, i32)]) -> Option<LiveRec> {
    let (last, first) = (samples.last()?, samples.first()?);
    Some(LiveRec { secs: last.0.duration_since(started).as_secs(), count: last.1, last_minute: last.1 - first.1 })
}

/// The core side: scans, compiles and recorder changes run on worker threads and publish into [`Shared`].
struct Core {
    manager: GameManager,
    backend: OnceLock<DriverBackend>,
    shared: Mutex<Shared>,
    state_lock: Mutex<()>,
    compile_lock: Mutex<()>,
    cancel: Mutex<CancelToken>,
    ctx: egui::Context,
}

impl Core {
    fn backend(&self) -> DriverBackend {
        *self.backend.get_or_init(drivers::detect_active_backend)
    }

    fn with<R>(&self, f: impl FnOnce(&mut Shared) -> R) -> R {
        let r = f(&mut self.shared.lock().unwrap_or_else(|e| e.into_inner()));
        self.ctx.request_repaint();
        r
    }

    fn read<R>(&self, f: impl FnOnce(&Shared) -> R) -> R {
        f(&self.shared.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn games(&self) -> Vec<GameInfo> {
        self.read(|s| s.games.clone())
    }

    /// Rescans every launcher and publishes the result (`wait`: queue behind a running scan instead of skipping).
    fn scan(&self, wait: bool) {
        let _guard = if wait {
            self.state_lock.lock().unwrap_or_else(|e| e.into_inner())
        } else {
            match self.state_lock.try_lock() {
                Ok(g) => g,
                Err(_) => return,
            }
        };
        self.with(|s| s.scanning = true);
        let backend = self.backend();
        let driver = backend.driver_info();
        let games = self.manager.scan_all_games(Some(&backend));
        let st = settings::load();
        save_last_scan(Some(&driver), &games);
        self.with(|s| {
            s.driver = Some(driver);
            s.games = games;
            s.settings = st;
            s.loaded = true;
            s.scanning = false;
        });
    }

    fn compiling(&self) -> bool {
        self.compile_lock.try_lock().is_err()
    }

    fn compile(&self, game: &GameInfo) {
        let Ok(guard) = self.compile_lock.try_lock() else { return };
        let cancel = CancelToken::new();
        *self.cancel.lock().unwrap() = cancel.clone();
        Coordinator::new(&self.manager, self.backend()).compile(game, &cancel, &mut |p| {
            self.with(|s| {
                s.progress.insert(p.game_id.clone(), p);
            });
        });
        drop(guard);
        self.scan(true);
    }

    fn find(&self, id: &str) -> Option<GameInfo> {
        self.games().into_iter().find(|g| g.id == id)
    }

    /// Shows the bar at once ("Starting...") and compiles on a worker thread.
    fn start_compile(self: &Arc<Self>, id: String) {
        if self.compiling() {
            return;
        }
        let name = self.find(&id).map(|g| g.name).unwrap_or_default();
        self.with(|s| {
            s.progress.insert(
                id.clone(),
                CompilationProgress {
                    game_id: id.clone(),
                    game_name: name,
                    processed_shaders: 0,
                    total_shaders: 0,
                    percentage: 0.0,
                    current_action: "Starting...".into(),
                    is_complete: false,
                    failed: false,
                    error_message: None,
                    failed_shaders: 0,
                    cache_verified: false,
                },
            );
        });
        self.spawn(move |c| {
            if let Some(g) = c.find(&id) {
                c.compile(&g);
            }
        });
    }

    fn spawn(self: &Arc<Self>, f: impl FnOnce(&Arc<Self>) + Send + 'static) {
        let me = self.clone();
        std::thread::spawn(move || f(&me));
    }
}

// ---- the window ------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Launcher {
    All,
    Steam,
    Heroic,
    Lutris,
}

enum Dialog {
    Diagnose,
    Settings,
    ConfirmClearAll,
    ConfirmClearGame { id: String, name: String },
}

struct StutterFreeApp {
    core: Arc<Core>,
    launcher: Launcher,
    needs_only: bool,
    search: String,
    dialog: Option<Dialog>,
    custom_cache: bool,
    custom_cache_gb: i32,
    bench: Option<Bench>,
    _monitor: GameExitMonitor,
}

/// STUTTERFREE_SCROLL_BENCH=1: scrolls the list for 600 frames, prints how often frames reached the screen, then quits.
struct Bench {
    frames: Vec<f32>,
    offset: f32,
    dir: f32,
    started: Option<Instant>,
}

impl StutterFreeApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx);
        let core = Arc::new(Core {
            manager: GameManager::default(),
            backend: OnceLock::new(),
            shared: Mutex::new(Shared { settings: settings::load(), ..Default::default() }),
            state_lock: Mutex::new(()),
            compile_lock: Mutex::new(()),
            cancel: Mutex::new(CancelToken::new()),
            ctx: cc.egui_ctx.clone(),
        });
        // the last scan shows at once; a rescan (on by default) refreshes it in the background
        let cached = load_last_scan();
        let have_cache = cached.is_some();
        if let Some(c) = cached {
            core.with(|s| {
                s.driver = c.driver;
                s.games = c.games;
                s.loaded = true;
            });
        }
        if !have_cache || core.read(|s| s.settings.scan_on_startup) {
            core.spawn(|c| c.scan(true));
        }

        // game exit monitor: import recordings, optional compile after a game closes
        let (a1, a2, a3) = (core.clone(), core.clone(), core.clone());
        let monitor = GameExitMonitor::start(
            move || a1.games(),
            move |g| {
                a2.scan(true);
                let target = a2.find(&g.id).unwrap_or(g);
                a2.compile(&target);
            },
            move |_| {
                if !a3.compiling() {
                    a3.scan(true);
                }
            },
        );

        // recordings in progress: every 5 s, how many pipelines each running armed game has recorded (the recorder writes
        // each one to its shaderless.db as the game creates it)
        let rec = core.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            let running: Vec<(String, i32)> = rec
                .games()
                .iter()
                .filter(|g| g.recorder_installed && proton::is_game_running(g))
                .map(|g| (g.id.clone(), recorder::recorded_psos(g)))
                .collect();
            if running.is_empty() && rec.read(|s| s.live.is_empty()) {
                continue;
            }
            rec.with(|s| {
                let now = Instant::now();
                s.live.retain(|id, _| running.iter().any(|(r, _)| r == id));
                for (id, n) in running {
                    let (_, samples) = s.live.entry(id).or_insert_with(|| (now, Vec::new()));
                    samples.push((now, n));
                    samples.retain(|(at, _)| now.duration_since(*at) <= Duration::from_secs(70));
                }
            });
        });

        // a Proton switch in a launcher refreshes the cards
        let watch = core.clone();
        std::thread::spawn(move || {
            let mut last: Option<String> = None;
            loop {
                std::thread::sleep(Duration::from_secs(2));
                if !watch.read(|s| s.loaded) || watch.compiling() {
                    continue;
                }
                let sig = proton::launcher_proton_config_signature();
                match &last {
                    None => last = Some(sig),
                    Some(l) if *l != sig => {
                        last = Some(sig);
                        watch.scan(true);
                    }
                    _ => {}
                }
            }
        });

        let gb = settings::load().effective_max_cache_size_gb();
        // STUTTERFREE_OPEN_DIALOG=settings|diagnose: opens that dialog at start (UI checks)
        let dialog = match std::env::var("STUTTERFREE_OPEN_DIALOG").as_deref() {
            Ok("settings") => Some(Dialog::Settings),
            Ok("diagnose") => {
                core.spawn(|c| {
                    let r = c.manager.diagnose(Some(&c.backend()));
                    c.with(|s| s.diag = Some(r));
                });
                Some(Dialog::Diagnose)
            }
            _ => None,
        };
        StutterFreeApp {
            core,
            launcher: Launcher::All,
            needs_only: false,
            search: String::new(),
            dialog,
            custom_cache: ![10, 25, 50, 100].contains(&gb),
            custom_cache_gb: gb,
            bench: std::env::var_os("STUTTERFREE_SCROLL_BENCH").map(|_| Bench { frames: Vec::new(), offset: 0.0, dir: 1.0, started: None }),
            _monitor: monitor,
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, s: &Shared) {
        ui.horizontal(|ui| {
            ui.add(egui::Image::from_bytes("bytes://stutterfree-icon.png", ICON_PNG).fit_to_exact_size(Vec2::splat(42.0)).corner_radius(10));
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(bold("StutterFree", 22.0).color(TEXT));
                ui.label(
                    RichText::new(format!("BASED ON SHADERLESS & SCSKILLER · V{}", paths::display_version().to_uppercase()))
                        .size(11.5)
                        .color(MUTED),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| driver_pill(ui, s.driver.as_ref()));
        });
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(10.0, 10.0);
            let rescan = if s.scanning { "🔄 Scanning..." } else { "🔄 Rescan" };
            if button(ui, rescan, Kind::Secondary).clicked() && !s.scanning {
                self.core.spawn(|c| c.scan(false));
            }
            if button(ui, "🩺 Diagnose", Kind::Secondary)
                .on_hover_text("Inspect all checked library paths, discovered games, and skipped manifests")
                .clicked()
            {
                self.dialog = Some(Dialog::Diagnose);
                self.core.with(|s| {
                    s.diag = None;
                    s.diag_running = true;
                });
                self.core.spawn(|c| {
                    let r = c.manager.diagnose(Some(&c.backend()));
                    c.with(|s| {
                        s.diag = Some(r);
                        s.diag_running = false;
                    });
                });
            }
            if button(ui, "⚙ Settings", Kind::Secondary).on_hover_text("Configure automation options (all off by default)").clicked() {
                self.dialog = Some(Dialog::Settings);
            }
            if button(ui, "⚡ Pre-Compile All", Kind::Primary).clicked() {
                self.core.spawn(|c| {
                    for g in c.games().into_iter().filter(|g| g.state != CacheState::Cached) {
                        while c.compiling() {
                            std::thread::sleep(Duration::from_millis(300));
                        }
                        c.start_compile(g.id.clone());
                        std::thread::sleep(Duration::from_millis(300));
                        while c.compiling() {
                            std::thread::sleep(Duration::from_millis(300));
                        }
                    }
                });
            }
            if button(ui, "🗑 Clear All Caches", Kind::Danger).on_hover_text("Purge ALL compiled shaders from GPU driver and Steam game caches").clicked() {
                self.dialog = Some(Dialog::ConfirmClearAll);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if button(ui, "☕ Support on Ko-fi", Kind::Secondary).on_hover_text(KOFI_URL).clicked() {
                    // the desktop's browser; nothing to do if it has none
                    if let Ok(mut c) = std::process::Command::new("xdg-open").arg(KOFI_URL).spawn() {
                        std::thread::spawn(move || c.wait()); // reaped, not left defunct until the app quits
                    }
                }
            });
        });
    }

    fn filter_bar(&mut self, ui: &mut egui::Ui, games: &[GameInfo]) {
        let count = |l: Launcher| games.iter().filter(|g| launcher_matches(l, g)).count();
        let needs = games.iter().filter(|g| needs_compile(g)).count();
        egui::Frame::new().fill(CARD).stroke(Stroke::new(1.0, BORDER)).corner_radius(10).inner_margin(Margin::same(14)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                for (l, name) in [(Launcher::All, "All"), (Launcher::Steam, "Steam"), (Launcher::Heroic, "Heroic"), (Launcher::Lutris, "Lutris")] {
                    if chip(ui, name, count(l), self.launcher == l).clicked() {
                        self.launcher = l;
                    }
                }
                ui.add_space(2.0);
                ui.add(egui::Separator::default().vertical().spacing(10.0));
                if chip(ui, "⚡ Needs Compile", needs, self.needs_only)
                    .on_hover_text("Show only games that need pre-compilation, are stale, or have newly recorded pipelines")
                    .clicked()
                {
                    self.needs_only = !self.needs_only;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if !self.search.is_empty() && ui.add(egui::Button::new(RichText::new("✕").color(MUTED)).frame(false)).on_hover_text("Clear search").clicked() {
                        self.search.clear();
                    }
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .id(Id::unique("search"))
                            .hint_text("🔍  Search game, engine, or ID... (/)")
                            .desired_width(ui.available_width().clamp(160.0, 360.0))
                            .margin(Margin::symmetric(10, 7)),
                    );
                    if ui.input(|i| i.key_pressed(egui::Key::Slash)) && ui.memory(|m| m.focused().is_none()) {
                        r.request_focus();
                    }
                });
            });
        });
    }

    fn visible_games(&self, games: &[GameInfo]) -> Vec<GameInfo> {
        let q = self.search.trim().to_lowercase();
        games
            .iter()
            .filter(|g| launcher_matches(self.launcher, g))
            .filter(|g| !self.needs_only || needs_compile(g))
            .filter(|g| {
                q.is_empty()
                    || g.name.to_lowercase().contains(&q)
                    || g.id.to_lowercase().contains(&q)
                    || g.engine.name().to_lowercase().contains(&q)
                    || g.source.name().to_lowercase().contains(&q)
                    || (g.is_flatpak && "flatpak".contains(q.as_str()))
            })
            .cloned()
            .collect()
    }

    fn games_grid(&mut self, ui: &mut egui::Ui, s: &Shared) {
        ui.horizontal(|ui| {
            ui.label(bold(&format!("INSTALLED GAMES ({})", s.games.len()), 14.0).color(MUTED));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new("SHADERS ARE COMPILED DIRECTLY INTO GPU DRIVER CACHE AHEAD OF TIME").size(11.0).color(MUTED));
            });
        });
        ui.add_space(12.0);
        if !s.loaded {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().color(ACCENT));
                ui.label(RichText::new("Scanning game libraries (Steam, Heroic, Lutris)...").color(MUTED));
            });
            return;
        }
        self.filter_bar(ui, &s.games);
        ui.add_space(16.0);
        if s.games.is_empty() {
            ui.label(RichText::new("No games detected in Steam, Heroic, or Lutris libraries. Click 🩺 Diagnose above to inspect library paths.").color(MUTED));
            return;
        }
        let games = self.visible_games(&s.games);
        if games.is_empty() {
            ui.horizontal(|ui| {
                ui.label(RichText::new("No games match the current filters.").color(MUTED));
                if ui.link("Reset filters").clicked() {
                    self.launcher = Launcher::All;
                    self.needs_only = false;
                    self.search.clear();
                }
            });
            return;
        }
        let gap = 16.0;
        let width = ui.available_width();
        let cols = (((width + gap) / (360.0 + gap)).floor() as usize).max(1);
        let card_w = ((width - gap * (cols as f32 - 1.0)) / cols as f32).floor();
        let compiling = self.core.compiling();
        for row in games.chunks(cols) {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for g in row {
                    ui.allocate_ui_with_layout(Vec2::new(card_w, 0.0), Layout::top_down(Align::Min), |ui| {
                        ui.set_width(card_w);
                        let live = s.live.get(&g.id).and_then(|(started, samples)| live_rec(*started, samples));
                        if let Some(action) = game_card(ui, g, s.progress.get(&g.id), live.as_ref(), compiling, card_w) {
                            self.on_card_action(action, g);
                        }
                    });
                }
            });
            ui.add_space(gap);
        }
    }

    fn on_card_action(&mut self, action: CardAction, g: &GameInfo) {
        match action {
            CardAction::Compile => self.core.start_compile(g.id.clone()),
            CardAction::ClearCache => self.dialog = Some(Dialog::ConfirmClearGame { id: g.id.clone(), name: g.name.clone() }),
            CardAction::ToggleRecorder => {
                let g = g.clone();
                self.core.spawn(move |c| {
                    let r = if g.recorder_installed {
                        recorder::uninstall(&g).map(|_| settings::set_game_auto_record_opt_out(&g.id, true))
                    } else {
                        recorder::install(&g).map(|_| settings::set_game_auto_record_opt_out(&g.id, false))
                    };
                    if let Err(e) = r {
                        c.with(|s| s.errors.push(e.to_string()));
                    }
                    c.scan(true);
                });
            }
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let errors = self.core.read(|s| s.errors.clone());
        if !errors.is_empty() {
            let r = egui::Modal::new(Id::unique("errors")).frame(modal_frame()).show(ctx, |ui| {
                ui.set_max_width(520.0);
                ui.label(bold("⚠ Couldn't do that", 18.0).color(ORANGE));
                ui.add_space(8.0);
                for e in &errors {
                    ui.label(RichText::new(e).color(TEXT));
                }
                ui.add_space(12.0);
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| button(ui, "OK", Kind::Secondary).clicked()).inner
            });
            if r.inner || r.should_close() {
                self.core.with(|s| s.errors.clear());
            }
            return;
        }
        let Some(dialog) = self.dialog.take() else { return };
        let keep = match &dialog {
            Dialog::ConfirmClearAll => self.confirm(ctx, "Purge ALL compiled shaders from GPU driver and Steam game caches?", "This will reset cache for all games to 0 B.", |c| {
                c.spawn(|c| {
                    c.backend().clear_all_caches();
                    c.scan(true);
                })
            }),
            Dialog::ConfirmClearGame { id, name } => {
                let id = id.clone();
                self.confirm(
                    ctx,
                    &format!("Delete on-disk shader cache for \"{name}\"?"),
                    "This will reset the game's shader status to Needs Pre-Compilation (0 B).",
                    move |c| {
                        c.spawn(move |c| {
                            if let Some(g) = c.find(&id) {
                                c.backend().clear_game_cache(&g.id, g.source, g.prefix_path.as_deref());
                            }
                            c.with(|s| {
                                s.progress.remove(&id);
                            });
                            c.scan(true);
                        })
                    },
                )
            }
            Dialog::Settings => self.settings_dialog(ctx),
            Dialog::Diagnose => self.diagnose_dialog(ctx),
        };
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }

    /// Returns whether the dialog stays open.
    fn confirm(&mut self, ctx: &egui::Context, title: &str, detail: &str, on_yes: impl FnOnce(&Arc<Core>)) -> bool {
        let r = egui::Modal::new(Id::unique("confirm")).frame(modal_frame()).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(bold(title, 16.0).color(TEXT));
            ui.add_space(6.0);
            ui.label(RichText::new(detail).color(MUTED));
            ui.add_space(14.0);
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                if button(ui, "🗑 Delete", Kind::Danger).clicked() {
                    return Some(true);
                }
                if button(ui, "Cancel", Kind::Secondary).clicked() {
                    return Some(false);
                }
                None
            })
            .inner
        });
        match r.inner {
            Some(true) => {
                on_yes(&self.core);
                false
            }
            Some(false) => false,
            None => !r.should_close(),
        }
    }

    fn settings_dialog(&mut self, ctx: &egui::Context) -> bool {
        let mut st = self.core.read(|s| s.settings.clone());
        let before = st.clone();
        let r = egui::Modal::new(Id::unique("settings")).frame(modal_frame()).show(ctx, |ui| {
            ui.set_width(600.0);
            let close = modal_title(ui, "⚙ Settings (\"Set and Forget\")");
            setting_row(
                ui,
                "Record all games automatically",
                "(Default: OFF)",
                "Automatically install the pipeline recorder on eligible games when scanning so you don't need to click Record on each one. Games with anti-cheat are always excluded, and you can still toggle Record per game.",
                |ui| {
                    toggle(ui, &mut st.auto_record_all_games);
                },
            );
            setting_row(
                ui,
                "Compile automatically after the game closes",
                "(Default: OFF)",
                "Watch for a running game to close, import newly recorded pipelines from your session, and run a fast incremental compile in the background when no other game is open.",
                |ui| {
                    toggle(ui, &mut st.auto_compile_on_exit);
                },
            );
            setting_row(
                ui,
                "Scan games when the app opens",
                "(Default: ON)",
                "The last scan always shows at once. With this on, StutterFree also rescans your launchers in the background to import new recordings and spot driver, Proton or game updates. Turn it off to scan only when you click Rescan.",
                |ui| {
                    toggle(ui, &mut st.scan_on_startup);
                },
            );
            let (custom, custom_gb) = (&mut self.custom_cache, &mut self.custom_cache_gb);
            setting_row(
                ui,
                "Max shader cache size",
                "(Default: 10 GB)",
                "Raises the GPU driver shader cache cap (NVIDIA & AMD Mesa) so large UE5 games never evict older shaders. On NVIDIA, StutterFree also splits caches into <1.85 GB lock-free multi-part files automatically so caches never truncate at 2 GB.",
                |ui| {
                    ui.vertical(|ui| {
                        let label = if *custom { "Custom...".to_string() } else { format!("{} GB", st.effective_max_cache_size_gb()) };
                        egui::ComboBox::from_id_salt("max-cache").selected_text(label).width(140.0).show_ui(ui, |ui| {
                            for gb in [10, 25, 50, 100] {
                                let text = if gb == 10 { "10 GB  (Default)".to_string() } else { format!("{gb} GB") };
                                if ui.selectable_label(!*custom && st.max_cache_size_gb == gb, text).clicked() {
                                    *custom = false;
                                    st.max_cache_size_gb = gb;
                                }
                            }
                            ui.separator();
                            if ui.selectable_label(*custom, "Custom...  (5–500 GB)").clicked() {
                                *custom = true;
                                *custom_gb = st.effective_max_cache_size_gb();
                            }
                        });
                        if *custom {
                            ui.horizontal(|ui| {
                                ui.add(egui::DragValue::new(custom_gb).range(5..=500).speed(1.0));
                                ui.label(RichText::new("GB").color(MUTED));
                            });
                            st.max_cache_size_gb = *custom_gb;
                        }
                    });
                },
            );
            ui.add_space(4.0);
            ui.label(RichText::new(format!("Saved in {}", settings::default_settings_file_path().display())).size(11.0).color(MUTED));
            close
        });
        if st != before {
            let saved = st.clone();
            self.core.with(|s| s.settings = saved.clone());
            self.core.spawn(move |c| {
                settings::save(&saved);
                proton::refresh_installed_proton_shader_cache_settings(&saved);
                c.scan(true);
            });
        }
        !(r.inner || r.should_close())
    }

    fn diagnose_dialog(&mut self, ctx: &egui::Context) -> bool {
        let report = self.core.read(|s| s.diag.clone());
        let screen = ctx.content_rect();
        let r = egui::Modal::new(Id::unique("diagnose")).frame(modal_frame()).show(ctx, |ui| {
            ui.set_width((screen.width() - 120.0).clamp(500.0, 1000.0));
            let close = modal_title(ui, "🩺 Library Discovery Diagnostics (Native, XDG & Flatpak)");
            match &report {
                None => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().color(ACCENT));
                        ui.label(RichText::new("Inspecting launcher directories and manifests...").color(MUTED));
                    });
                }
                Some(r) => {
                    egui::ScrollArea::vertical().max_height((screen.height() - 220.0).max(240.0)).auto_shrink([false, true]).show(ui, |ui| diag_report(ui, r));
                }
            }
            close
        });
        !(r.inner || r.should_close())
    }

    /// The scroll offset to force this frame in bench mode.
    fn run_bench(&mut self, ui: &egui::Ui) -> Option<f32> {
        let loaded = self.core.read(|s| s.loaded);
        let b = self.bench.as_mut()?;
        ui.ctx().request_repaint();
        if !loaded {
            return None;
        }
        let started = *b.started.get_or_insert_with(Instant::now);
        if started.elapsed() < Duration::from_secs(2) {
            return None;
        }
        b.frames.push(ui.input(|i| i.unstable_dt) * 1000.0);
        b.offset += b.dir * 12.0;
        if b.offset > 1200.0 {
            b.dir = -1.0;
        } else if b.offset <= 0.0 {
            b.dir = 1.0;
        }
        if b.frames.len() >= 600 {
            let mut f = b.frames[10..].to_vec();
            f.sort_by(|a, b| a.total_cmp(b));
            let avg = f.iter().sum::<f32>() / f.len() as f32;
            eprintln!(
                "scroll bench: avg {:.2} ms ({:.0} fps), p95 {:.2} ms, max {:.2} ms",
                avg,
                1000.0 / avg,
                f[(f.len() as f32 * 0.95) as usize],
                f[f.len() - 1]
            );
            std::process::exit(0);
        }
        Some(b.offset)
    }
}

impl eframe::App for StutterFreeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let shared = self.core.read(|s| Shared { diag: None, errors: Vec::new(), ..s.clone() });
        let bench_offset = self.run_bench(ui);
        egui::Panel::top("header")
            .frame(egui::Frame::new().fill(HEADER).inner_margin(Margin { left: 24, right: 24, top: 18, bottom: 16 }).stroke(Stroke::new(1.0, BORDER)))
            .show(ui, |ui| self.header(ui, &shared));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG)).show(ui, |ui| {
            let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
            if let Some(off) = bench_offset {
                area = area.vertical_scroll_offset(off);
            }
            area.show(ui, |ui| {
                egui::Frame::new().inner_margin(Margin::same(24)).show(ui, |ui| self.games_grid(ui, &shared));
            });
        });
        self.dialogs(&ctx);
        if self.core.compiling() || shared.scanning {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.core.cancel.lock().unwrap().cancel();
    }
}

/// The last scan, kept so the app opens with the cards already filled.
#[derive(serde::Serialize, serde::Deserialize)]
struct LastScan {
    driver: Option<DriverInfo>,
    games: Vec<GameInfo>,
}

fn last_scan_path() -> std::path::PathBuf {
    paths::data_dir().join("last-scan.json")
}

fn save_last_scan(driver: Option<&DriverInfo>, games: &[GameInfo]) {
    let snap = LastScan { driver: driver.cloned(), games: games.to_vec() };
    if let Ok(json) = serde_json::to_vec(&snap) {
        let _ = stutterfree_core::util::write_atomic(&last_scan_path(), &json);
    }
}

/// None when there is no saved scan or it can't be read (an older format: the app then scans as before).
fn load_last_scan() -> Option<LastScan> {
    serde_json::from_slice(&std::fs::read(last_scan_path()).ok()?).ok()
}

fn launcher_matches(l: Launcher, g: &GameInfo) -> bool {
    match l {
        Launcher::All => true,
        Launcher::Steam => g.source == GameSource::Steam,
        Launcher::Heroic => g.source == GameSource::Heroic,
        Launcher::Lutris => g.source == GameSource::Lutris,
    }
}

fn needs_compile(g: &GameInfo) -> bool {
    g.state != CacheState::Cached || g.new_recorded_psos > 0
}

fn driver_pill(ui: &mut egui::Ui, d: Option<&DriverInfo>) {
    use egui::text::{LayoutJob, TextFormat};
    use egui::{FontFamily, FontId};
    let mut job = LayoutJob::default();
    let reg = |c: Color32| TextFormat { font_id: FontId::proportional(13.0), color: c, ..Default::default() };
    let strong = |c: Color32| TextFormat { font_id: FontId::new(13.5, FontFamily::Name("bold".into())), color: c, ..Default::default() };
    job.append("● ", 0.0, reg(GREEN_TEXT));
    match d {
        Some(d) => {
            job.append(&d.device_name, 0.0, strong(TEXT));
            job.append(&format!("  •  {} (v{})  •  Total GPU Cache: ", d.driver_name, d.driver_version), 0.0, reg(MUTED));
            job.append(&format_bytes(d.total_cache_size_bytes), 0.0, strong(TEXT));
        }
        None => job.append("Scanning GPU & Driver...", 0.0, reg(MUTED)),
    }
    egui::Frame::new().fill(CARD).stroke(Stroke::new(1.0, BORDER)).corner_radius(20).inner_margin(Margin::symmetric(14, 8)).show(ui, |ui| {
        ui.label(job);
    });
}

fn diag_report(ui: &mut egui::Ui, r: &DiscoveryReport) {
    let kv = |ui: &mut egui::Ui, k: &str, v: &str| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(k).color(MUTED));
            ui.label(RichText::new(v).color(TEXT));
        });
    };
    kv(ui, "Active driver:", &r.active_driver_summary);
    kv(ui, "Default Proton:", r.default_proton_binary.as_deref().unwrap_or("Not found"));
    for src in &r.sources {
        ui.add_space(12.0);
        ui.label(bold(&format!("{} ({} games found)", src.source.name(), src.discovered_games.len()), 15.0).color(ACCENT));
        if let Some(e) = &src.error {
            ui.label(RichText::new(format!("Source error: {e}")).color(RED));
        }
        ui.label(bold("Checked paths", 12.0).color(MUTED));
        for p in &src.checked_paths {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(if p.exists { "FOUND  " } else { "MISSING" }).monospace().size(11.5).color(if p.exists { GREEN_TEXT } else { MUTED }));
                ui.label(RichText::new(&p.description).size(12.5).color(TEXT));
                ui.label(RichText::new(&p.path).monospace().size(11.5).color(MUTED));
            });
        }
        ui.add_space(4.0);
        ui.label(bold("Discovered games", 12.0).color(MUTED));
        if src.discovered_games.is_empty() {
            ui.label(RichText::new("(none discovered)").size(12.5).color(MUTED));
        }
        for g in &src.discovered_games {
            ui.label(bold(&g.name, 13.5).color(TEXT));
            ui.label(
                RichText::new(format!(
                    "ID: {}{} • Engine: {} • Cache: {}",
                    g.id,
                    if g.is_flatpak { " • Flatpak" } else { "" },
                    g.engine,
                    format_bytes(g.cache_size_bytes)
                ))
                .size(12.0)
                .color(MUTED),
            );
            ui.label(RichText::new(format!("Install: {}  |  Exe: {}", g.install_path, g.executable_path)).monospace().size(11.0).color(MUTED));
        }
        if !src.skipped_items.is_empty() {
            ui.add_space(4.0);
            ui.label(bold(&format!("Skipped entries ({})", src.skipped_items.len()), 12.0).color(MUTED));
            for s in &src.skipped_items {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("{}{}:", s.name_or_id, if s.is_flatpak { " [Flatpak]" } else { "" })).size(12.5).color(ORANGE));
                    ui.label(RichText::new(&s.reason).size(12.5).color(TEXT));
                });
                ui.label(RichText::new(&s.path).monospace().size(11.0).color(MUTED));
            }
        }
    }
}

fn main() -> eframe::Result {
    // a command runs the command line (one executable in the AppImage): `stutterfree list`, `stutterfree --cli ...`
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--cli") => stutterfree_cli::run(args[1..].to_vec()),
        Some(a) if stutterfree_cli::is_command(a) => stutterfree_cli::run(args),
        _ => {}
    }
    if std::env::args().any(|a| a == "--version" || a == "-v") {
        println!("StutterFree {}", paths::display_version());
        return Ok(());
    }
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(format!("StutterFree v{} — shader stutter killer for Linux (based on Shaderless / SCSKiller)", paths::display_version()))
        .with_app_id("stutterfree")
        .with_inner_size([1150.0, 750.0])
        .with_min_inner_size([900.0, 600.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    if std::env::var_os("STUTTERFREE_MAXIMIZED").is_some() {
        viewport = viewport.with_maximized(true);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("StutterFree", options, Box::new(|cc| Ok(Box::new(StutterFreeApp::new(cc)))))
}
