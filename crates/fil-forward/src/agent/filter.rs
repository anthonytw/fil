//! The agent message filter: what the remote host may ask of the laptop's ssh-agent (DESIGN.md
//! section 10, `docs/m2/plan.md` section 4.3, OD-3). Pure functions over one message, with no IO.
//!
//! **This runs on the client, the trusted end.** The daemon relays agent bytes without reading
//! them; any process of the user's uid on the remote can write to the agent socket, so every
//! message is treated as hostile:
//!
//! - only `REQUEST_IDENTITIES` (11), `SIGN_REQUEST` (13) and `EXTENSION` (27) for
//!   `session-bind@openssh.com` and `query` pass ([`check_request`]); everything else, among them
//!   add, remove, remove-all, lock, unlock, smartcard and constrained add, is answered with
//!   `SSH_AGENT_FAILURE` and never reaches the agent;
//! - a request must parse exactly: a message with a short field or bytes left over is refused;
//! - a message is at most [`MAX_AGENT_MSG`] bytes;
//! - the agent's answers are checked too ([`check_answer`]): a list answer is parsed and rebuilt
//!   with only the keys the allowlist admits ([`filter_identities`]), and an answer of a type
//!   that does not answer the request becomes `SSH_AGENT_FAILURE`.
//!
//! Nothing here panics on any input: every length is checked before it is used, and counts are
//! bounded by the bytes that remain.
//!
//! The message layout is the one in draft-miller-ssh-agent: a `uint32` length, then that many
//! bytes, the first of which is the type. The functions here take the bytes after the length.

use fil_proto::limits::MAX_AGENT_MSG;

/// `SSH_AGENT_FAILURE`.
pub const FAILURE: u8 = 5;
/// `SSH_AGENT_SUCCESS`.
pub const SUCCESS: u8 = 6;
/// `SSH_AGENTC_REQUEST_IDENTITIES`.
pub const REQUEST_IDENTITIES: u8 = 11;
/// `SSH_AGENT_IDENTITIES_ANSWER`.
pub const IDENTITIES_ANSWER: u8 = 12;
/// `SSH_AGENTC_SIGN_REQUEST`.
pub const SIGN_REQUEST: u8 = 13;
/// `SSH_AGENT_SIGN_RESPONSE`.
pub const SIGN_RESPONSE: u8 = 14;
/// `SSH_AGENTC_EXTENSION`.
pub const EXTENSION: u8 = 27;
/// `SSH_AGENT_EXTENSION_FAILURE`.
pub const EXTENSION_FAILURE: u8 = 28;
/// `SSH_AGENT_EXTENSION_RESPONSE`.
pub const EXTENSION_RESPONSE: u8 = 29;

/// The extensions the remote may use: OpenSSH's session binding (8.9+), and the list of
/// extensions the agent supports.
pub const ALLOWED_EXTENSIONS: [&str; 2] = ["session-bind@openssh.com", "query"];

/// The `SSH_AGENT_FAILURE` message, with its length.
pub const FAILURE_MSG: [u8; 5] = [0, 0, 0, 1, FAILURE];

/// Longest key type and comment shown in a prompt or a summary, in characters.
pub const MAX_SHOWN: usize = 64;

/// What a request that passed the filter asks for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Request<'a> {
    /// List the keys.
    Identities,
    /// Sign with the key whose public blob is `key`.
    Sign { key: &'a [u8] },
    /// One of [`ALLOWED_EXTENSIONS`].
    Extension,
}

/// Names no key (a blob is user data).
impl std::fmt::Debug for Request<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Request::Identities => f.write_str("Identities"),
            Request::Sign { .. } => f.write_str("Sign"),
            Request::Extension => f.write_str("Extension"),
        }
    }
}

