//! What the daemon's log says at its default level (DESIGN.md section 11.1, principle 2): a
//! takeover between two of the user's machines leaves no trace of which machines they were.
//! Alone in this test binary: it installs the global subscriber.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use common::*;
use tracing_subscriber::EnvFilter;

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_default_log_names_no_host_label_and_no_session() {
    let sink = Sink::default();
    let writer = sink.clone();
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::new(fil_daemon::logging::filter_spec(None)))
        .with_writer(move || writer.clone())
        .init();

    let s = Scratch::new();
    let d = start(&s, config(CAT)).await;
    let mut a = Client::attach(
        d.pipe().await,
        Connect::new("project-x", key(1)).host("laptop-a"),
    )
    .await;
    a.wait_screen("ready").await;
    // A second machine takes the session over: the old one is told by whom; the log names neither
    // machine and not the session.
    let b = Client::attach(
        d.pipe().await,
        Connect::new("project-x", key(2)).host("tablet-b"),
    )
    .await;
    assert!(b.reply.is_some());
    a.wait("the takeover notice", |c| c.taken_over.is_some())
        .await;
    assert_eq!(
        a.taken_over.as_ref().unwrap().by.as_str(),
        "tablet-b",
        "the user's other client is told"
    );
    d.stop().await;

    let log = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("\"attach\""), "the attach was logged: {log}");
    for label in ["laptop-a", "tablet-b", "project-x"] {
        assert!(!log.contains(label), "{label} is in the log:\n{log}");
    }
}
