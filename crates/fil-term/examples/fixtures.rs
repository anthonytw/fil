//! Writes the synthetic fixtures under `tests/fixtures/` (`just fixtures`).
//!
//! The streams are defined here, in source, so they can be reviewed as text. The generated
//! `.raw` files are committed, and the fidelity harness reads those. A unit test in
//! `tests/fixtures_current.rs` fails if a committed file drifts from this source.

#[path = "../tests/common/synthetic.rs"]
mod synthetic;

use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    for f in synthetic::all() {
        std::fs::write(dir.join(format!("{}.raw", f.name)), &f.bytes).unwrap();
        let meta = format!(
            "cols = {}\nrows = {}\nkind = synthetic\ndescription = {}\n",
            f.cols, f.rows, f.description
        );
        std::fs::write(dir.join(format!("{}.meta", f.name)), meta).unwrap();
        println!("{:<22} {:>6} bytes", f.name, f.bytes.len());
    }
}
