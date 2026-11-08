//! The descriptor-limit trampoline (`fil_session::nofile`) on its own, for this crate's tests:
//! a test process cannot be one itself, because its `main` is the test harness's. Built only with
//! the `test-hooks` feature; the daemon's binaries are their own trampoline.

fn main() {
    fil_session::nofile::run_trampoline_if_asked();
    eprintln!("not started as a trampoline");
    std::process::exit(2);
}