/// Why a request was answered with `SSH_AGENT_FAILURE` instead of being passed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A message with no type byte.
    Empty,
    /// A type the relay does not pass (add, remove, lock, ...), or an unknown one.
    Type(u8),
    /// An extension other than [`ALLOWED_EXTENSIONS`].
    Extension,
    /// A passed type whose body does not parse exactly.
    Malformed,
}

/// Reads SSH wire fields, bounds-checked.
struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn u32(&mut self) -> Option<u32> {
        let (head, rest) = self.0.split_first_chunk::<4>()?;
        self.0 = rest;
        Some(u32::from_be_bytes(*head))
    }

    fn string(&mut self) -> Option<&'a [u8]> {
        let len = usize::try_from(self.u32()?).ok()?;
        if len > self.0.len() {
            return None;
        }
        let (s, rest) = self.0.split_at(len);
        self.0 = rest;
        Some(s)
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// An SSH `string` (a four-byte big-endian length, then the bytes). A whole agent message on the
/// stream is the same form, so this also frames one.
pub fn ssh_string(s: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + s.len());
    put_string(&mut v, s);
    v
}

/// Appends an SSH `string` to `out`.
fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

/// The filter: whether a request from the remote may reach the agent. `msg` is the message
/// without its length (type byte first).
pub fn check_request(msg: &[u8]) -> Result<Request<'_>, Refusal> {
    let (&ty, body) = msg.split_first().ok_or(Refusal::Empty)?;
    let mut f = Fields(body);
    match ty {
        REQUEST_IDENTITIES => {
            if !f.is_empty() {
                return Err(Refusal::Malformed);
            }
            Ok(Request::Identities)
        }
        SIGN_REQUEST => {
            let key = f.string().ok_or(Refusal::Malformed)?;
            f.string().ok_or(Refusal::Malformed)?;
            f.u32().ok_or(Refusal::Malformed)?;
            if !f.is_empty() || key.is_empty() {
                return Err(Refusal::Malformed);
            }
            Ok(Request::Sign { key })
        }
        EXTENSION => {
            let name = f.string().ok_or(Refusal::Malformed)?;
            // What follows the name is the extension's own; the agent parses it.
            if ALLOWED_EXTENSIONS.iter().any(|a| a.as_bytes() == name) {
                Ok(Request::Extension)
            } else {
                Err(Refusal::Extension)
            }
        }
        other => Err(Refusal::Type(other)),
    }
}

/// Whether the agent's answer `msg` (without its length) answers `req`: the types the protocol
/// allows for it. A list answer must also be rebuilt through [`filter_identities`].
pub fn check_answer(req: &Request<'_>, msg: &[u8]) -> bool {
    let Some(&ty) = msg.first() else {
        return false;
    };
    match req {
        Request::Identities => ty == IDENTITIES_ANSWER || ty == FAILURE,
        Request::Sign { .. } => ty == SIGN_RESPONSE || ty == FAILURE,
        Request::Extension => matches!(
            ty,
            SUCCESS | FAILURE | EXTENSION_FAILURE | EXTENSION_RESPONSE
        ),
    }
}

/// One key of a list answer, as the relay shows and counts it.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyInfo {
    /// As `ssh-keygen -l` and `ssh-add -l` print it ([`fingerprint`]): a certificate by the
    /// plain key inside it.
    pub fingerprint: String,
    /// The key's type (`ssh-ed25519`), sanitized and at most [`MAX_SHOWN`] characters; for a
    /// certificate, the type of the key it certifies.
    pub key_type: String,
    /// An OpenSSH certificate (`*-cert-v01@openssh.com`).
    pub certificate: bool,
    /// The agent's comment, sanitized to one line of at most [`MAX_SHOWN`] characters.
    pub comment: String,
}

/// Names no key: fingerprints and comments are user data, and stay out of logs.
impl std::fmt::Debug for KeyInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeyInfo(..)")
    }
}

