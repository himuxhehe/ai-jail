#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("ai-jail only supports Linux and macOS");

mod bootstrap;
mod cli;
mod config;
mod output;
mod sandbox;
mod signals;

fn run_landlock_exec(cli: &cli::CliArgs) -> Result<i32, String> {
    use std::os::unix::process::CommandExt;

    if cli.command.is_empty() {
        return Err("--landlock-exec requires a command".into());
    }

    let project_dir = std::env::current_dir()
        .map_err(|e| format!("Cannot determine current directory: {e}"))?;

    // Load config from .ai-jail in project dir (cwd inside sandbox)
    let existing = config::load();
    let config = config::merge(cli, existing);

    // Apply Landlock inside the sandbox (after bwrap namespace setup)
    sandbox::apply_landlock(&config, &project_dir, cli.verbose);

    // Replace this process with the real command
    let err = std::process::Command::new(&cli.command[0])
        .args(&cli.command[1..])
        .exec();

    Err(format!("Failed to exec {}: {err}", cli.command[0]))
}

fn run() -> Result<i32, String> {
    let cli = cli::parse()?;

    // Internal: apply Landlock and exec (used inside bwrap sandbox)
    if cli.landlock_exec {
        return run_landlock_exec(&cli);
    }

    // Load or skip config
    let existing = if cli.clean {
        config::Config::default()
    } else {
        config::load()
    };

    let config = config::merge(&cli, existing);

    // Handle status command
    if cli.status {
        config::display_status(&config);
        return Ok(0);
    }

    // Handle --init: save config and exit
    if cli.init {
        config::save(&config);
        output::info("Config saved to .ai-jail");
        return Ok(0);
    }

    // Handle --bootstrap: generate AI tool configs and exit
    if cli.bootstrap {
        bootstrap::run(cli.verbose)?;
        return Ok(0);
    }

    // Check sandbox tool is available
    sandbox::check()?;

    // Platform-specific info messages (e.g. no-op flags on macOS)
    sandbox::platform_notes(&config);

    // Prepare sandbox resources (temp hosts file on Linux, no-op on macOS)
    let guard = sandbox::prepare()?;

    let project_dir = std::env::current_dir()
        .map_err(|e| format!("Cannot determine current directory: {e}"))?;

    // Save config in normal mode. In lockdown mode avoid host writes unless user
    // explicitly requested persistence via --init.
    if !config.lockdown_enabled() {
        config::save(&config);
    }

    // Handle dry run
    if cli.dry_run {
        let formatted =
            sandbox::dry_run(&guard, &config, &project_dir, cli.verbose);
        output::dry_run_line(&formatted);
        return Ok(0);
    }

    output::info(&format!("Jail Active: {}", project_dir.display()));

    // Install signal handlers before spawning
    signals::install_handlers();

    // Build bwrap command (reads $HOME, /dev, etc. for mount discovery).
    // When Landlock is enabled, the inner command is wrapped with
    // `ai-jail --landlock-exec` so Landlock is applied INSIDE the
    // sandbox after bwrap finishes mount namespace setup.
    let mut cmd = sandbox::build(&guard, &config, &project_dir, cli.verbose);

    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to start sandbox: {e}"))?;

    let pid = child.id() as i32;
    signals::set_child_pid(pid);

    // Wait for child via nix (handles EINTR correctly)
    let exit_code = signals::wait_child(pid);

    // Explicitly forget the std::process::Child since we already waited via nix.
    // This prevents a double-wait.
    std::mem::forget(child);

    // Guard is dropped here, cleaning up any temp files
    drop(guard);

    Ok(exit_code)
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(msg) => {
            output::error(&msg);
            std::process::exit(1);
        }
    }
}
