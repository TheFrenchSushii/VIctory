use anyhow::{Context, bail};
use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(super) fn run(warm: bool) -> anyhow::Result<()> {
    let launcher = std::env::current_exe()?;
    let folder = launcher.parent().context("Launcher folder is missing")?;
    let runtime = folder.join("victory.exe");
    let mut args = std::env::args_os().skip(1).peekable();
    let (mut warp, mut rt, mut warm_only, mut check_paths) = (false, false, false, false);
    while let Some(option) = args.peek() {
        if option == "--warp" {
            warp = true;
        } else if option == "--rt" {
            rt = true;
        } else if option == "--warm-only" && warm {
            warm_only = true;
        } else if option == "--check-paths" && warm {
            check_paths = true;
        } else {
            break;
        }
        args.next();
    }
    let game = match args.next() {
        Some(path) => PathBuf::from(path),
        None => {
            let config = folder.join("game-path.txt");
            let value = fs::read_to_string(&config)
                .with_context(|| format!("Missing or unreadable {}. Run Setup.cmd -GamePath <game executable> again.", config.display()))?;
            if value.trim().is_empty() {
                bail!("Game path is empty in {}. Run Setup.cmd -GamePath <game executable> again.", config.display());
            }
            folder.join(value.trim())
        }
    };
    if args.next().is_some() {
        bail!("Usage\n{} [--rt] [--warp]{} [game executable]", launcher.display(), if warm { " [--warm-only] [--check-paths]" } else { "" });
    }
    let game = std::path::absolute(game)?;
    let root = game.parent().context("Game install folder is missing")?;
    validate_game(&game)?;
    checked_file(&runtime).context("Runtime executable is missing or unreadable. Run Setup.cmd again.")?;
    let logs = folder.join("logs");
    let state = folder.join("state");
    let cache = std::env::var_os("VICTORY_CACHE_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(|| folder.join("cache"), |value| folder.join(value));
    // Reject source-tree destinations before creating anything. Resolve
    // existing paths before comparing them, so junctions cannot silently
    // point the cache at the game's files or the persistent save directory.
    for dir in [root, logs.as_path(), state.as_path(), cache.as_path()] {
        victory_core::ensure_outside_repo(dir)?;
    }
    for dir in [&logs, &state, &cache] {
        fs::create_dir_all(dir).with_context(|| format!("Cannot create {}", dir.display()))?;
    }
    let dirs = [root, logs.as_path(), state.as_path(), cache.as_path()];
    let resolved = dirs.iter().map(fs::canonicalize).collect::<std::io::Result<Vec<_>>>()?;
    for i in 0..resolved.len() {
        for j in i + 1..resolved.len() {
            if paths_overlap(&resolved[i], &resolved[j]) {
                bail!("Game, logs, saves and cache must be separate directories: {} and {}", dirs[i].display(), dirs[j].display());
            }
        }
    }
    let _session = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(state.join("launcher.lock"))
        .context("Could not lock the launcher state. Check for another DexNom Runtime session.")?;
    let started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    for dir in [&cache, &logs] {
        let probe = dir.join(format!(".write-check-{started}-{}", std::process::id()));
        let file = OpenOptions::new().write(true).create_new(true).open(&probe)
            .with_context(|| format!("Directory is not writable: {}", dir.display()))?;
        drop(file);
        fs::remove_file(&probe).with_context(|| format!("Cannot remove write check: {}", probe.display()))?;
    }

    if warm && !check_paths {
        let _ = writeln!(
            std::io::stdout().lock(),
            "Path checks passed\nGame: {}\nGame directory: {}\nCache: {}\nSaves: {}\nLogs: {}",
            game.display(), root.display(), cache.display(), state.display(), logs.display()
        );
    }
    // Keep the same launcher lock for both stages; no gap for another
    // launcher or setup to modify the cache between warmup and gameplay.
    let command = || {
        let mut child = Command::new(&runtime);
        if rt {
            child.env("VICTORY_BVH_BUILDER", "1")
                .env("VICTORY_BVH_QUARANTINE", "0")
                .env("VICTORY_ACTIVATE_RAYS", "1")
                .env("VICTORY_RT_RESOLVE", "1");
        }
        if warp {
            child.env("VICTORY_D3D12_WARP", "1");
        }
        if std::env::var_os("VICTORY_DEBUG_HUD").is_none() {
            child.env("VICTORY_DEBUG_HUD", "1");
        }
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        child.current_dir(folder)
            .env("VICTORY_GAME_DIR", root)
            .env("VICTORY_DATA_DIR", &state)
            .env("VICTORY_CACHE_DIR", &cache)
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        child
    };

    if check_paths {
        // Also check the configuration actually resolved by the child (including
        // any explicit VICTORY_CONFIG file), without launching title code.
        let output = command().arg("config").output()
            .context("Could not check the runtime configuration")?;
        if !output.status.success() {
            bail!("Runtime configuration check failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        let config = String::from_utf8_lossy(&output.stdout);
        if config.contains("POLICY VIOLATION:") {
            bail!("Runtime configuration check failed:\n{config}");
        }
        let _ = writeln!(std::io::stdout().lock(), "{config}\nPath checks passed. No shader compilation or game launch requested.");
        return Ok(());
    }

    if warm {
        let (path, mut log) = open_run_log(&logs, "warm", started, &runtime, &game, rt, warp)?;
        let mut child = command();
        child.stderr(log.try_clone()?);
        let status = preload(&mut child, &game, &cache, &path, &mut log)?;
        require_success(status, &path)?;
        if warm_only {
            let _ = writeln!(std::io::stdout().lock(), "Shader warmup completed. Warm-only mode: game not started.");
            return Ok(());
        }
        // A successful exit and a nonempty final summary are required.
        // Unsupported shaders can still be reported by the compiler.
        validate_game(&game)?;
        let _ = writeln!(std::io::stdout().lock(), "Shader warmup completed. Starting the game...");
    }

    let (path, mut log) = open_run_log(&logs, "run", started, &runtime, &game, rt, warp)?;
    let status = command()
        .arg("run")
        .arg(&game)
        .args(["--execute", "--console", "series-x", "--unimplemented", "trap"])
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?)
        .status()
        .with_context(|| format!("Could not start {}", runtime.display()))?;
    writeln!(log, "VIctory exited with {status}")?;
    require_success(status, &path)
}

fn checked_file(path: &Path) -> anyhow::Result<File> {
    let file = File::open(path).with_context(|| format!("Cannot read {}", path.display()))?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() == 0 {
        bail!("Expected a nonempty file: {}", path.display());
    }
    Ok(file)
}

fn validate_game(game: &Path) -> anyhow::Result<()> {
    if !game.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe")) {
        bail!("Select the original game executable (.exe), not a folder: {}", game.display());
    }
    let mut file = checked_file(game).context("Invalid game path. Run Setup.cmd -GamePath <game executable> again.")?;
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic).with_context(|| format!("Cannot read executable header: {}", game.display()))?;
    if &magic != b"MZ" {
        bail!("Not a readable PE executable: {}", game.display());
    }
    // These archives are required by the GTA VI installation targeted by
    // this fork. Do not impose this layout on unrelated Xbox test titles.
    if game.file_name().is_some_and(|name| name.eq_ignore_ascii_case("game_scarlett_master.exe")) {
        let root = game.parent().context("Game install folder is missing")?;
        let mut missing = Vec::new();
        for name in ["MicrosoftGame.Config", "common.rpf", "data.rpf", "levels.rpf", "shaders.rpf"] {
            if let Err(error) = checked_file(&root.join(name)) {
                missing.push(format!("{name}: {error:#}"));
            }
        }
        if !missing.is_empty() {
            bail!("Game installation is incomplete or unreadable in {}:\n{}\nSelect the complete game installation in Setup.cmd.", root.display(), missing.join("\n"));
        }
    }
    Ok(())
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    // Canonicalized Windows paths: compare whole components, not string
    // prefixes (cache and cache_backup are distinct).
    fn prefix(a: &Path, b: &Path) -> bool {
        let mut parts = b.components();
        a.components().all(|part| parts.next().is_some_and(|other| {
            part.as_os_str().eq_ignore_ascii_case(other.as_os_str())
        }))
    }
    prefix(a, b) || prefix(b, a)
}

