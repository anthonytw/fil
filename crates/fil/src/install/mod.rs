//! Getting fil onto a remote host (`docs/m4/plan.md` §3 U1; `docs/m4/release.md` §4.3 to §6;
//! the user-facing steps are in `docs/m4/install.md`).
//!
//! **By hand, always:** copy a release's `bin/fil` to the host. When `fil server --stdio` cannot
//! find it, the first attach's error says where fil looks and what to do (`connect::failure`).
//!
//! **Through fil** (M4 OQ-1: the owner approved, 2026-11-04, the second fixed remote command,
//! the constant bootstrap of `fil_transport::pipe::PipeCommand::BOOTSTRAP`, and the install
//! directory `~/.local/share/fil`):
//!
//! - [`layout`]: the install root, the FROZEN version-directory name pattern, the directory
//!   rules the bootstrap applies, checked again in Rust;
//! - [`probe`]: the three-state lock probe;
//! - [`gc`]: the one selection function of the FROZEN `--install-gc` contract, plain and forced;
//! - [`remote`]: `fil server --install-finish` and `fil server --install-gc`, which only the
//!   bootstrap runs;
//! - [`upload`]: which binary would be sent, and the checks before a byte leaves (the bundle's
//!   `SHA256SUMS`, the release-build mark, the scan for test hooks and builder paths);
//! - [`fallback`]: which remote command each connection of a run uses (`remote = ...`);
//! - [`boot`]: the client's half of the bootstrap's exchange;
//! - [`attach`]: the bootstrap's greeting inside a run (the offer, the upload, `go`);
//! - [`client`]: `fil install`.

pub mod attach;
pub mod boot;
pub mod client;
pub mod fallback;
pub mod gc;
pub mod layout;
pub mod probe;
pub mod remote;
pub mod upload;
