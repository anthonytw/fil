//! The committed synthetic `.raw` files must equal what `examples/fixtures.rs` generates
//! (run `just fixtures` after editing `tests/common/synthetic.rs`), and every fixture must
//! have a metadata file.

#[path = "common/synthetic.rs"]
mod synthetic;

use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn synthetic_fixtures_match_their_source() {
    for f in synthetic::all() {
        let on_disk = std::fs::read(dir().join(format!("{}.raw", f.name)))
            .unwrap_or_else(|_| panic!("{}.raw missing: run `just fixtures`", f.name));
        assert!(
            on_disk == f.bytes,
            "{}.raw is stale: run `just fixtures`",
            f.name
        );
    }
}

#[test]
fn every_raw_file_has_metadata() {
    for e in std::fs::read_dir(dir()).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "raw") {
            let meta = std::fs::read_to_string(p.with_extension("meta"))
                .unwrap_or_else(|_| panic!("{} has no .meta", p.display()));
            for key in ["cols = ", "rows = ", "kind = ", "description = "] {
                assert!(meta.contains(key), "{} lacks `{key}`", p.display());
            }
        }
    }
}
