//! The relay's own `session-bind@openssh.com` against OpenSSH's real `ssh-agent` (DESIGN.md
//! section 10, OD-5). Throwaway keys are made at test time. Skipped, with a line on stderr, where
//! `ssh-agent`, `ssh-add` or `ssh-keygen` is missing or too old for destination constraints.
//!
//! A key added with `ssh-add -h github.example` may be used only from this machine to that host.
//! The host fil relays to could otherwise claim to be that first hop: on a connection the agent
//! sees as local, it binds `github.example` itself (`is_forwarding = 0`) and asks for a hostbound
//! userauth signature, which the agent then makes. The relay binds every connection first, as a
//! forwarding hop to a key of its own, so the agent refuses such a key whatever the host binds
//! after. Unconstrained keys still list and sign. The control run (no bind) shows the attack
//! works without it, so the test is about the bind.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fil_forward::BoxStream;
use fil_forward::agent::{KeyUses, Rules, connect_local};
use fil_testkit::agent::{framed, listed, read_answer, sign_request, string};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair};
use tokio::io::AsyncWriteExt;

struct Rig {
    dir: PathBuf,
    agent: Child,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.agent.kill();
        let _ = self.agent.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= c.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unb64(s: &str) -> Vec<u8> {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (mut bits, mut n, mut out) = (0u32, 0, Vec::new());
    for c in s.trim().bytes().take_while(|&c| c != b'=') {
        bits = (bits << 6) | A.iter().position(|&a| a == c).unwrap() as u32;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
        }
    }
    out
}

fn ok(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn pub_blob(path: &Path) -> Vec<u8> {
    let text = std::fs::read_to_string(path).unwrap();
    unb64(text.split(' ').nth(1).unwrap())
}

/// The host's side of a hostbound userauth: the data a signature is asked for.
fn userauth(sid: &[u8], key: &[u8], hostkey: &[u8]) -> Vec<u8> {
    let mut d = string(sid);
    d.push(50); // SSH2_MSG_USERAUTH_REQUEST
    d.extend(string(b"git"));
    d.extend(string(b"ssh-connection"));
    d.extend(string(b"publickey-hostbound-v00@openssh.com"));
    d.push(1);
    d.extend(string(b"ssh-ed25519"));
    d.extend(string(key));
    d.extend(string(hostkey));
    d
}

fn bind(host: &Ed25519KeyPair, host_blob: &[u8], sid: &[u8], forwarding: bool) -> Vec<u8> {
    let sig = host.sign(sid);
    let mut sig_blob = string(b"ssh-ed25519");
    sig_blob.extend(string(sig.as_ref()));
    let mut m = vec![27];
    m.extend(string(b"session-bind@openssh.com"));
    m.extend(string(host_blob));
    m.extend(string(sid));
    m.extend(string(&sig_blob));
    m.push(u8::from(forwarding));
    m
}

async fn ask(s: &mut BoxStream, msg: &[u8]) -> Vec<u8> {
    s.write_all(&framed(msg)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), read_answer(s))
        .await
        .expect("an answer")
        .unwrap()
}

#[tokio::test]
async fn a_destination_constrained_key_fails_closed_and_others_still_sign() {
    let dir = PathBuf::from(format!("/tmp/fil-a1-dc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let gen_ok = |name: &str| {
        ok(Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
            .arg(dir.join(name)))
    };
    if !gen_ok("free") || !gen_ok("held") {
        eprintln!("no ssh-keygen: skipped");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let sock = dir.join("agent.sock");
    let Ok(agent) = Command::new("ssh-agent")
        .arg("-D")
        .arg("-a")
        .arg(&sock)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        eprintln!("no ssh-agent: skipped");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let rig = Rig {
        dir: dir.clone(),
        agent,
    };
    let end = Instant::now() + Duration::from_secs(10);
    while !sock.exists() {
        assert!(Instant::now() < end, "ssh-agent did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The destination's host key, ours to sign with (a host that has really connected there
    // holds a signature of it over its session id).
    let rng = SystemRandom::new();
    let host =
        Ed25519KeyPair::from_pkcs8(Ed25519KeyPair::generate_pkcs8(&rng).unwrap().as_ref()).unwrap();
    let mut host_blob = string(b"ssh-ed25519");
    host_blob.extend(string(host.public_key().as_ref()));
    let kh = dir.join("known_hosts");
    std::fs::write(
        &kh,
        format!("github.example ssh-ed25519 {}\n", b64(&host_blob)),
    )
    .unwrap();
    let add = |args: &[&str], key: &str| {
        ok(Command::new("ssh-add")
            .env("SSH_AUTH_SOCK", &sock)
            .args(args)
            .arg(dir.join(key)))
    };
    assert!(add(&[], "free"), "ssh-add");
    if !add(
        &["-H", kh.to_str().unwrap(), "-h", "github.example"],
        "held",
    ) {
        eprintln!("ssh-add has no destination constraints: skipped");
        return;
    }
    let (free, held) = (
        pub_blob(&dir.join("free.pub")),
        pub_blob(&dir.join("held.pub")),
    );

    for bound in [true, false] {
        let rules = Rules {
            bind: if bound { Rules::default().bind } else { None },
            ..Rules::default()
        };
        let mut s = connect_local(sock.clone(), Arc::new(rules), Arc::new(KeyUses::default()))
            .await
            .unwrap();
        let keys = listed(&ask(&mut s, &[11]).await).unwrap();
        assert!(
            keys.contains(&free),
            "bound {bound}: the free key is listed"
        );
        // An unconstrained key signs anything, bound or not.
        let sig = ask(&mut s, &sign_request(&free, b"a commit")).await;
        assert_eq!(sig[0], 14, "bound {bound}: the free key signs");
        // The host binds the destination as the first hop, then asks for a userauth to it.
        let mut sid = [0u8; 32];
        rng.fill(&mut sid).unwrap();
        ask(&mut s, &bind(&host, &host_blob, &sid, false)).await;
        let got = ask(
            &mut s,
            &sign_request(&held, &userauth(&sid, &held, &host_blob)),
        )
        .await;
        if bound {
            assert_eq!(got, [5], "the constrained key was used through the relay");
        } else {
            assert_eq!(
                got[0], 14,
                "control: without the relay's bind the attack works"
            );
        }
    }
    drop(rig);
}
