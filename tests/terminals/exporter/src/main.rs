//! `terminals-export OUT_DIR [NAME...]` writes one `NAME.json` trace per scenario for
//! `tests/terminals/run.sh`; `terminals-export --list` prints the names. See
//! docs/m3/terminals.md.

mod overlay;
mod overlay_scenarios;
mod scenarios;
mod trace;

use std::path::Path;

use serde_json::Value;

use trace::Tb;

/// Every trace, with the size the terminal starts at.
pub fn all() -> Vec<(Tb, u16, u16)> {
    let mut out = Vec::new();
    for r in overlay_scenarios::runs() {
        for sync in [true, false] {
            let o = overlay::run(&r, sync);
            // The status lines and the typing session must draw something; a hazard trace
            // often shows only that the guard held back what the fixed predictor wanted.
            assert!(
                o.painted > 0 || r.name.starts_with("ov-"),
                "{}: nothing was ever painted, so the trace proves nothing",
                r.name
            );
            assert!(
                o.painted > 0 || r.name != "ov-typing",
                "ov-typing: nothing was ever painted"
            );
            out.push((o.tb, r.size.cols, r.size.rows));
        }
    }
    out.push((scenarios::canary(), 80, 24));
    for tb in scenarios::snapshots()
        .into_iter()
        .chain(scenarios::clears())
        .chain(scenarios::assumptions())
        .chain(scenarios::widths())
        .chain(scenarios::reflows())
    {
        let (c, r) = (tb.start.cols, tb.start.rows);
        out.push((tb, c, r));
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--list") {
        for (tb, _, _) in all() {
            println!("{}", tb.name);
        }
        return;
    }
    let Some(dir) = args.first() else {
        eprintln!("usage: terminals-export OUT_DIR [NAME...] | --list");
        std::process::exit(2);
    };
    std::fs::create_dir_all(dir).expect("create the output directory");
    let only = &args[1..];
    let mut n = 0;
    for (tb, cols, rows) in all() {
        if !only.is_empty() && !only.iter().any(|o| tb.name.contains(o.as_str())) {
            continue;
        }
        let j: Value = tb.to_json(cols, rows);
        let path = Path::new(dir).join(format!("{}.json", tb.name));
        std::fs::write(&path, serde_json::to_string(&j).expect("serialize")).expect("write");
        n += 1;
    }
    println!("wrote {n} traces to {dir}");
}

#[cfg(test)]
mod selfcheck {
    //! The harness's self-check (docs/m3/plan.md section 3, T2): every exported trace, replayed
    //! into a fresh `alacritty_terminal` exactly as a real terminal is fed (the synchronized-output
    //! markers included), must show what the trace says at every checkpoint.

    use fil_predict::oracle::Oracle;
    use fil_term::Size;
    use serde_json::Value;

    use crate::trace::{Check, HISTORY, Hist, expectation, model_backend};

    fn check_of(step: &Value) -> Check {
        let e = &step["expect"];
        let mut c = Check::new(step["name"].as_str().unwrap_or_default());
        c.screen = e.get("screen").is_some();
        c.rows = e.get("rows").map(|r| {
            r.as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_u64().unwrap() as u16)
                .collect()
        });
        c.cursor = e.get("cursor").is_some();
        c.attrs = match step["attrs"].as_str().unwrap_or_default() {
            "" => "",
            "u" => "u",
            "ur" => "ur",
            "bfgu" => "bfgu",
            "bdiursfg" => "bdiursfg",
            other => panic!("unknown attrs {other}"),
        };
        c.hist = if e.get("history").is_some() {
            Hist::Exact
        } else if e.get("history_count").is_some() {
            Hist::Count
        } else {
            Hist::No
        };
        c.content = e.get("content").is_some();
        c
    }

    #[test]
    fn every_trace_replays_into_alacritty_with_no_difference() {
        let mut checks = 0;
        let mut skipped = Vec::new();
        for (tb, cols, rows) in crate::all() {
            if tb.group == "canary" {
                continue; // wrong on purpose
            }
            let j = tb.to_json(cols, rows);
            let mut m = Oracle::new(&model_backend(), Size { cols, rows }, HISTORY);
            for step in j["steps"].as_array().unwrap() {
                match step["op"].as_str().unwrap() {
                    "write" => {
                        let h = step["hex"].as_str().unwrap();
                        let bytes: Vec<u8> = (0..h.len() / 2)
                            .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
                            .collect();
                        m.write(&bytes);
                    }
                    "resize" => m.resize(Size {
                        cols: step["cols"].as_u64().unwrap() as u16,
                        rows: step["rows"].as_u64().unwrap() as u16,
                    }),
                    "check" => {
                        if step["info"] == true {
                            skipped.push(format!("{}/{}", tb.name, step["name"]));
                            continue;
                        }
                        checks += 1;
                        let got = expectation(&m.state(), &check_of(step));
                        assert_eq!(
                            got, step["expect"],
                            "{}/{}: alacritty, fed the trace's bytes, differs from the checkpoint",
                            tb.name, step["name"]
                        );
                    }
                    other => panic!("unknown step {other}"),
                }
            }
        }
        eprintln!("{checks} checkpoints replayed; informational, not replayed: {skipped:?}");
        assert!(checks > 100);
    }
}