/// An `IDENTITIES_ANSWER` with only the keys `allow` admits, and those keys. `None` when `msg`
/// is not a well-formed list answer (it is then answered with `SSH_AGENT_FAILURE`). A
/// `FAILURE` answer is not a list; callers pass it on as it is.
pub fn filter_identities(
    msg: &[u8],
    allow: impl Fn(&str) -> bool,
) -> Option<(Vec<u8>, Vec<KeyInfo>)> {
    let mut kept: Vec<(&[u8], &[u8])> = Vec::new();
    let mut infos = Vec::new();
    for (blob, comment) in parse_identities(msg)? {
        let info = key_info(blob, comment);
        if allow(&info.fingerprint) {
            kept.push((blob, comment));
            infos.push(info);
        }
    }
    let mut out = Vec::with_capacity(msg.len());
    out.push(IDENTITIES_ANSWER);
    out.extend_from_slice(&(kept.len() as u32).to_be_bytes());
    for (blob, comment) in kept {
        put_string(&mut out, blob);
        put_string(&mut out, comment);
    }
    Some((out, infos))
}

/// The keys of an `IDENTITIES_ANSWER` (blob and comment, in the agent's order). `None` when it
/// is not a well-formed one.
pub fn parse_identities(msg: &[u8]) -> Option<Vec<(&[u8], &[u8])>> {
    let (&ty, body) = msg.split_first()?;
    if ty != IDENTITIES_ANSWER {
        return None;
    }
    let mut f = Fields(body);
    let n = f.u32()?;
    // Each key takes at least two lengths: a count beyond that is a lie, refused before any
    // allocation follows it.
    if u64::from(n) > (f.0.len() / 8) as u64 {
        return None;
    }
    let mut keys = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let blob = f.string()?;
        let comment = f.string()?;
        keys.push((blob, comment));
    }
    f.is_empty().then_some(keys)
}

/// What a prompt and a summary show of a key: its fingerprint, its type, whether it is a
/// certificate, and the comment, sanitized. Call it only on blobs the agent listed: the relay
/// never shows or counts a key by bytes the host sent.
pub fn key_info(blob: &[u8], comment: &[u8]) -> KeyInfo {
    let ty = Fields(blob).string().unwrap_or_default();
    let (key_type, certificate) = match cert_type(ty) {
        Some((plain, _)) => (plain.as_bytes(), true),
        None => (ty, ty.ends_with(CERT_SUFFIX.as_bytes())),
    };
    KeyInfo {
        fingerprint: fingerprint(blob),
        key_type: shown(key_type),
        certificate,
        comment: shown(comment),
    }
}

/// The suffix of OpenSSH certificate types.
const CERT_SUFFIX: &str = "-cert-v01@openssh.com";

/// An OpenSSH certificate type: the plain key type it certifies, and how many public fields
/// follow its nonce.
fn cert_type(ty: &[u8]) -> Option<(&'static str, usize)> {
    Some(match ty {
        b"ssh-ed25519-cert-v01@openssh.com" => ("ssh-ed25519", 1),
        b"ssh-rsa-cert-v01@openssh.com" => ("ssh-rsa", 2),
        b"ssh-dss-cert-v01@openssh.com" => ("ssh-dss", 4),
        b"ecdsa-sha2-nistp256-cert-v01@openssh.com" => ("ecdsa-sha2-nistp256", 2),
        b"ecdsa-sha2-nistp384-cert-v01@openssh.com" => ("ecdsa-sha2-nistp384", 2),
        b"ecdsa-sha2-nistp521-cert-v01@openssh.com" => ("ecdsa-sha2-nistp521", 2),
        b"sk-ssh-ed25519-cert-v01@openssh.com" => ("sk-ssh-ed25519@openssh.com", 2),
        b"sk-ecdsa-sha2-nistp256-cert-v01@openssh.com" => ("sk-ecdsa-sha2-nistp256@openssh.com", 3),
        _ => return None,
    })
}

