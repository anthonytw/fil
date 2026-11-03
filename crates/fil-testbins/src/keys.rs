//! The key bootstrap the QUIC stand-ins (`quic-echo`, `quiclink-probe`) share. The real daemon
//! learns a client's key through ssh; here the harness plays that part with files in a directory:
//! the client writes `NAME.pub` (hex of its SPKI), the server registers it and answers with
//! `NAME.registered`, and the harness hands the client the server's pin (its fingerprint) in a
//! file of its own.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fil_transport::auth::{ClientKey, Fingerprint, KeyRegistry, PublicKey};

use crate::{Args, hex_decode, hex_encode};

/// Write a file so a reader never sees it half written.
pub fn write_atomic(path: &str, content: &str) -> Result<(), String> {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("write {tmp}: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {tmp}: {e}"))
}

pub fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Registers every `*.pub` file (hex of the client's SPKI) that appears in `dir`, and writes the
/// `.registered` acknowledgement the client waits for (the bootstrap's "registered"). `on_register`
/// gets the key and the file name, to print the program's own `REGISTERED` line.
pub async fn watch_keys(
    dir: PathBuf,
    registry: KeyRegistry,
    on_register: impl Fn(&PublicKey, &str) + Send + 'static,
) {
    let mut seen = std::collections::HashSet::new();
    loop {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "pub")
                    && !seen.contains(&path)
                    && let Some(bytes) = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| hex_decode(&s))
                {
                    seen.insert(path.clone());
                    let key = PublicKey::from_spki_der(bytes);
                    registry.register(key.clone());
                    on_register(&key, &file_name(&path));
                    let _ = std::fs::write(path.with_extension("registered"), b"ok");
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The client's side of the bootstrap: generates a key, hands its public half to the "bootstrap"
/// (`--pub-out`, a file), and waits (up to 60 s) for it to be registered and for the pin
/// (`--pin-file`: the daemon's fingerprint) to come back.
pub async fn client_bootstrap(a: &Args) -> Result<(ClientKey, Fingerprint), String> {
    let key = ClientKey::generate().map_err(|e| e.to_string())?;
    let pub_out = a.require("pub-out")?;
    write_atomic(pub_out, &hex_encode(key.public_key().as_spki_der()))?;
    let pin_file = a.require("pin-file")?;
    let registered = Path::new(pub_out).with_extension("registered");
    let waited = Instant::now();
    loop {
        if registered.exists()
            && let Ok(s) = std::fs::read_to_string(pin_file)
            && let Ok(fp) = s.trim().parse()
        {
            return Ok((key, fp));
        }
        if waited.elapsed() > Duration::from_secs(60) {
            return Err("no registration and pin after 60 s".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
