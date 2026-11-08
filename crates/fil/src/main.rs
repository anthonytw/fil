fn main() -> std::process::ExitCode {
    // The daemon starts a session's child through its own binary, to give it the descriptor
    // limit the daemon had before it raised its own (`fil_session::nofile`). Before anything
    // else: a trampoline never parses a command line.
    fil_daemon::run_trampoline_if_asked();
    fil::app::main()
}