/// The plain key a certificate blob certifies (its type, then the public fields after the
/// nonce), as OpenSSH fingerprints a certificate. `None` for a blob that is not a certificate of
/// a known type, or does not parse.
pub fn plain_key(blob: &[u8]) -> Option<Vec<u8>> {
    let mut f = Fields(blob);
    let (plain, n) = cert_type(f.string()?)?;
    f.string()?; // the nonce
    let mut out = Vec::with_capacity(blob.len());
    put_string(&mut out, plain.as_bytes());
    for _ in 0..n {
        put_string(&mut out, f.string()?);
    }
    Some(out)
}

/// Untrusted text as one short, inert line (`fil_proto::text::sanitize_inline`, at most
/// [`MAX_SHOWN`] characters).
fn shown(bytes: &[u8]) -> String {
    fil_proto::text::sanitize_inline(&bytes[..bytes.len().min(4 * MAX_SHOWN)])
        .chars()
        .take(MAX_SHOWN)
        .collect::<String>()
        .trim()
        .to_string()
}

/// The ssh fingerprint of a public key blob, as `ssh-keygen -l` and `ssh-add -l` print it:
/// `SHA256:` and the unpadded standard base64 of its SHA-256. A certificate is fingerprinted by
/// the plain key inside it ([`plain_key`]), as OpenSSH does; an unknown certificate type by its
/// whole blob.
pub fn fingerprint(blob: &[u8]) -> String {
    let plain = plain_key(blob);
    let d = ring::digest::digest(&ring::digest::SHA256, plain.as_deref().unwrap_or(blob));
    format!("SHA256:{}", base64_unpadded(d.as_ref()))
}

