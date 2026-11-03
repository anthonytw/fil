//! The PTY and its child: spawning, resizing, writing, and reading the line discipline.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use bytes::Bytes;
use nix::sys::termios::LocalFlags;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::Notify;

use fil_proto::msg::TermSize;

use crate::config::SessionConfig;
use crate::env::{SESSION_ONLY_DROPPED_VARS, login_session_vars_in_env};
use crate::proc::{SCOPE_DESCRIPTION, Scope, systemd_run_argv};

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("the command is empty")]
    EmptyCommand,
    #[error("emulator backend {0:?} is not built in")]
    UnknownEmulator(String),
    #[error("could not start the PTY or its child: {0}")]
    Pty(String),
    #[error("cannot run the session in a systemd user scope: {0}")]
    Scope(String),
}

/// The PTY's master side and the way to write to it.
pub(crate) struct Pty {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: mpsc::Sender<Bytes>,
    /// Bytes ever handed to `write`.
    queued: AtomicU64,
    /// Bytes the writer thread has got into the PTY (the line discipline has them).
    written: Arc<AtomicU64>,
    /// Signalled by the writer thread after each write, so the core need not poll `written`.
    pub written_signal: Arc<Notify>,
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

/// Removes what describes the login that started the daemon ([`crate::env`]) from the inherited
/// environment. What the configuration sets afterwards is the caller's choice and stays.
///
/// The names come from the process's real environment (`CommandBuilder` starts from it), not
/// from the builder's `*_as_str` iterators, which skip any entry whose value is not UTF-8.
fn drop_login_vars(cmd: &mut CommandBuilder) {
    for k in login_session_vars_in_env() {
        cmd.env_remove(k);
    }
    for k in SESSION_ONLY_DROPPED_VARS {
        cmd.env_remove(k);
    }
}

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
            CommandBuilder::from_argv(
                systemd_run_argv(unit, SCOPE_DESCRIPTION, &inner)
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
    drop_login_vars(&mut cmd);
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
    let mut cmd = build_command(cfg, scope)?;
    // The child starts with the descriptor limit the daemon had before it raised its own
    // (`crate::nofile`), through the trampoline.
    crate::nofile::wrap(&mut cmd).map_err(SpawnError::Pty)?;
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
    let (tx, rx) = mpsc::channel::<Bytes>();
    let written = Arc::new(AtomicU64::new(0));
    let taken = written.clone();
    let signal = Arc::new(Notify::new());
    let writer_signal = signal.clone();
    std::thread::Builder::new()
        .name("fil-pty-write".into())
        .spawn(move || {
            while let Ok(bytes) = rx.recv() {
                let written = writer
                    .write_all(&bytes)
                    .and_then(|()| writer.flush())
                    .is_ok();
                taken.fetch_add(bytes.len() as u64, Ordering::SeqCst);
                writer_signal.notify_one();
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
            queued: AtomicU64::new(0),
            written,
            written_signal: signal,
            pid,
        },
        child,
        reader,
    })
}

impl Pty {
    /// Queues bytes for the child's input. Never blocks.
    ///
    /// Returns a ticket: [`Pty::written`] reaches it once the PTY has taken these bytes.
    pub fn write(&self, bytes: Bytes) -> u64 {
        let ticket =
            self.queued.fetch_add(bytes.len() as u64, Ordering::SeqCst) + bytes.len() as u64;
        let _ = self.writer.send(bytes);
        ticket
    }

    /// Bytes the PTY has taken so far, counted like the tickets.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::SeqCst)
    }

    /// Bytes passed to [`Pty::write`] that the PTY has not taken yet.
    pub fn queued(&self) -> usize {
        let queued = self.queued.load(Ordering::SeqCst);
        queued.saturating_sub(self.written()) as usize
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    /// The variables the test process is started with, by the parent half of
    /// `the_login_environment_is_dropped_even_when_a_value_is_not_utf8`. The values of the first
    /// two are Latin-1 bytes, which are not UTF-8.
    fn login_environment() -> Vec<(&'static str, std::ffi::OsString)> {
        use std::os::unix::ffi::OsStrExt;
        let latin1 = |b: &[u8]| std::ffi::OsString::from(OsStr::from_bytes(b));
        vec![
            ("SSH_ORIGINAL_COMMAND", latin1(b"caf\xe9 \xff")),
            ("LC_TERMINAL", latin1(b"iTerm\xe9")),
            ("DISPLAY", "localhost:10.0".into()),
            ("XDG_SESSION_ID", "42".into()),
            ("XDG_SESSION_TYPE", "tty".into()),
            ("SSH_CONNECTION", "a b c d".into()),
            ("TMUX", "/tmp/tmux-1000/default,1,0".into()),
            ("FIL_LOG", "trace".into()),
        ]
    }

    /// The process environment is global, so the checks run in a child copy of the test binary
    /// that is started with the login's variables set. The parent half starts it.
    #[test]
    fn the_login_environment_is_dropped_even_when_a_value_is_not_utf8() {
        const CHILD: &str = "FIL_TEST_LOGIN_ENV_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let mut cmd = CommandBuilder::new("true");
            for (k, _) in login_environment() {
                assert!(cmd.get_env(k).is_some(), "{k} was not inherited: bad setup");
            }
            drop_login_vars(&mut cmd);
            for (k, _) in login_environment() {
                assert_eq!(cmd.get_env(k), None, "{k} must be dropped");
            }
            assert_eq!(
                cmd.get_env("XDG_RUNTIME_DIR"),
                Some(OsStr::new("/run/user/1000"))
            );
            assert_eq!(cmd.get_env("LANG"), Some(OsStr::new("C")));
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "pty::tests::the_login_environment_is_dropped_even_when_a_value_is_not_utf8",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("XDG_RUNTIME_DIR", "/run/user/1000")
            .env("LANG", "C");
        for (k, v) in login_environment() {
            child.env(k, v);
        }
        let out = child.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{text}");
        assert!(
            text.contains("1 passed"),
            "the child test did not run:\n{text}"
        );
    }

    #[test]
    fn a_systemd_scope_names_the_session_neither_in_its_unit_nor_in_its_description() {
        let mut cfg = SessionConfig::new(
            "acme-incident".parse().unwrap(),
            "xterm-256color".parse().unwrap(),
            TermSize::new(80, 24).unwrap(),
            0x1234_5678_9abc_def0_1111_2222_3333_4444,
        );
        cfg.command = Some(vec!["/bin/true".into()]);
        let unit = crate::proc::unit_name(cfg.incarnation);
        let cmd = build_command(&cfg, &Scope::Systemd { unit }).unwrap();
        let argv: Vec<String> = cmd
            .get_argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            argv.iter().any(|a| a.starts_with("--unit=fil-")),
            "{argv:?}"
        );
        assert!(
            argv.iter().all(|a| !a.contains("acme")),
            "the session name is in the command line systemd records: {argv:?}"
        );
        assert!(argv.contains(&format!("--description={SCOPE_DESCRIPTION}")));
    }
}
