//! `tmpclean-daemon [--every-ms N] [--no-refresh] [--no-dir-lock]`: the fil daemon of the current
//! user, at the standard `/tmp/fil-$UID` paths, with its keep-alive settings (`Keeping`) given on
//! the command line. The real `fil` relay finds it by its socket, as it finds any daemon of this
//! wire revision. For the macOS runner's `tmp_cleaner` test only; never shipped.

use std::process::ExitCode;
use std::time::Duration;

use fil_daemon::paths::Keeping;
use fil_daemon::{Daemon, DaemonConfig, Paths, StartError, ensure_socket_dir};

fn main() -> ExitCode {
    // A session's child starts through this binary (the descriptor-limit trampoline).
    fil_daemon::run_trampoline_if_asked();
    let mut keeping = Keeping {
        every: Duration::from_secs(1),
        ..Keeping::default()
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--every-ms" => {
                let ms = args.next().and_then(|v| v.parse::<u64>().ok());
                let Some(ms) = ms else {
                    eprintln!("tmpclean-daemon: --every-ms needs a number");
                    return ExitCode::from(2);
                };
                keeping.every = Duration::from_millis(ms);
            }
            "--no-refresh" => keeping.refresh = false,
            "--no-dir-lock" => keeping.dir_lock = false,
            other => {
                eprintln!("tmpclean-daemon: unknown argument {other}");
                return ExitCode::from(2);
            }
        }
    }
    let paths = Paths::for_current_user();
    if let Err(e) = ensure_socket_dir(&paths.dir) {
        eprintln!("tmpclean-daemon: {}: {e}", paths.dir.display());
        return ExitCode::FAILURE;
    }
    fil_daemon::logging::init(&paths.log, true);
    let _flush = fil_daemon::logging::flush_at_exit();
    let cfg = match DaemonConfig::load(&paths.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tmpclean-daemon: bad configuration: {e}");
            return ExitCode::FAILURE;
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tmpclean-daemon: cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = rt.block_on(async {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            eprintln!("tmpclean-daemon: cannot install signal handlers");
            return ExitCode::FAILURE;
        };
        let mut daemon = match Daemon::start(paths.clone(), cfg).await {
            Ok(d) => d,
            Err(StartError::AlreadyRunning(_)) => {
                eprintln!("tmpclean-daemon: another daemon is running");
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("tmpclean-daemon: cannot start: {e}");
                return ExitCode::FAILURE;
            }
        };
        daemon.set_keeping(keeping);
        let handle = daemon.handle();
        tokio::spawn(async move {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            tracing::info!("stopping on a signal");
            handle.shutdown(true);
        });
        daemon.run().await;
        ExitCode::SUCCESS
    });
    rt.shutdown_timeout(Duration::from_secs(2));
    code
}