fn base64_unpadded(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Whether a message length read off the wire is one the relay takes: 1 to [`MAX_AGENT_MSG`].
pub fn length_ok(len: u32) -> bool {
    len >= 1 && (len as usize) <= MAX_AGENT_MSG
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(key: &[u8]) -> Vec<u8> {
        let mut m = vec![SIGN_REQUEST];
        m.extend(ssh_string(key));
        m.extend(ssh_string(b"data"));
        m.extend(0u32.to_be_bytes());
        m
    }

    fn ext(name: &str) -> Vec<u8> {
        let mut m = vec![EXTENSION];
        m.extend(ssh_string(name.as_bytes()));
        m
    }

    /// Every message type, 0 to 255: only 11, 13 and the two extensions pass.
    #[test]
    fn the_filter_table_over_every_type() {
        for ty in 0..=255u8 {
            // The well-formed body of each passed type; a single type byte for the rest.
            let msg = match ty {
                REQUEST_IDENTITIES => vec![ty],
                SIGN_REQUEST => sign(b"key"),
                EXTENSION => ext("query"),
                _ => vec![ty],
            };
            let got = check_request(&msg);
            match ty {
                REQUEST_IDENTITIES => assert_eq!(got, Ok(Request::Identities)),
                SIGN_REQUEST => assert_eq!(got, Ok(Request::Sign { key: b"key" })),
                EXTENSION => assert_eq!(got, Ok(Request::Extension)),
                _ => assert_eq!(got, Err(Refusal::Type(ty)), "type {ty}"),
            }
            // A body after a type that has none, or junk after any type: never passed as more.
            let mut longer = msg.clone();
            longer.push(0);
            if ty == EXTENSION {
                assert_eq!(check_request(&longer), Ok(Request::Extension));
            } else if ty == REQUEST_IDENTITIES || ty == SIGN_REQUEST {
                assert_eq!(check_request(&longer), Err(Refusal::Malformed));
            } else {
                assert_eq!(check_request(&longer), Err(Refusal::Type(ty)));
            }
        }
        // The ones the plan names, by name.
        for (ty, what) in [
            (1, "v1 list"),
            (7, "v1 add"),
            (9, "v1 remove all"),
            (17, "add"),
            (18, "remove"),
            (19, "remove all"),
            (20, "add smartcard"),
            (21, "remove smartcard"),
            (22, "lock"),
            (23, "unlock"),
            (25, "constrained add"),
            (26, "constrained smartcard add"),
        ] {
            assert_eq!(check_request(&[ty]), Err(Refusal::Type(ty)), "{what}");
        }
        assert_eq!(check_request(&[]), Err(Refusal::Empty));
    }

    #[test]
    fn extensions_other_than_the_two_are_refused() {
        assert_eq!(
            check_request(&ext("session-bind@openssh.com")),
            Ok(Request::Extension)
        );
        assert_eq!(check_request(&ext("query")), Ok(Request::Extension));
        for bad in [
            "add-smartcard@openssh.com",
            "restrict-destination-v00@openssh.com",
            "QUERY",
            "query\0",
            "",
        ] {
            assert_eq!(check_request(&ext(bad)), Err(Refusal::Extension), "{bad:?}");
        }
        // A name that runs past the message.
        assert_eq!(
            check_request(&[EXTENSION, 0, 0, 0, 9, b'q']),
            Err(Refusal::Malformed)
        );
    }

    #[test]
    fn a_sign_request_must_parse_exactly() {
        let good = sign(b"key");
        for cut in 1..good.len() {
            assert_eq!(
                check_request(&good[..cut]),
                Err(Refusal::Malformed),
                "cut at {cut}"
            );
        }
        // An empty key blob.
        assert_eq!(check_request(&sign(b"")), Err(Refusal::Malformed));
        // A length that claims more than there is (and one that overflows on 32-bit sizes).
        assert_eq!(
            check_request(&[SIGN_REQUEST, 0xff, 0xff, 0xff, 0xff, 1]),
            Err(Refusal::Malformed)
        );
    }

    fn answer(keys: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut m = vec![IDENTITIES_ANSWER];
        m.extend((keys.len() as u32).to_be_bytes());
        for (b, c) in keys {
            m.extend(ssh_string(b));
            m.extend(ssh_string(c));
        }
        m
    }

    fn blob(ty: &str, key: &[u8]) -> Vec<u8> {
        let mut b = ssh_string(ty.as_bytes());
        b.extend(ssh_string(key));
        b
    }

    /// A certificate of each known type is fingerprinted by the plain key inside it.
    #[test]
    fn certificates_are_fingerprinted_by_their_plain_key() {
        for (cert, plain, fields) in [
            ("ssh-ed25519-cert-v01@openssh.com", "ssh-ed25519", 1),
            ("ssh-rsa-cert-v01@openssh.com", "ssh-rsa", 2),
            ("ssh-dss-cert-v01@openssh.com", "ssh-dss", 4),
            (
                "ecdsa-sha2-nistp256-cert-v01@openssh.com",
                "ecdsa-sha2-nistp256",
                2,
            ),
            (
                "ecdsa-sha2-nistp521-cert-v01@openssh.com",
                "ecdsa-sha2-nistp521",
                2,
            ),
            (
                "sk-ssh-ed25519-cert-v01@openssh.com",
                "sk-ssh-ed25519@openssh.com",
                2,
            ),
            (
                "sk-ecdsa-sha2-nistp256-cert-v01@openssh.com",
                "sk-ecdsa-sha2-nistp256@openssh.com",
                3,
            ),
        ] {
            let public: Vec<Vec<u8>> = (0..fields).map(|i| vec![i as u8 + 1; 7 + i]).collect();
            let mut p = ssh_string(plain.as_bytes());
            let mut c = ssh_string(cert.as_bytes());
            c.extend(ssh_string(b"nonce-nonce"));
            for f in &public {
                p.extend(ssh_string(f));
                c.extend(ssh_string(f));
            }
            // What follows the public fields (serial, principals, the CA's signature...).
            c.extend(ssh_string(b"the rest of the certificate"));
            assert_eq!(fingerprint(&c), fingerprint(&p), "{cert}");
            let info = key_info(&c, b"me");
            assert!(info.certificate);
            assert_eq!(info.key_type, plain);
            assert!(!key_info(&p, b"").certificate);
        }
        // A short certificate falls back to its whole blob; an unknown one too.
        let short = [
            ssh_string(b"ssh-ed25519-cert-v01@openssh.com"),
            ssh_string(b"n"),
        ]
        .concat();
        assert_eq!(fingerprint(&short), {
            let d = ring::digest::digest(&ring::digest::SHA256, &short);
            format!("SHA256:{}", base64_unpadded(d.as_ref()))
        });
        let unknown = blob("x-cert-v01@openssh.com", b"k");
        assert!(key_info(&unknown, b"").certificate);
    }

    /// Against `ssh-keygen` itself, when it is installed: a throwaway key and a certificate of
    /// it, both made now, have the fingerprint `ssh-keygen -l` prints.
    #[test]
    fn fingerprints_match_ssh_keygen_for_a_certificate() {
        let dir = std::env::temp_dir().join(format!("fil-a1-cert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let keygen = |args: &[&str]| {
            std::process::Command::new("ssh-keygen")
                .args(args)
                .current_dir(&dir)
                .output()
        };
        let Ok(out) = keygen(&["-q", "-t", "ed25519", "-N", "", "-f", "user"]) else {
            eprintln!("no ssh-keygen: skipped");
            return;
        };
        assert!(out.status.success());
        assert!(
            keygen(&["-q", "-t", "ed25519", "-N", "", "-f", "ca"])
                .unwrap()
                .status
                .success()
        );
        let signed = keygen(&["-q", "-s", "ca", "-I", "test", "-n", "nobody", "user.pub"]).unwrap();
        assert!(signed.status.success(), "{signed:?}");
        let printed = |file: &str| {
            let o = keygen(&["-l", "-E", "sha256", "-f", file]).unwrap();
            String::from_utf8(o.stdout)
                .unwrap()
                .split(' ')
                .nth(1)
                .unwrap()
                .to_string()
        };
        let blob_of = |file: &str| {
            let text = std::fs::read_to_string(dir.join(file)).unwrap();
            base64_decode(text.split(' ').nth(1).unwrap())
        };
        let (plain, cert) = (blob_of("user.pub"), blob_of("user-cert.pub"));
        assert_eq!(fingerprint(&plain), printed("user.pub"));
        assert_eq!(fingerprint(&cert), printed("user-cert.pub"));
        assert_eq!(fingerprint(&cert), fingerprint(&plain));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn base64_decode(s: &str) -> Vec<u8> {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut bits = 0u32;
        let mut n = 0;
        let mut out = Vec::new();
        for c in s.trim().bytes().take_while(|&c| c != b'=') {
            let v = A.iter().position(|&a| a == c).unwrap() as u32;
            bits = (bits << 6) | v;
            n += 6;
            if n >= 8 {
                n -= 8;
                out.push((bits >> n) as u8);
            }
        }
        out
    }

    #[test]
    fn the_list_answer_is_filtered_by_fingerprint() {
        let a = blob("ssh-ed25519", &[1; 32]);
        let b = blob("ssh-ed25519", &[2; 32]);
        let msg = answer(&[(&a, b"first"), (&b, b"second")]);
        let fa = fingerprint(&a);
        let (out, infos) = filter_identities(&msg, |f| f == fa).unwrap();
        assert_eq!(out, answer(&[(&a, b"first")]));
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].fingerprint, fa);
        assert_eq!(infos[0].key_type, "ssh-ed25519");
        assert_eq!(infos[0].comment, "first");
        // Everything allowed: the same bytes back.
        let (all, _) = filter_identities(&msg, |_| true).unwrap();
        assert_eq!(all, msg);
        // Nothing allowed: an empty list.
        let (none, _) = filter_identities(&msg, |_| false).unwrap();
        assert_eq!(none, answer(&[]));
    }

    #[test]
    fn a_bad_list_answer_is_refused() {
        let a = blob("ssh-ed25519", &[1; 32]);
        let good = answer(&[(&a, b"c")]);
        for cut in 1..good.len() {
            assert!(filter_identities(&good[..cut], |_| true).is_none(), "{cut}");
        }
        let mut longer = good.clone();
        longer.push(0);
        assert!(filter_identities(&longer, |_| true).is_none());
        // A count far beyond the bytes is refused before anything is allocated for it.
        assert!(
            filter_identities(&[IDENTITIES_ANSWER, 0xff, 0xff, 0xff, 0xff], |_| true).is_none()
        );
        assert!(filter_identities(&[FAILURE], |_| true).is_none());
    }

    #[test]
    fn answers_must_answer_the_request() {
        let s = sign(b"k");
        let sreq = check_request(&s).unwrap();
        assert!(check_answer(&sreq, &[SIGN_RESPONSE, 0]));
        assert!(check_answer(&sreq, &[FAILURE]));
        assert!(!check_answer(&sreq, &[IDENTITIES_ANSWER]));
        assert!(!check_answer(&sreq, &[SUCCESS]));
        assert!(!check_answer(&sreq, &[]));
        assert!(check_answer(&Request::Identities, &[IDENTITIES_ANSWER]));
        assert!(!check_answer(&Request::Identities, &[SIGN_RESPONSE]));
        assert!(check_answer(&Request::Extension, &[SUCCESS]));
        assert!(!check_answer(&Request::Extension, &[SIGN_RESPONSE]));
    }

    #[test]
    fn comments_and_types_are_inert_and_short() {
        let b = blob("ssh-ed25519\x1b[2J", &[1; 32]);
        let info = key_info(&b, "me@laptop\x1b]0;owned\x07\nnext\u{202e}".as_bytes());
        assert_eq!(info.key_type, "ssh-ed25519[2J");
        assert!(!info.comment.chars().any(|c| c.is_control()));
        assert!(!info.comment.contains('\u{202e}'));
        let long = key_info(&b, &[b'x'; 10_000]);
        assert_eq!(long.comment.chars().count(), MAX_SHOWN);
        // A blob that is not one: no type, still a fingerprint.
        let odd = key_info(&[0xff], b"");
        assert_eq!(odd.key_type, "");
        assert!(odd.fingerprint.starts_with("SHA256:"));
    }

    #[test]
    fn fingerprints_are_ssh_keygens() {
        // `printf '' | sha256sum`, in ssh's base64: 43 characters, no padding.
        assert_eq!(
            fingerprint(b""),
            "SHA256:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU"
        );
        assert_eq!(base64_unpadded(b"f"), "Zg");
        assert_eq!(base64_unpadded(b"fo"), "Zm8");
        assert_eq!(base64_unpadded(b"foo"), "Zm9v");
        assert_eq!(base64_unpadded(b"foob"), "Zm9vYg");
    }

    #[test]
    fn lengths() {
        assert!(!length_ok(0));
        assert!(length_ok(1));
        assert!(length_ok(MAX_AGENT_MSG as u32));
        assert!(!length_ok(MAX_AGENT_MSG as u32 + 1));
        assert!(!length_ok(u32::MAX));
    }

    /// Random bytes never panic, in requests or answers.
    #[test]
    fn random_bytes_never_panic() {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..20_000 {
            let len = (next() % 64) as usize;
            let mut m: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            // Bias toward the parsed types, so their parsers get the random bodies.
            if let Some(first) = m.first_mut() {
                *first = [
                    SIGN_REQUEST,
                    EXTENSION,
                    REQUEST_IDENTITIES,
                    IDENTITIES_ANSWER,
                ][(next() % 4) as usize];
            }
            let r = check_request(&m);
            if let Ok(req) = r {
                let _ = check_answer(&req, &m);
            }
            let _ = filter_identities(&m, |_| true);
            let _ = key_info(&m, &m);
        }
    }
}
