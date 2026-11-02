//! The PTY and its child: spawning, resizing, writing, and reading the line discipline.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use nix::sys::termios::LocalFlags;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use fil_proto::msg::TermSize;

use crate::config::SessionConfig;
use crate::proc::{Scope, systemd_run_argv};

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("the command is empty")]
    EmptyCommand,
    #[error("emulator backend {0:?} is not built in")]
    UnknownEmulator(String),
    #[error("could not start the PTY or its child: {0}")]
    Pty(String),
}

/// The PTY's master side and the way to write to it.
pub(crate) struct Pty {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: mpsc::Sender<Vec<u8>>,
    /// Bytes handed to `write` that the PTY has not taken yet.
    queued: Arc<AtomicUsize>,
    /// The child's pid, which is also its process group id: it is a session leader.
    pub pid: u32,
}

pub(crate) struct Spawned {
    pub pty: Pty,
    pub child: Box<dyn Child + Send + Sync>,
    pub reader: Box<dyn Read + Send>,
}

fn size_of(s: TermSize) -> PtySize {
    PtySize {
        rows: s.rows(),
        cols: s.cols(),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// The variables that describe the SSH login which happened to start the daemon, not the
/// session's user.
const STALE_SSH_VARS: [&str; 3] = ["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"];

fn build_command(cfg: &SessionConfig, scope: &Scope) -> Result<CommandBuilder, SpawnError> {
    if cfg.command.as_ref().is_some_and(|c| c.is_empty()) {
        return Err(SpawnError::EmptyCommand);
    }
    let mut cmd = match scope {
        Scope::Tree => match &cfg.command {
            // The user's shell, as a login shell (argv[0] gets a leading dash).
            None => CommandBuilder::new_default_prog(),
            Some(argv) => CommandBuilder::from_argv(argv.iter().map(OsString::from).collect()),
        },
        Scope::Systemd { unit } => {
            let inner = match &cfg.command {
                Some(argv) => argv.clone(),
                // `systemd-run` cannot set argv[0], so ask for a login shell with `-l`.
                None => vec![CommandBuilder::new_default_prog().get_shell(), "-l".into()],
            };
            let description = format!("fil session {}", cfg.name);
            CommandBuilder::from_argv(
                systemd_run_argv(unit, &description, &inner)
                    .into_iter()
                    .map(OsString::from)
                    .collect(),
            )
        }
    };
    let keep: Vec<(&str, OsString)> = if cfg.clear_env && scope.is_systemd() {
        ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"]
            .into_iter()
            .filter_map(|k| std::env::var_os(k).map(|v| (k, v)))
            .collect()
    } else {
        Vec::new()
    };
    if cfg.clear_env {
        cmd.env_clear();
    }
    for (k, v) in keep {
        cmd.env(k, v);
    }
    for k in STALE_SSH_VARS {
        cmd.env_remove(k);
    }
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    cmd.env("TERM", cfg.term.as_str());
    cmd.env("FIL_SESSION", cfg.name.as_str());
    match &cfg.agent_socket {
        Some(p) => cmd.env("SSH_AUTH_SOCK", p),
        None => cmd.env_remove("SSH_AUTH_SOCK"),
    }
    if let Some(dir) = &cfg.cwd {
        cmd.cwd(dir);
    }
    Ok(cmd)
}

/// Opening a PTY is not safe to do from several threads at once on every platform (macOS's
/// `ptsname` hands out a shared buffer, and concurrent opens fail with nonsense errors), and a
/// daemon may create sessions concurrently.
static OPEN_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn spawn(cfg: &SessionConfig, scope: &Scope) -> Result<Spawned, SpawnError> {
    let cmd = build_command(cfg, scope)?;
    let _open = OPEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let pair = native_pty_system()
        .openpty(size_of(cfg.size))
        .map_err(|e| SpawnError::Pty(e.to_string()))?;
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| SpawnError::Pty(e.to_string()))?;
    // Our copy of the slave must go, or the master never sees end of file when the child exits.
    drop(pair.slave);
    let pid = child
        .process_id()
        .ok_or_else(|| SpawnError::Pty("the child has no pid".into()))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| SpawnError::Pty(e.to_string()))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| SpawnError::Pty(e.to_string()))?;

    // Writes can block when the child is not reading, so they have a thread of their own and
    // the session never waits on them.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let queued = Arc::new(AtomicUsize::new(0));
    let taken = queued.clone();
    std::thread::Builder::new()
        .name("fil-pty-write".into())
        .spawn(move || {
            while let Ok(bytes) = rx.recv() {
                let written = writer
                    .write_all(&bytes)
                    .and_then(|()| writer.flush())
                    .is_ok();
                taken.fetch_sub(bytes.len(), Ordering::Relaxed);
                if !written {
                    break;
                }
            }
        })
        .map_err(|e| SpawnError::Pty(e.to_string()))?;

    Ok(Spawned {
        pty: Pty {
            master: Mutex::new(pair.master),
            writer: tx,
            queued,
            pid,
        },
        child,
        reader,
    })
}

impl Pty {
    /// Queues bytes for the child's input. Never blocks.
    pub fn write(&self, bytes: Vec<u8>) {
        self.queued.fetch_add(bytes.len(), Ordering::Relaxed);
        let _ = self.writer.send(bytes);
    }

    /// Bytes passed to [`Pty::write`] that the PTY has not taken yet.
    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    pub fn resize(&self, size: TermSize) {
        let master = self.master.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = master.resize(size_of(size)) {
            tracing::warn!(error = %e, "PTY resize failed");
        }
    }

    /// `(canonical, echo)`: the ICANON and ECHO flags of the PTY's line discipline.
    pub fn mode(&self) -> Option<(bool, bool)> {
        let master = self.master.lock().unwrap_or_else(|e| e.into_inner());
        let t = master.get_termios()?;
        Some((
            t.local_flags.contains(LocalFlags::ICANON),
            t.local_flags.contains(LocalFlags::ECHO),
        ))
    }
}
