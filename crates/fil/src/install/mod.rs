//! Getting fil onto a remote host (`docs/m4/plan.md` §3 U1; `docs/m4/release.md` §4.3 to §6;
//! the user-facing steps are in `docs/m4/install.md`).
//!
//! **By hand, always:** copy a release's `bin/fil` to the host. When `fil server --stdio` cannot
//! find it, the first attach's error says where fil looks and what to do (`connect::failure`).
//!
//! **Through fil, gated on M4 OQ-1** (the owner's approval of a second fixed remote command, the
//! constant bootstrap of `fil_transport::pipe::PipeCommand::BOOTSTRAP`, and of the install
//! directory `~/.local/share/fil`). What is here regardless, because it runs nothing new on a
//! remote host:
//!
//! - [`layout`]: the install root, the FROZEN version-directory name pattern, the directory
//!   rules the bootstrap applies, checked again in Rust;
//! - [`probe`]: the three-state lock probe;
//! - [`gc`]: the one selection function of the FROZEN `--install-gc` contract, plain and forced;
//! - [`remote`]: `fil server --install-finish` and `fil server --install-gc`, which only the
//!   bootstrap runs;
//! - [`upload`]: which binary would be sent, and the checks before a byte leaves (the bundle's
//!   `SHA256SUMS`, the release-build mark, the scan for test hooks and builder paths);
//! - [`fallback`]: which remote command each connection of a run would use (`remote = ...`).
//!
//! Only with the `install-bootstrap` feature (off by default; no release build has it):
//! [`boot`], the client's half of the bootstrap's exchange, and [`client`], `fil install`.

pub mod fallback;
pub mod gc;
pub mod layout;
pub mod probe;
pub mod remote;
pub mod upload;

#[cfg(feature = "install-bootstrap")]
pub mod boot;
#[cfg(feature = "install-bootstrap")]
pub mod client;