fn open_run_log(
    logs: &Path, action: &str, started: u128, runtime: &Path, game: &Path, rt: bool, warp: bool,
) -> anyhow::Result<(PathBuf, File)> {
    let path = logs.join(format!("{action}-{started}-{}.log", std::process::id()));
    let mut log = OpenOptions::new().append(true).create_new(true).open(&path)?;
    writeln!(log, "Runtime {}\nGame {}\nExperimental RT: {rt}; WARP: {warp}", runtime.display(), game.display())?;
    let latest = if action == "warm" { "latest-warm.txt" } else { "latest.txt" };
    fs::write(logs.join(latest), path.display().to_string())?;
    Ok((path, log))
}

fn require_success(status: ExitStatus, path: &Path) -> anyhow::Result<()> {
    if !status.success() {
        bail!("VIctory exited with {status}.\n\nLog file\n{}", path.display());
    }
    Ok(())
}

fn preload(
    child: &mut Command,
    game: &Path,
    cache: &Path,
    path: &Path,
    log: &mut File,
) -> anyhow::Result<ExitStatus> {
    let root = game.parent().context("Game install folder is missing")?;
    writeln!(
        log,
        "Scan {}\nCache {}\nThe offline reader recognizes SGD5 records and raw DXBC in files up to 2 GiB. Graphics pipelines are created when the game supplies their draw state.",
        root.display(),
        cache.display()
    )?;
    let _ = writeln!(
        std::io::stdout().lock(),
        "Preloading shaders and compute pipelines from {}\nLive log: {}",
        root.display(),
        path.display()
    );
    // Give the worker a file rather than a pipe owned by the launcher.
    // A broken console or an interrupted reader cannot panic its workers.
    let reader = BufReader::new(File::open(path)?);
    let mut worker = child
        .arg("warm")
        .arg(root)
        .stdout(log.try_clone()?)
        .spawn()
        .context("Could not start DexNom Runtime shader preload")?;
    // Read a regular file, never a pipe: loss of the viewer must not
    // interrupt the shader compiler or its cache writes.
    let status = match monitor_warm(&mut worker, reader) {
        Ok(status) => status,
        Err(error) => {
            let _ = writeln!(std::io::stderr().lock(), "Progress viewer error: {error}. Waiting for the compiler; see {}", path.display());
            worker.wait()?
        }
    };
    let last = last_warm_summary(BufReader::new(File::open(path)?));
    writeln!(log, "VIctory exited with {status}")?;
    if status.success()
        && let Err(message) = check_warm_summary(last?.as_deref())
    {
        writeln!(log, "{message}")?;
        bail!("{message}\n\nLog file\n{}", path.display());
    }
    Ok(status)
}

