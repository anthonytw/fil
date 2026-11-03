//! A fake terminal: input events from the test, output into an emulator that plays the user's
//! terminal and is the screen oracle.

use std::io;
use std::sync::{Arc, Mutex};

use fil_client::{TermEvent, TermInput, TermOutput, Terminal};
use fil_term::{Emulator, Size, TermState};
use tokio::sync::mpsc;

pub struct TermShared {
    pub emu: Box<dyn Emulator>,
    pub written: Vec<u8>,
    pub raw: bool,
    pub raw_changes: Vec<bool>,
    pub size: (u16, u16),
    /// Every write, one entry per call, to check what went out together.
    pub writes: Vec<Vec<u8>>,
}

pub struct FakeIn(mpsc::UnboundedReceiver<TermEvent>);
pub struct FakeOut(Arc<Mutex<TermShared>>);

#[derive(Clone)]
pub struct TermHandle {
    tx: mpsc::UnboundedSender<TermEvent>,
    shared: Arc<Mutex<TermShared>>,
}

pub fn fake_terminal(
    cols: u16,
    rows: u16,
    local_term: &str,
) -> (Terminal<FakeIn, FakeOut>, TermHandle) {
    let (tx, rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Mutex::new(TermShared {
        emu: fil_term::create(fil_term::DEFAULT_BACKEND, Size { cols, rows }, 1000).unwrap(),
        written: Vec::new(),
        raw: false,
        raw_changes: Vec::new(),
        size: (cols, rows),
        writes: Vec::new(),
    }));
    let term = Terminal {
        input: FakeIn(rx),
        output: FakeOut(shared.clone()),
        local_term: Some(local_term.to_string()),
    };
    (term, TermHandle { tx, shared })
}

impl TermInput for FakeIn {
    async fn next(&mut self) -> TermEvent {
        self.0.recv().await.unwrap_or(TermEvent::Eof)
    }
}

impl TermOutput for FakeOut {
    fn size(&mut self) -> io::Result<(u16, u16)> {
        Ok(self.0.lock().unwrap().size)
    }
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut s = self.0.lock().unwrap();
        s.emu.feed(bytes);
        s.written.extend_from_slice(bytes);
        s.writes.push(bytes.to_vec());
        Ok(())
    }
    fn set_raw(&mut self, raw: bool) -> io::Result<()> {
        let mut s = self.0.lock().unwrap();
        s.raw = raw;
        s.raw_changes.push(raw);
        Ok(())
    }
}

impl TermHandle {
    pub fn type_bytes(&self, b: &[u8]) {
        let _ = self.tx.send(TermEvent::Input(b.to_vec()));
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        {
            let mut s = self.shared.lock().unwrap();
            s.size = (cols, rows);
            s.emu.resize(Size { cols, rows });
        }
        let _ = self.tx.send(TermEvent::Resize { cols, rows });
    }

    /// The window changes size, but its event is not delivered (yet): the SIGWINCH that is still
    /// waiting for a runtime thread while the keys already typed behind it have been read.
    pub fn resize_quietly(&self, cols: u16, rows: u16) {
        let mut s = self.shared.lock().unwrap();
        s.size = (cols, rows);
        s.emu.resize(Size { cols, rows });
    }

    /// Delivers a resize event as given, without touching the window: a late one.
    pub fn resize_event(&self, cols: u16, rows: u16) {
        let _ = self.tx.send(TermEvent::Resize { cols, rows });
    }

    pub fn eof(&self) {
        let _ = self.tx.send(TermEvent::Eof);
    }

    pub fn signal(&self, n: i32) {
        let _ = self.tx.send(TermEvent::Signal(n));
    }

    /// The user's screen and scrollback.
    pub fn state(&self) -> TermState {
        self.shared.lock().unwrap().emu.state(1000)
    }

    pub fn is_raw(&self) -> bool {
        self.shared.lock().unwrap().raw
    }

    pub fn raw_changes(&self) -> Vec<bool> {
        self.shared.lock().unwrap().raw_changes.clone()
    }

    pub fn written(&self) -> Vec<u8> {
        self.shared.lock().unwrap().written.clone()
    }

    /// The visible rows as text, trailing spaces trimmed.
    pub fn rows(&self) -> Vec<String> {
        self.state().screen_text()
    }

    /// Screen text joined, for `contains` checks. Lines that wrapped are joined without a break.
    pub fn text(&self) -> String {
        let st = self.state();
        let mut out = String::new();
        for l in &st.screen {
            let t = if l.wrapped {
                l.cells
                    .iter()
                    .map(|c| {
                        if c.text.is_empty() {
                            " "
                        } else {
                            c.text.as_str()
                        }
                    })
                    .collect::<String>()
            } else {
                l.text()
            };
            out.push_str(&t);
            if !l.wrapped {
                out.push('\n');
            }
        }
        out
    }

    /// Scrollback lines as text.
    pub fn history(&self) -> Vec<String> {
        self.state()
            .history
            .iter()
            .map(fil_term::model::Line::text)
            .collect()
    }
}
