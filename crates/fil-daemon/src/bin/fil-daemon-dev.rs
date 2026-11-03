//! A small stand-in for the `fil` binary's server side, so the daemon can be exercised before the
//! real binary exists (and by this crate's tests, which run it as a separate process).
//!
//! ```text
//! fil-daemon-dev [--dir DIR] daemon [--command SCRIPT]
//!                                            run the daemon (what the relay starts); sessions run
//!                                            `sh -c SCRIPT` instead of a login shell
//! fil-daemon-dev [--dir DIR] relay           the stdio relay (`fil server --stdio`)
//! fil-daemon-dev [--dir DIR] ls              list sessions, through a real relay
//! fil-daemon-dev [--dir DIR] kill NAME       kill a session
//! fil-daemon-dev [--dir DIR] detach NAME     detach whoever is attached
//! fil-daemon-dev [--dir DIR] sockets         the daemons' sockets in the directory
//! ```
//!
//! `--dir` replaces `/tmp/fil-$UID` (it is still checked for owner and mode); configuration and
//! the log then live inside it. Not shipped: the real entry points are in the library.

use std::ffi::OsString;
use std::process::ExitCode;

use fil_daemon::{DaemonLauncher, Paths, RelayOptions, run_daemon_with, run_relay};
use fil_proto::ChannelId;
use fil_proto::msg::{Agent, ControlMsg, Hello, Request, Response};
use fil_transport::Link;
use fil_transport::pipe::{PipeCommand, PipeConfig, PipeLink};

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut dir = None;
    if args.first().is_some_and(|a| a == "--dir") && args.len() >= 2 {
        dir = Some(std::path::PathBuf::from(args.remove(1)));
        args.remove(0);
    }
    let paths = dir
        .clone()
        .map_or_else(Paths::for_current_user, Paths::in_dir);
    let Some(cmd) = args.first().and_then(|a| a.to_str()).map(str::to_owned) else {
        eprintln!(
            "usage: fil-daemon-dev [--dir DIR] daemon|relay|ls|kill NAME|detach NAME|sockets"
        );
        return ExitCode::from(2);
    };
    let exe = std::env::current_exe().expect("own path");
    let with_dir = |sub: &str| -> Vec<OsString> {
        let mut v = Vec::new();
        if let Some(d) = &dir {
            v.push("--dir".into());
            v.push(d.clone().into());
        }
        v.push(sub.into());
        v
    };
    match cmd.as_str() {
        "daemon" => {
            let script = match (args.get(1).and_then(|a| a.to_str()), args.get(2)) {
                (Some("--command"), Some(s)) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            };
            run_daemon_with(&paths, |cfg| {
                if let Some(script) = script {
                    cfg.session.command = Some(vec!["/bin/sh".into(), "-c".into(), script]);
                }
            })
        }
        "relay" => run_relay(&RelayOptions {
            paths,
            launcher: DaemonLauncher::new(&exe, with_dir("daemon")),
        }),
        "sockets" => {
            for (major, path) in fil_daemon::list_sockets(&paths.dir) {
                println!("{major} {}", path.display());
            }
            ExitCode::SUCCESS
        }
        "ls" | "kill" | "detach" => {
            let name = args.get(1).and_then(|a| a.to_str());
            let request = match (cmd.as_str(), name) {
                ("ls", _) => Request::ListSessions,
                ("kill", Some(n)) => match n.parse() {
                    Ok(name) => Request::KillSession { name },
                    Err(e) => return fail(e),
                },
                ("detach", Some(n)) => match n.parse() {
                    Ok(name) => Request::DetachSession { name },
                    Err(e) => return fail(e),
                },
                _ => return fail("a session name is needed"),
            };
            let mut relay_argv: Vec<OsString> = vec![exe.clone().into()];
            relay_argv.extend(with_dir("relay"));
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            match rt.block_on(ask(relay_argv, request)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(e),
            }
        }
        other => fail(format!("unknown command {other:?}")),
    }
}

fn fail(e: impl std::fmt::Display) -> ExitCode {
    eprintln!("fil-daemon-dev: {e}");
    ExitCode::FAILURE
}

/// Sends `Hello` and one `Request` through the relay and prints the `Response`.
async fn ask(relay_argv: Vec<OsString>, request: Request) -> Result<(), String> {
    let command = PipeCommand::custom(relay_argv).map_err(|e| e.to_string())?;
    let link = PipeLink::spawn(&command, PipeConfig::default())
        .await
        .map_err(|e| e.to_string())?;
    let send = |m: ControlMsg| {
        let payload = m.encode().expect("encodes");
        let link = &link;
        async move {
            link.send(ChannelId::CONTROL, payload.into())
                .await
                .map_err(|e| e.to_string())
        }
    };
    let agent = Agent::new("fil-daemon-dev").expect("valid");
    send(ControlMsg::Hello(Hello::ours(agent))).await?;
    send(ControlMsg::Request(request)).await?;
    loop {
        let (chan, payload) = link.recv().await.map_err(|e| e.to_string())?;
        if chan != ChannelId::CONTROL {
            return Err("unexpected data".into());
        }
        match ControlMsg::decode(&payload).map_err(|e| e.to_string())? {
            ControlMsg::Hello(_) => {}
            ControlMsg::Response(Response::Sessions { list, truncated }) => {
                for s in list {
                    let who = s.attached.map_or("detached".to_string(), |a| {
                        format!("attached from {}", a.host)
                    });
                    println!("{}\t{}\t{who}", s.name, s.term);
                }
                if truncated {
                    println!("(list truncated)");
                }
                break;
            }
            ControlMsg::Response(Response::Done) => {
                println!("done");
                break;
            }
            ControlMsg::Response(Response::NoSuchSession) => {
                return Err("no such session".into());
            }
            ControlMsg::Error(code) => return Err(format!("the daemon said {code:?}")),
            other => return Err(format!("unexpected message {other:?}")),
        }
    }
    link.shutdown().await;
    Ok(())
}