/// Only display concise progress records in the console. Full diagnostics
/// remain in the run log, including individual shader messages.
fn warm_console_line(line: &str) -> Option<&str> {
    let line = line.trim_end_matches(['\r', '\n']);
    if let Some(progress) = line.strip_prefix("warm-progress ") {
        Some(progress)
    } else if line.starts_with("warm ") {
        Some(line)
    } else {
        None
    }
}

/// Read newly appended complete lines. Keep a partial line for the next poll
/// so a writer that has not finished a record cannot corrupt the display.
fn show_warm_lines(reader: &mut impl BufRead, pending: &mut Vec<u8>, output: &mut impl Write) -> std::io::Result<bool> {
    let mut displayed = false;
    while reader.read_until(b'\n', pending)? != 0 {
        if pending.last() != Some(&b'\n') {
            break;
        }
        if let Some(line) = warm_console_line(&String::from_utf8_lossy(pending)) {
            let _ = writeln!(output, "{line}");
            displayed = true;
        }
        pending.clear();
    }
    let _ = output.flush();
    Ok(displayed)
}

fn monitor_warm(worker: &mut Child, mut reader: impl BufRead) -> std::io::Result<ExitStatus> {
    let started = Instant::now();
    let mut last_update = started;
    let mut pending = Vec::new();
    loop {
        // Check exit before draining the log, so the final summary is also read.
        let status = worker.try_wait()?;
        {
            let mut output = std::io::stdout().lock();
            if show_warm_lines(&mut reader, &mut pending, &mut output)? {
                last_update = Instant::now();
            } else if status.is_none() && last_update.elapsed() >= Duration::from_secs(10) {
                let _ = writeln!(
                    output,
                    "Compiler still running | {} s elapsed | no new progress record yet",
                    started.elapsed().as_secs()
                );
                let _ = output.flush();
                last_update = Instant::now();
            }
        }
        if let Some(status) = status {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Read the last completed warmup summary from the append-only run log.
fn last_warm_summary(reader: impl BufRead) -> std::io::Result<Option<String>> {
    reader.lines().try_fold(None, |summary, line| {
        let line = line?;
        Ok(if line.starts_with("warm ") {
            Some(line)
        } else {
            summary
        })
    })
}

/// The runtime prints this after saving its pipeline library. Preserve its exact
/// counters in the log, including partial refusal; an empty scan is not a preload.
fn check_warm_summary(summary: Option<&str>) -> Result<(), &'static str> {
    let Some(summary) = summary else {
        return Err("VIctory did not report a shader warmup result");
    };
    let mut words = summary.split_whitespace();
    if words.next() != Some("warm")
        || words
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .is_none_or(|n| n == 0)
    {
        return Err("No shaders successfully translated. See the runtime totals in the log");
    }
    Ok(())
}

#[allow(dead_code)] // the console warmer reports errors to stderr
pub(super) fn show_error(text: &str) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(owner: *mut c_void, text: *const u16, title: *const u16, flags: u32) -> i32;
    }
    let text: Vec<_> = text.encode_utf16().chain([0]).collect();
    let title: Vec<_> = "DexNom Runtime".encode_utf16().chain([0]).collect();
    // SAFETY: the two strings are NUL terminated and live for the call.
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), 0x10) };
}

