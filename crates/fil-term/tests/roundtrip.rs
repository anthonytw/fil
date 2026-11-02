//! Quick restore-fidelity gate for `just ci`: every fixture, every compiled-in backend, a
//! round trip at the original size and after a resize narrower and wider. A field a backend
//! cannot report is skipped; a supported field that differs fails the test.
//!
//! The full harness (cross-backend, replies, performance, the continuation probe) is
//! `just fidelity`.

use std::path::PathBuf;

use fil_term::compare::{Verdict, compare};
use fil_term::snapshot::snapshot;
use fil_term::{Size, backends, create};

fn fixtures() -> Vec<(String, Size, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_none_or(|x| x != "raw") {
            continue;
        }
        let meta = std::fs::read_to_string(p.with_extension("meta")).unwrap();
        let get = |k: &str| -> u16 {
            meta.lines()
                .find_map(|l| l.strip_prefix(&format!("{k} = ")))
                .unwrap()
                .parse()
                .unwrap()
        };
        out.push((
            p.file_stem().unwrap().to_string_lossy().into_owned(),
            Size {
                cols: get("cols"),
                rows: get("rows"),
            },
            std::fs::read(&p).unwrap(),
        ));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn snapshots_restore_every_supported_field() {
    let mut failures = Vec::new();
    for b in backends() {
        for (name, size, bytes) in fixtures() {
            let narrow = Size {
                cols: (size.cols * 2 / 3).max(4),
                rows: size.rows,
            };
            let wide = Size {
                cols: size.cols * 3 / 2,
                rows: size.rows,
            };
            for target in [size, narrow, wide] {
                let mut e = (b.create)(size, 1000);
                e.feed(&bytes);
                if target != size {
                    e.resize(target);
                }
                let s1 = e.state(1000);
                let snap = snapshot(&s1);
                let mut e2 = (b.create)(target, 1000);
                e2.feed(&snap);
                let s2 = e2.state(1000);
                for r in compare(&s1, &s2, &b.capabilities) {
                    if let Verdict::Fail(d) = r.verdict {
                        failures.push(format!(
                            "{} / {name} / {}x{}: {}: {d}",
                            b.name, target.cols, target.rows, r.field
                        ));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn registry_creates_every_backend_by_name() {
    for b in backends() {
        let mut e = create(b.name, Size { cols: 20, rows: 3 }, 10).unwrap();
        e.feed(b"hello\r\n");
        assert_eq!(e.backend(), b.name);
        assert_eq!(e.state(0).screen_text()[0], "hello");
    }
}
