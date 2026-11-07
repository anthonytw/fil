//! What `gen_docs` writes and `tests/docs_sync.rs` checks: the `fil(1)` man page and the shell
//! completions, from the clap definitions in `fil::cli`. One place, so the committed man page
//! and the generator cannot drift apart without the test saying so.

use clap::CommandFactory;
use clap_complete::Shell;

/// The command, as the generated files describe it: without the version text. The version is
/// not in a committed file, because the wire revision in it changes with the protocol and the
/// package version with every release; `fil --version` is where it belongs.
fn unbuilt() -> clap::Command {
    fil::cli::Cli::command()
        .version(None::<&str>)
        .long_version(None::<&str>)
}

fn command() -> clap::Command {
    let mut cmd = unbuilt();
    cmd.build();
    cmd
}

/// roff text: backslashes, hyphens and a leading dot or quote made safe.
fn roff(text: &str) -> String {
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let line = line
            .replace('\\', "\\e")
            .replace('-', "\\-")
            .replace('\'', "\\*(Aq")
            .replace('`', "");
        if line.starts_with('.') || line.starts_with("\\*(Aq") {
            out.push_str("\\&");
        }
        out.push_str(&line);
    }
    out
}

/// One argument as a `.TP` entry: its flags, then its help.
fn argument(arg: &clap::Arg, out: &mut String) {
    let mut names = Vec::new();
    let values = arg
        .get_value_names()
        .filter(|_| arg.get_action().takes_values())
        .map(|v| {
            v.iter()
                .map(|n| format!(" \\fI{}\\fR", roff(n)))
                .collect::<String>()
        })
        .unwrap_or_default();
    if arg.is_positional() {
        names.push(format!(
            "\\fI{}\\fR",
            roff(arg.get_id().as_str().to_uppercase().as_str())
        ));
    }
    if let Some(c) = arg.get_short() {
        names.push(format!("\\fB\\-{c}\\fR{values}"));
    }
    if let Some(l) = arg.get_long() {
        names.push(format!("\\fB\\-\\-{}\\fR{values}", roff(l)));
    }
    out.push_str(&format!(".TP\n{}\n", names.join(", ")));
    if let Some(help) = arg.get_help() {
        out.push_str(&roff(&help.to_string()));
        out.push('\n');
    }
}

fn visible_arguments(cmd: &clap::Command) -> impl Iterator<Item = &clap::Arg> {
    cmd.get_arguments()
        .filter(|a| !a.is_hide_set() && !a.is_global_set() && a.get_id() != "help")
}

/// One subcommand as a `.SS` section, and the ones below it (`fil config show`) after it.
fn subcommand(sub: &clap::Command, parent: &str, out: &mut String) {
    let name = sub.get_name();
    let path = format!("{parent} {name}");
    out.push_str(&format!(".SS \"{}\"\n", roff(&path)));
    if let Some(about) = sub.get_about() {
        out.push_str(&roff(&about.to_string()));
        out.push_str("\n.PP\n");
    }
    let mut usage = sub.clone().disable_help_flag(true);
    usage.build();
    // `fil server` has options only the program itself uses, which clap's usage line lists.
    let line = if name == "server" {
        "fil server --stdio".to_string()
    } else {
        usage.render_usage().to_string().replace("Usage: ", "")
    };
    out.push_str(&format!("Usage: \\fB{}\\fR\n", roff(&line)));
    for arg in visible_arguments(sub) {
        // A positional with no help text would be an empty entry.
        if arg.is_positional() && arg.get_help().is_none() {
            continue;
        }
        argument(arg, out);
    }
    for below in sub.get_subcommands().filter(|s| !s.is_hide_set()) {
        subcommand(below, &path, out);
    }
}

