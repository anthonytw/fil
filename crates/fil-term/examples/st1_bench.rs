use std::time::Instant;
fn main() {
    let mut data = Vec::new();
    for i in 0..200_000u32 {
        data.extend_from_slice(
            format!(
                "\x1b[3{}mline {} of some plain text that is typical output\x1b[0m\r\n",
                i % 8,
                i
            )
            .as_bytes(),
        );
    }
    for name in ["alacritty", "vt100"] {
        let mut best = u128::MAX;
        for _ in 0..7 {
            let mut e = fil_term::create(
                name,
                fil_term::Size {
                    cols: 120,
                    rows: 40,
                },
                1000,
            )
            .unwrap();
            let t = Instant::now();
            for chunk in data.chunks(4096) {
                e.feed(chunk);
            }
            best = best.min(t.elapsed().as_millis());
        }
        println!("{name}: {} MB best of 7 {best} ms", data.len() / 1_000_000);
    }
}
