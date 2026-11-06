//! Writes the generated documentation files: the `fil(1)` man page and the completions for bash,
//! zsh and fish, laid out like an archive (`docs/m4/release.md` section 4.3).
//!
//! ```text
//! gen_docs --out DIR         write DIR/share/man/man1/fil.1 and the three completion scripts
//! gen_docs --man-page        print fil.1 on stdout (`just docs-man` rewrites the committed copy)
//! ```
//!
//! The pages `fil-config(5)` and `fil-policy(5)` are not made here: they come from
//! `docs/user/config.md` and `policy.md` by pandoc (`just docs`).

use std::path::PathBuf;

#[path = "support/docgen.rs"]
mod docgen;

fn main() {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next(), args.next()) {
        (Some("--man-page"), None, None) => {
            use std::io::Write;
            std::io::stdout()
                .write_all(&docgen::man_page())
                .expect("write to stdout");
        }
        (Some("--out"), Some(dir), None) => {
            let dir = PathBuf::from(dir);
            let mut files = vec![("share/man/man1/fil.1", docgen::man_page())];
            for (shell, path) in docgen::COMPLETIONS {
                files.push((path, docgen::completion(shell)));
            }
            for (path, bytes) in files {
                let to = dir.join(path);
                std::fs::create_dir_all(to.parent().expect("a file path has a parent"))
                    .unwrap_or_else(|e| die(&format!("{}: {e}", to.display())));
                std::fs::write(&to, bytes)
                    .unwrap_or_else(|e| die(&format!("{}: {e}", to.display())));
            }
        }
        _ => die("usage: gen_docs --out DIR | --man-page"),
    }
}

fn die(message: &str) -> ! {
    eprintln!("gen_docs: {message}");
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::docgen;

    #[test]
    fn the_man_page_and_the_completions_name_every_subcommand() {
        let man = String::from_utf8(docgen::man_page()).unwrap();
        assert!(man.starts_with(".ie \\n(.g .ds Aq"), "a roff file");
        for sub in ["ls", "kill", "detach", "forward", "ports", "install"] {
            assert!(
                man.contains(&format!(".SS \"fil {sub}\"")),
                "{sub} in the man page"
            );
        }
        for (shell, _) in docgen::COMPLETIONS {
            let script = String::from_utf8(docgen::completion(shell)).unwrap();
            assert!(script.contains("forward"), "{shell}: the subcommands");
            assert!(script.contains("transport"), "{shell}: the flags");
        }
    }
}