const SYNOPSIS: &str = ".SH SYNOPSIS
\\fBfil\\fR [\\fB\\-L\\fR \\fISPEC\\fR]... [\\fB\\-R\\fR \\fISPEC\\fR]... [\\fB\\-D\\fR \\fISPEC\\fR]... [\\fB\\-A\\fR|\\fB\\-a\\fR] [\\fB\\-\\-transport\\fR \\fIMODE\\fR] [\\fB\\-\\-config\\fR \\fIFILE\\fR] \\fIHOST\\fR [\\fISESSION\\fR]
.br
\\fBfil\\fR \\fICOMMAND\\fR ...
";

/// `fil(1)`: one page for the whole command line. The attach form and its options first, then
/// one section per subcommand (there is no page per subcommand: an archive carries `fil.1` alone).
pub fn man_page() -> Vec<u8> {
    let mut cmd = command();
    let notes = cmd.get_after_help().map(|h| h.to_string());
    cmd = cmd.after_help(None::<&str>).disable_help_flag(true);
    cmd.build();
    let mut out: Vec<u8> = Vec::new();
    let man = clap_mangen::Man::new(cmd.clone()).source("fil");
    // The .TH line by hand: clap_mangen's has no date, and roff then reads the source as the date.
    out.extend_from_slice(
        b".ie \\n(.g .ds Aq \\(aq\n.el .ds Aq '\n.TH \"FIL\" \"1\" \"\" \"fil\" \"fil manual\"\n",
    );
    // clap_mangen repeats the `Aq` preamble in every section (the .TH line above has it once),
    // and its synopsis drops the option values, so that section is written by hand.
    let mut sections: Vec<u8> = Vec::new();
    man.render_name_section(&mut sections).expect("memory");
    sections.extend_from_slice(SYNOPSIS.as_bytes());
    man.render_description_section(&mut sections)
        .expect("memory");
    man.render_options_section(&mut sections).expect("memory");
    for line in String::from_utf8(sections)
        .expect("utf-8")
        .split_inclusive('\n')
    {
        if !line.starts_with(".ie \\n(.g .ds Aq") && !line.starts_with(".el .ds Aq") {
            out.extend_from_slice(line.as_bytes());
        }
    }

    let mut rest = String::from(".SH COMMANDS\n");
    for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
        subcommand(sub, "fil", &mut rest);
    }
    out.extend_from_slice(rest.as_bytes());

    if let Some(notes) = notes {
        out.extend_from_slice(b".SH NOTES\n");
        for para in notes.split("\n\n") {
            if para.starts_with("Examples:") {
                out.extend_from_slice(format!(".nf\n{}\n.fi\n", roff(para)).as_bytes());
            } else {
                out.extend_from_slice(format!(".PP\n{}\n", roff(para)).as_bytes());
            }
        }
    }
    out.extend_from_slice(b".SH \"SEE ALSO\"\n\\fBfil\\-config\\fR(5), \\fBfil\\-policy\\fR(5)\n");
    out
}

/// The completion script of one shell.
pub fn completion(shell: Shell) -> Vec<u8> {
    let mut out = Vec::new();
    // `fil server` is for the program itself. clap_complete ignores `hide` on subcommands, so the
    // subcommand is left out: the root is rebuilt from its own name, about and arguments, with
    // every subcommand but `server`.
    let full = unbuilt();
    let mut cmd = clap::Command::new("fil").disable_help_subcommand(true);
    if let Some(about) = full.get_about() {
        cmd = cmd.about(about.clone());
    }
    cmd = cmd.args(full.get_arguments().cloned()).subcommands(
        full.get_subcommands()
            .filter(|s| s.get_name() != "server")
            .cloned(),
    );
    cmd.build();
    clap_complete::generate(shell, &mut cmd, "fil", &mut out);
    out
}

/// The shells that get a script, with the path each one's script has in an archive
/// (`docs/m4/release.md` section 4.3).
pub const COMPLETIONS: [(Shell, &str); 3] = [
    (Shell::Bash, "share/bash-completion/completions/fil"),
    (Shell::Zsh, "share/zsh/site-functions/_fil"),
    (Shell::Fish, "share/fish/vendor_completions.d/fil.fish"),
];