#[cfg(test)]
mod tests {
    use super::{check_warm_summary, last_warm_summary, paths_overlap, show_warm_lines, warm_console_line};
    use std::path::Path;

    #[test]
    fn paths_cannot_share_game_cache_or_save_directories() {
        assert!(paths_overlap(Path::new(r"C:\Games"), Path::new(r"c:\games\cache")));
        assert!(paths_overlap(Path::new(r"C:\Runtime\cache"), Path::new(r"C:\Runtime\cache")));
        assert!(!paths_overlap(Path::new(r"C:\Runtime\cache"), Path::new(r"C:\Runtime\cache_backup")));
        assert!(!paths_overlap(Path::new(r"C:\Runtime\cache"), Path::new(r"C:\Runtime\state")));
    }

    #[test]
    fn console_filters_diagnostics_and_displays_progress_and_summary() {
        assert_eq!(warm_console_line("warm-progress 50%\r\n"), Some("50%"));
        assert_eq!(warm_console_line("warm  12 translated\n"), Some("warm  12 translated"));
        assert_eq!(warm_console_line("INFO shader translated\n"), None);
    }

    #[test]
    fn console_preserves_a_partial_progress_line() {
        let mut pending = Vec::new();
        let mut output = Vec::new();
        let mut first = &b"INFO detail\nwarm-progress Reading ar"[..];
        assert!(!show_warm_lines(&mut first, &mut pending, &mut output).unwrap());
        assert_eq!(pending.as_slice(), b"warm-progress Reading ar");
        let mut rest = &b"chive\nwarm-progress 50%\nwarm  1 translated\n"[..];
        assert!(show_warm_lines(&mut rest, &mut pending, &mut output).unwrap());
        assert!(pending.is_empty());
        assert_eq!(output.as_slice(), b"Reading archive\n50%\nwarm  1 translated\n");
    }

    #[test]
    fn warm_summary_survives_interleaved_log_messages() {
        let log = b"scan\nwarm  1 translated\nINFO saved cache\nwarm  2 translated\nexit\n";
        assert_eq!(
            last_warm_summary(&log[..]).unwrap().as_deref(),
            Some("warm  2 translated")
        );
        assert!(last_warm_summary(&b"scan only\n"[..]).unwrap().is_none());
    }

    #[test]
    fn empty_and_incomplete_scans_are_reported() {
        assert!(check_warm_summary(None).is_err());
        assert!(
            check_warm_summary(Some(
                "warm  0 translated, 0 refused; 0 compute pipelines, 0 refused; 0 s"
            ))
            .is_err()
        );
        assert!(
            check_warm_summary(Some(
                "warm  0 translated, 2 refused; 0 compute pipelines, 0 refused; 1 s"
            ))
            .is_err()
        );
        assert!(
            check_warm_summary(Some(
                "warm  1 translated, 2 refused; 3 compute pipelines, 4 refused; 5 s"
            ))
            .is_ok()
        );
    }
}
