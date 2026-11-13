//! StutterFree command line (SCSKiller.Linux.Cli/Program.cs): `run` is the stutterfree-cli binary, and the app's own
//! binary when it's started with a command (one executable in the AppImage).

use stutterfree_core::coordinator::Coordinator;
use stutterfree_core::drivers::{self, DriverBackend};
use stutterfree_core::game_manager::{filter_games, GameManager};
use stutterfree_core::models::{CacheState, CompilationProgress, GameInfo, GameSource};
use stutterfree_core::proton::CancelToken;
use stutterfree_core::util::{format_bytes, n0};
use stutterfree_core::{paths, proton, recorder, settings};
use std::io::{self, BufRead, IsTerminal, Write};

struct Color;
impl Color {
    fn on() -> bool {
        io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
    }
    fn paint(code: &str, s: &str) -> String {
        if Self::on() {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}
fn cyan(s: &str) -> String {
    Color::paint("36", s)
}
fn green(s: &str) -> String {
    Color::paint("32", s)
}
fn yellow(s: &str) -> String {
    Color::paint("33", s)
}
fn red(s: &str) -> String {
    Color::paint("31", s)
}
fn gray(s: &str) -> String {
    Color::paint("90", s)
}

static CANCEL: std::sync::OnceLock<CancelToken> = std::sync::OnceLock::new();

extern "C" fn on_sigint(_: libc::c_int) {
    if let Some(c) = CANCEL.get() {
        c.cancel();
    }
}

/// The first arguments that make the app's binary run as the command line instead of opening its window.
pub fn is_command(arg: &str) -> bool {
    matches!(
        arg,
        "status" | "driver" | "list" | "scan" | "games" | "diagnose" | "diag" | "settings" | "record" | "compile" | "compile-all" | "plan" | "clear"
            | "clear-game" | "clear-all" | "clear-cache" | "help" | "--help" | "-h"
    )
}

/// Runs a command (the arguments after the program name) and exits with its status.
pub fn run(args: Vec<String>) -> ! {
    if matches!(args.first().map(String::as_str), Some("--version" | "-v" | "version")) {
        println!("StutterFree {}", paths::display_version());
        std::process::exit(0);
    }
    CANCEL.get_or_init(CancelToken::new);
    unsafe { libc::signal(libc::SIGINT, on_sigint as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    print_header();
    let cmd = args.first().map(|s| s.to_lowercase()).unwrap_or_else(|| "help".into());
    let code = match cmd.as_str() {
        "status" | "driver" => status(&drivers::detect_active_backend()),
        "list" | "scan" | "games" => list(&args[1..]),
        "diagnose" | "diag" => diagnose(),
        "settings" => settings_cmd(&args),
        "compile" => match args.get(1) {
            Some(q) => compile(&GameManager::default(), &drivers::detect_active_backend(), q),
            None => usage_err("Usage: stutterfree-cli compile <game-id-or-name>"),
        },
        "compile-all" => compile_all(),
        "record" => record(&args[1..]),
        "plan" => match args.get(1) {
            Some(q) => plan(q),
            None => usage_err("Usage: stutterfree-cli plan <game-id-or-name>"),
        },
        "clear" | "clear-game" => match args.get(1) {
            Some(q) => clear_game(&GameManager::default(), &drivers::detect_active_backend(), q),
            None => usage_err("Usage: stutterfree-cli clear <game-id-or-name>\nExample: stutterfree-cli clear cyberpunk"),
        },
        "clear-all" => clear_all(&drivers::detect_active_backend()),
        "clear-cache" => match args.get(1).map(String::as_str) {
            Some("--all" | "-a" | "all") => clear_all(&drivers::detect_active_backend()),
            Some(q) => clear_game(&GameManager::default(), &drivers::detect_active_backend(), q),
            None => clear_interactive(),
        },
        _ => {
            usage();
            0
        }
    };
    std::process::exit(code);
}

fn usage_err(msg: &str) -> i32 {
    println!("{}", yellow(msg));
    1
}

fn print_header() {
    println!(
        "{}",
        cyan(
            "
╔═══════════════════════════════════════════════════════════════╗
║      StutterFree — Shader Compilation Stutter Killer          ║
║        Kills shader stutter before you play (AMD/NVIDIA)      ║
╚═══════════════════════════════════════════════════════════════╝"
        )
    );
    println!("{}", cyan(&format!("  v{} (based on Shaderless, the unofficial Linux port of SCSKiller)", paths::display_version())));
}

fn usage() {
    println!("Available commands:");
    println!("  stutterfree-cli status              Show GPU driver and shader cache stats");
    println!("  stutterfree-cli list                List detected games across Steam, Heroic & Lutris (Native + Flatpak)");
    println!("  stutterfree-cli diagnose            Inspect all checked library paths, discovered games, and skipped entries");
    println!("  stutterfree-cli settings            Show or update automation & max cache size settings");
    println!("  stutterfree-cli record <id|name> on|off   Install or remove the DX12 recorder for a game");
    println!("  stutterfree-cli plan <id|name>      Read the game's shaders from its files without compiling (dry run)");
    println!("  stutterfree-cli compile <id|name>   Pre-compile shaders for a specific game");
    println!("  stutterfree-cli compile-all         Pre-compile shaders for all uncompiled games");
    println!("  stutterfree-cli clear <id|name>     Purge shader cache for a specific game (e.g. cyberpunk)");
    println!("  stutterfree-cli clear-all           Purge ALL GPU driver and game shader caches");
    println!("  stutterfree-cli clear-cache         Interactive cache clearing menu");
    println!();
}

fn on_off(v: &str) -> bool {
    matches!(v, "on" | "true" | "1")
}

fn settings_cmd(args: &[String]) -> i32 {
    let mut s = settings::load();
    let mut modified = false;
    let mut i = 1;
    while i < args.len() {
        let a = args[i].to_lowercase();
        let next = args.get(i + 1);
        match (a.as_str(), next) {
            ("--auto-record", Some(v)) => {
                s.auto_record_all_games = on_off(v);
                modified = true;
                i += 1;
            }
            ("--auto-compile", Some(v)) => {
                s.auto_compile_on_exit = on_off(v);
                modified = true;
                i += 1;
            }
            ("--scan-on-startup", Some(v)) => {
                s.scan_on_startup = on_off(v);
                modified = true;
                i += 1;
            }
            ("--max-cache-gb" | "--max-cache", Some(v)) => {
                if let Ok(gb) = v.trim_end_matches(['g', 'G', 'b', 'B']).parse::<i32>() {
                    s.max_cache_size_gb = settings::clamp_max_cache_size_gb(gb);
                    modified = true;
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    if modified {
        settings::save(&s);
        proton::refresh_installed_proton_shader_cache_settings(&s);
        println!("{}\n", green("Updated settings saved."));
    }
    println!("Settings file: {}", settings::default_settings_file_path().display());
    println!("  Auto-Record All Games:          {}", if s.auto_record_all_games { "ON" } else { "OFF" });
    println!("  Auto-Compile After Game Closes: {}", if s.auto_compile_on_exit { "ON" } else { "OFF" });
    println!("  Scan Games When the App Opens:  {}", if s.scan_on_startup { "ON" } else { "OFF" });
    println!("  Max Shader Cache Size:          {} GB (5-500 GB)", s.effective_max_cache_size_gb());
    println!("\nUsage: stutterfree-cli settings [--auto-record on|off] [--auto-compile on|off] [--scan-on-startup on|off] [--max-cache-gb <5..500>]\n");
    0
}

fn status(b: &DriverBackend) -> i32 {
    println!("Detecting GPU and driver caching configuration...\n");
    let info = b.driver_info();
    println!("GPU Device:           {}", info.device_name);
    println!("Driver Stack:         {} (v{})", info.driver_name, info.driver_version);
    println!("Vendor:               {}", info.vendor);
    println!("Global Cache Path:    {}", info.shader_cache_path);
    println!("Global Cached Files:  {} entries ({})", n0(info.cached_files_count as i64), format_bytes(info.cache_size_bytes));
    if info.steam_shader_cache_size_bytes > 0 {
        println!("Steam Shader Caches:  {} entries ({})", n0(info.steam_shader_cache_files_count as i64), format_bytes(info.steam_shader_cache_size_bytes));
    }
    println!("{}", green(&format!("Total Cache on Disk:  {}", format_bytes(info.total_cache_size_bytes))));
    println!("Max Cache Cap:        {} GB (configurable via 'stutterfree-cli settings --max-cache-gb <GB>')", settings::load().effective_max_cache_size_gb());
    println!("Status:               {} {}", info.status, info.status_message.map(|m| format!("({m})")).unwrap_or_default());
    match drivers::find_fossilize_replay() {
        Some(p) => println!("Pipeline Replayer:    Found ({})", p.display()),
        None => println!("Pipeline Replayer:    Not found in system"),
    }
    println!("Warmer:               {}", proton::find_warmer_binary().map(|p| p.display().to_string()).unwrap_or_else(|| "NOT FOUND (shaderless_warm.exe)".into()));
    println!("Recorder:             {}", paths::find_native("d3d12.dll").map(|p| p.display().to_string()).unwrap_or_else(|| "NOT FOUND (d3d12.dll)".into()));
    println!("\nRecommended Environment Variables:");
    println!("{}", gray(&b.recommended_environment_variables()));
    0
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max - 3).collect::<String>())
    }
}

fn list(filter: &[String]) -> i32 {
    let mut source: Option<GameSource> = None;
    let mut needs = false;
    let mut search: Vec<String> = Vec::new();
    let mut i = 0;
    while i < filter.len() {
        let a = &filter[i];
        if (a.eq_ignore_ascii_case("--source") || a.eq_ignore_ascii_case("-s")) && i + 1 < filter.len() {
            i += 1;
            source = GameSource::parse(&filter[i]);
        } else if a.eq_ignore_ascii_case("--needs-compile") || a.eq_ignore_ascii_case("--uncompiled") {
            needs = true;
        } else if let (Some(s), true) = (GameSource::parse(a), search.is_empty() && source.is_none()) {
            source = Some(s);
        } else {
            search.push(a.clone());
        }
        i += 1;
    }
    println!("Scanning game libraries (Steam, Heroic, Lutris — Native & Flatpak)...");
    let backend = drivers::detect_active_backend();
    let all = GameManager::default().scan_all_games(Some(&backend));
    if all.is_empty() {
        println!("No games discovered in Steam, Heroic, or Lutris libraries.");
        println!("Tip: Run 'stutterfree-cli diagnose' to see all checked paths and why entries were skipped.");
        return 0;
    }
    let q = (!search.is_empty()).then(|| search.join(" "));
    let games = filter_games(&all, source, q.as_deref(), needs);
    if games.is_empty() {
        println!("\n0 of {} installed games matched your filter.", all.len());
        println!("Usage: stutterfree-cli list [--source steam|heroic|lutris] [--needs-compile] [<search>]\n");
        return 0;
    }
    if games.len() == all.len() {
        println!("\nFound {} installed games:\n", games.len());
    } else {
        println!("\nShowing {} of {} installed games:\n", games.len(), all.len());
    }
    println!("{:<15} {:<28} {:<14} {:<22} {:<10} {:<10} {:<12}", "Source", "Name", "Engine", "Status", "Recorded", "Shaders", "Disk Cache");
    println!("{}", "-".repeat(118));
    for g in &games {
        let src = if g.is_flatpak { format!("{} (FP)", g.source) } else { g.source.to_string() };
        let st = if g.state == CacheState::Outdated {
            g.stale_reason.as_ref().map(|r| format!("Stale ({r})")).unwrap_or_else(|| "Stale".into())
        } else {
            g.state.name().to_string()
        };
        let rec = if g.recorded_psos > 0 { n0(g.recorded_psos as i64) } else { "-".into() };
        let line = format!(
            "{:<15} {:<28} {:<14} {:<22} {:<10} {:<10} {:<12}",
            src,
            truncate(&g.name, 26),
            g.engine.name(),
            truncate(&st, 21),
            rec,
            format!("~{}", n0(g.total_shaders_estimated as i64)),
            format_bytes(g.cache_size_bytes)
        );
        println!("{}", if g.state == CacheState::Cached { green(&line) } else { yellow(&line) });
    }
    println!("\nFilter tip: 'stutterfree-cli list steam', 'stutterfree-cli list --needs-compile', or 'stutterfree-cli list <name>'.");
    println!("Run 'stutterfree-cli compile <id|name>' to pre-compile shaders ahead of time.\n");
    0
}

fn diagnose() -> i32 {
    println!("Running full library discovery diagnostics (Native, XDG, and Flatpak)...\n");
    let r = GameManager::default().diagnose(None);
    println!("Driver:         {}", r.active_driver_summary);
    println!("Default Proton: {}", r.default_proton_binary.as_deref().unwrap_or("Not found"));
    println!("XDG_DATA_HOME:  {}", paths::xdg_data_home().display());
    println!("XDG_CONFIG_HOME:{}", paths::xdg_config_home().display());
    println!("XDG_CACHE_HOME: {}", paths::xdg_cache_home().display());
    for src in &r.sources {
        println!("\n{}", cyan(&format!("=== {} ===", src.source.name().to_uppercase())));
        if let Some(e) = &src.error {
            println!("{}", red(&format!("  Source Error: {e}")));
        }
        println!("  Checked Paths ({}):", src.checked_paths.len());
        for p in &src.checked_paths {
            let l = format!("    {} {}: {}", if p.exists { "[FOUND]  " } else { "[MISSING]" }, p.description, p.path);
            println!("{}", if p.exists { green(&l) } else { gray(&l) });
        }
        println!("  Discovered Games ({}):", src.discovered_games.len());
        if src.discovered_games.is_empty() {
            println!("{}", gray("    (none)"));
        }
        for g in &src.discovered_games {
            println!("{}", green(&format!("    + {} [ID: {}, Source: {}, Engine: {}]", g.name, g.id, g.source_label, g.engine)));
            println!("      Install: {}", g.install_path);
            println!("      Exe:     {}", g.executable_path);
            println!("      Prefix:  {}", g.prefix_path.as_deref().unwrap_or("(default/shared)"));
            println!("      Cache:   {} ({})", format_bytes(g.cache_size_bytes), g.state.name());
        }
        println!("  Skipped Entries ({}):", src.skipped_items.len());
        if src.skipped_items.is_empty() {
            println!("{}", gray("    (none)"));
        }
        for s in &src.skipped_items {
            println!("{}", yellow(&format!("    - {}{}: {}", s.name_or_id, if s.is_flatpak { " [Flatpak]" } else { "" }, s.reason)));
            println!("{}", gray(&format!("      Path: {}", s.path)));
        }
    }
    println!();
    0
}

fn match_game<'a>(games: &'a [GameInfo], q: &str) -> Option<&'a GameInfo> {
    let ql = q.to_lowercase();
    games
        .iter()
        .find(|g| g.id.eq_ignore_ascii_case(q))
        .or_else(|| games.iter().find(|g| g.raw_id().eq_ignore_ascii_case(q)))
        .or_else(|| games.iter().find(|g| g.name.to_lowercase().contains(&ql)))
}

fn render_progress(p: &CompilationProgress) {
    let width = 30usize;
    let filled = ((p.percentage / 100.0) * width as f64).round().clamp(0.0, width as f64) as usize;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(width - filled));
    print!("\r\x1b[2K[{bar}] {:>5.1}% ({}/{}) — {}", p.percentage, p.processed_shaders, p.total_shaders, truncate(&p.current_action, 70));
    let _ = io::stdout().flush();
}

fn compile(m: &GameManager, b: &DriverBackend, q: &str) -> i32 {
    let games = m.scan_all_games(Some(b));
    let Some(game) = match_game(&games, q) else {
        println!("{}", red(&format!("Game matching '{q}' was not found in installed libraries.")));
        return 1;
    };
    println!("Preparing pre-compilation for: {} ({}, ID: {})", game.name, game.source_label, game.id);
    println!("Engine: {} | Estimated Shaders: {}", game.engine, n0(game.total_shaders_estimated as i64));
    println!("Game Directory: {}\n", game.install_path);
    let mut last: Option<CompilationProgress> = None;
    Coordinator::new(m, *b).compile(game, CANCEL.get().unwrap(), &mut |p| {
        render_progress(&p);
        last = Some(p);
    });
    let Some(last) = last else { return 1 };
    if last.failed {
        println!("\n\n{}\n", red(&format!("ERROR: {}", last.error_message.as_deref().unwrap_or(&last.current_action))));
        return 1;
    }
    if !last.cache_verified {
        println!("\n\n{}\n", yellow(&format!("WARNING: {}", last.current_action)));
        return 0;
    }
    println!("\n\n{}", green(&last.current_action));
    println!("{}\n", green("Shader cache is installed and ready."));
    0
}

fn compile_all() -> i32 {
    let m = GameManager::default();
    let b = drivers::detect_active_backend();
    let todo: Vec<GameInfo> = m.scan_all_games(Some(&b)).into_iter().filter(|g| g.state != CacheState::Cached).collect();
    if todo.is_empty() {
        println!("{}", green("All detected games are already cached and up to date!"));
        return 0;
    }
    println!("Starting batch compilation for {} games...\n", todo.len());
    for g in todo {
        if CANCEL.get().unwrap().is_cancelled() {
            break;
        }
        compile(&m, &b, &g.id);
    }
    0
}

fn record(args: &[String]) -> i32 {
    let (Some(q), Some(state)) = (args.first(), args.get(1)) else {
        return usage_err("Usage: stutterfree-cli record <game-id-or-name> on|off");
    };
    let b = drivers::detect_active_backend();
    let games = GameManager::default().scan_all_games(Some(&b));
    let Some(g) = match_game(&games, q) else {
        println!("{}", red(&format!("Game matching '{q}' was not found in installed libraries.")));
        return 1;
    };
    let on = on_off(&state.to_lowercase());
    let r = if on { recorder::install(g) } else { recorder::uninstall(g) };
    match r {
        Ok(()) => {
            settings::set_game_auto_record_opt_out(&g.id, !on);
            if on {
                println!("{}", green(&format!("Recorder installed for {}. Play 5-10 minutes, close the game, then run: stutterfree-cli compile {}", g.name, g.id)));
            } else {
                println!("{}", green(&format!("Recorder removed from {} (what it recorded was kept).", g.name)));
            }
            0
        }
        Err(e) => {
            println!("{}", red(&format!("ERROR: {e}")));
            1
        }
    }
}

fn plan(q: &str) -> i32 {
    let games = GameManager::default().scan_all_games(Some(&drivers::detect_active_backend()));
    let Some(g) = match_game(&games, q) else {
        println!("{}", red(&format!("Game matching '{q}' was not found in installed libraries.")));
        return 1;
    };
    println!("Reading the shaders of {} ({}) from its files...", g.name, g.engine);
    let dir = paths::data_dir().join("work").join(format!("plan-{}", g.id));
    let _ = std::fs::create_dir_all(&dir);
    let t = std::time::Instant::now();
    let r = stutterfree_core::engines::plan_from_game_files(g, &dir, None, CANCEL.get().unwrap());
    let _ = std::fs::remove_dir_all(&dir);
    match r {
        Ok(o) => {
            println!("{}", green(&format!("{} ({:.1} s)", o.summary, t.elapsed().as_secs_f32())));
            0
        }
        Err(e) => {
            println!("{}", yellow(&format!("Not plannable from the game files: {e}")));
            1
        }
    }
}

fn confirm(prompt: &str) -> bool {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().lock().read_line(&mut line);
    line.trim().eq_ignore_ascii_case("y")
}

fn clear_game(m: &GameManager, b: &DriverBackend, q: &str) -> i32 {
    let games = m.scan_all_games(Some(b));
    let Some(g) = match_game(&games, q) else {
        println!("{}", red(&format!("Game matching '{q}' was not found in installed libraries.")));
        return 1;
    };
    let (bytes, files, path) = b.game_cache_stats(&g.id, g.source, g.prefix_path.as_deref());
    println!("Target game:         {} ({}, ID: {})", g.name, g.source_label, g.id);
    println!("Cache location:      {}", if path.is_empty() { "N/A" } else { &path });
    println!("Current cache size:  {} ({} files)", format_bytes(bytes), n0(files as i64));
    if bytes == 0 {
        println!("{}", yellow("Shader cache is already 0 B (empty). Nothing to clean."));
        return 0;
    }
    if !confirm(&format!("\nAre you sure you want to delete shader cache for '{}'? (y/N): ", g.name)) {
        println!("Operation cancelled.");
        return 0;
    }
    println!("Deleting shader cache...");
    b.clear_game_cache(&g.id, g.source, g.prefix_path.as_deref());
    let (after, _, _) = b.game_cache_stats(&g.id, g.source, g.prefix_path.as_deref());
    println!("{}", green(&format!("\nSUCCESS: Shader cache for '{}' has been purged from disk!", g.name)));
    println!("{}", green(&format!("Space freed: {}", format_bytes(bytes))));
    println!("{}", green(&format!("New state:   NeedsCompilation ({})", format_bytes(after))));
    0
}

fn clear_all(b: &DriverBackend) -> i32 {
    let info = b.driver_info();
    println!("Total cache detected on disk: {}", format_bytes(info.total_cache_size_bytes));
    println!("  - Global Driver Cache:      {} ({})", format_bytes(info.cache_size_bytes), info.shader_cache_path);
    if info.steam_shader_cache_size_bytes > 0 {
        println!("  - Steam Game Shader Caches: {}", format_bytes(info.steam_shader_cache_size_bytes));
    }
    if !confirm("\nAre you sure you want to purge ALL GPU driver and Steam game shader caches? (y/N): ") {
        println!("Operation cancelled.");
        return 0;
    }
    println!("Purging all driver and game shader caches from disk...");
    b.clear_all_caches();
    let after = b.driver_info();
    println!("{}", green("\nSUCCESS: All shader caches successfully purged from disk!"));
    println!("{}", green(&format!("Remaining cache size: {}", format_bytes(after.total_cache_size_bytes))));
    0
}

fn clear_interactive() -> i32 {
    println!("Select cache purge option:");
    println!("  1) Clear ALL GPU driver and Steam game shader caches");
    println!("  2) Clear shader cache for a specific game");
    println!("  3) Clear only global driver cache");
    println!("  q) Cancel");
    print!("\nEnter choice (1-3, q): ");
    let _ = io::stdout().flush();
    let mut c = String::new();
    let _ = io::stdin().lock().read_line(&mut c);
    let b = drivers::detect_active_backend();
    match c.trim() {
        "1" => clear_all(&b),
        "2" => {
            print!("Enter game name or App ID: ");
            let _ = io::stdout().flush();
            let mut q = String::new();
            let _ = io::stdin().lock().read_line(&mut q);
            if q.trim().is_empty() {
                println!("No game specified. Cancelled.");
                0
            } else {
                clear_game(&GameManager::default(), &b, q.trim())
            }
        }
        "3" => {
            println!("Purging global driver cache...");
            b.clear_cache();
            println!("{}", green("Global driver cache cleared."));
            0
        }
        _ => {
            println!("Cancelled.");
            0
        }
    }
}
