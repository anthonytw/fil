//! What the scenarios share: a shell with a scratch directory, commands with a marker, exact
//! sequence and file checks, vim helpers, the client's prompts, and stopping a process group.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::harness::{Client, ensure, pgid_of, signal_group};
use crate::{Ctx, Variant};

pub const ATTACH_TIMEOUT: Duration = Duration::from_secs(25);
pub const CMD_TIMEOUT: Duration = Duration::from_secs(30);

pub const UP: &str = "\x1b[A";
pub const CTRL_C: &str = "\x03";
pub const CTRL_D: &str = "\x04";
pub const CTRL_Z: &str = "\x1a";
pub const ESC: &str = "\x1b";

pub type Res = Result<Vec<String>, String>;

/// Not applicable here (for instance, it needs ssh as the transport). Fails the run on a test host
/// unless `--allow-skip` is given.
pub fn skip(why: &str) -> Res {
    Ok(vec![format!("SKIP: {why}")])
}

/// A tool the scenario needs is not installed. Fails the run unless `--allow-missing` is given.
pub fn missing(what: &str) -> Res {
    Ok(vec![format!("MISSING: {what}")])
}

/// Every unique marker in a scenario, so none is ever on the screen already.
static TAGS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn next_tag() -> u32 {
    TAGS.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

impl Client {
    /// Runs `cmd` in the shell and returns what it printed (and the echoed command line). The
    /// marker is `echo`ed after it, so a command that fails still returns.
    pub fn run(&mut self, cmd: &str, timeout: Duration) -> Result<String, String> {
        let n = next_tag();
        let from = self.len();
        self.send(&format!("{cmd}; echo DPM{n}-$((6*7))\r"));
        let marker = format!("DPM{n}-42");
        let (_, end) = self.want_from(&marker, from, timeout)?;
        Ok(self.with_plain(|p| String::from_utf8_lossy(&p[from..end - marker.len()]).into_owned()))
    }

    /// Whether `cmd` succeeds in the shell.
    pub fn test(&mut self, cmd: &str) -> Result<bool, String> {
        let n = next_tag();
        let from = self.len();
        self.send(&format!(
            "if {cmd}; then echo DPY{n}-$((6*7)); else echo DPN{n}-$((6*7)); fi\r"
        ));
        let yes = format!("DPY{n}-42");
        let no = format!("DPN{n}-42");
        let t0 = Instant::now();
        loop {
            let (y, nn) = self.with_plain(|p| {
                let s = String::from_utf8_lossy(&p[from.min(p.len())..]);
                (s.contains(&yes), s.contains(&no))
            });
            if y || nn {
                return Ok(y);
            }
            if t0.elapsed() > CMD_TIMEOUT || self.exit_code().is_some() {
                return Err(format!("no answer to `{cmd}`\n{}", self.dump()));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Whether the host has the tool.
    pub fn have(&mut self, tool: &str) -> Result<bool, String> {
        self.test(&format!("command -v {tool} > /dev/null 2>&1"))
    }

    /// The output must not hold the gap marker, unless this client is one that is allowed it (a
    /// flood). Catch-up is for big output; anywhere else it would be hiding a loss.
    pub fn clean_output(&self) -> Result<(), String> {
        ensure(self.gap_ok || !self.text().contains("fil: skipped"), || {
            format!(
                "a gap marker where nothing should be skipped\n{}",
                self.dump()
            )
        })
    }

    /// Rows of the screen that are not in `before`, trimmed.
    pub fn new_rows(&self, before: &[String]) -> Vec<String> {
        self.rows()
            .into_iter()
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty() && !before.iter().any(|b| b.trim() == r))
            .collect()
    }

    /// Waits for the screen's status line to go away.
    pub fn status_cleared(&self) -> Result<(), String> {
        self.wait_for(Duration::from_secs(15), |c| !c.screen().contains("fil:"))
            .map(|_| ())
            .ok_or_else(|| {
                format!(
                    "the status line is still on the screen after the recovery\n{}",
                    self.dump()
                )
            })
    }

    /// Waits for `needle` in the plain output after `from` (a mark taken before the action), and
    /// returns the new mark just after it.
    pub fn after(&self, needle: &str, from: usize) -> Result<usize, String> {
        self.want_from(needle, from, CMD_TIMEOUT).map(|(_, e)| e)
    }
}

/// A line of `out` that is exactly `want` (trimmed).
pub fn has_line(out: &str, want: &str) -> bool {
    out.lines().any(|l| l.trim() == want)
}

impl Ctx {
    /// A client with a shell that works, a neutral prompt (so no host name is ever on screen),
    /// a history that goes nowhere (on --local it would be the developer's own), and a scratch directory
    /// `$D` of its own under `/tmp/dp.*`.
    pub fn shell(&self, host: &str, variant: Variant, name: &str) -> Result<Client, String> {
        self.shell_sized(host, variant, name, 30, 100)
    }

    pub fn shell_sized(
        &self,
        host: &str,
        variant: Variant,
        name: &str,
        rows: u16,
        cols: u16,
    ) -> Result<Client, String> {
        self.shell_full(host, variant, name, rows, cols, &crate::Extra::default())
    }

    /// A shell whose client has options before the host (`-L`, `-R`, `-D`) and, with `via`, both
    /// of its paths through the cut relay (`netcut.rs`).
    pub fn shell_with(
        &self,
        host: &str,
        variant: Variant,
        name: &str,
        args: &[String],
        via: Option<&crate::netcut::Cut>,
    ) -> Result<Client, String> {
        self.shell_full(
            host,
            variant,
            name,
            30,
            100,
            &crate::Extra {
                args,
                via,
                ..Default::default()
            },
        )
    }

    /// A shell whose client has what `x` adds (environment, the agent setting, flags).
    pub fn shell_x(
        &self,
        host: &str,
        variant: Variant,
        name: &str,
        x: &crate::Extra<'_>,
    ) -> Result<Client, String> {
        self.shell_full(host, variant, name, 30, 100, x)
    }

    fn shell_full(
        &self,
        host: &str,
        variant: Variant,
        name: &str,
        rows: u16,
        cols: u16,
        x: &crate::Extra<'_>,
    ) -> Result<Client, String> {
        let mut c = self.client_with(host, variant, &format!("dp-{name}"), rows, cols, x);
        c.roundtrip("RDY", ATTACH_TIMEOUT)?;
        // What an earlier failure left in the test account's /tmp goes first; on this machine
        // nothing is swept (the scenarios remove their own directory).
        let sweep = if Ctx::is_local(host) {
            ""
        } else {
            "rm -rf /tmp/dp.*; "
        };
        c.run(
            &format!(
                "PS1='fil$ '; unset PROMPT_COMMAND; HISTFILE=/dev/null; {sweep}D=$(mktemp -d /tmp/dp.XXXXXX) && cd \"$D\""
            ),
            CMD_TIMEOUT,
        )?;
        Ok(c)
    }
}

/// Removes the scratch directory, checks nothing was skipped without a marker, and detaches.
pub fn finish(mut c: Client) -> Result<(), String> {
    c.run("cd; rm -rf \"$D\"", CMD_TIMEOUT)?;
    c.clean_output()?;
    c.detach().map(|_| ())
}

/// Ctrl-C, then waits until the shell has drawn its next prompt: a character typed before that is
/// lost by readline (over ssh too). Returns how long the prompt took.
pub fn ctrl_c_to_prompt(c: &mut Client) -> Result<Duration, String> {
    let from = c.len();
    let t = Instant::now();
    c.send(CTRL_C);
    c.after("fil$ ", from)?;
    let back = t.elapsed();
    thread::sleep(Duration::from_millis(150));
    Ok(back)
}

/// Answers one of the client's yes/no prompts: the prompt ignores keys until typing has been
/// quiet for 300 ms and takes only a chunk that is exactly the key, so wait, then send it alone,
/// and type nothing for another 400 ms.
pub fn answer(c: &mut Client, key: &str) {
    thread::sleep(Duration::from_millis(500));
    c.send(key);
    thread::sleep(Duration::from_millis(400));
}

// ---------------------------------------------------------------------------------------------
// Sequences

/// The numbers a program printed, in order: whole lines of digits (`prefix` empty) or lines
/// `PREFIX<digits>...`.
pub fn numbers(plain: &[u8], prefix: &str) -> Vec<u64> {
    numbered(plain, prefix)
        .into_iter()
        .map(|(_, n)| n)
        .collect()
}

/// [`numbers`], each with the index of the line it is on.
fn numbered(plain: &[u8], prefix: &str) -> Vec<(usize, u64)> {
    let text = String::from_utf8_lossy(plain);
    let lines: Vec<&str> = text.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            // A skip (catch-up) cuts at a byte, so the line just before the marker may be a
            // stump (`LINE-024`); it is not a line that was printed.
            if lines.get(i + 1).is_some_and(|n| n.contains("fil: skipped")) {
                return None;
            }
            // The status line ("fil: disconnected ...") is drawn by cursor moves and padded with
            // blanks; once the escapes are gone, the output that follows can sit on its line. Read
            // what comes after the last run of two blanks.
            let l = l.rsplit("  ").next().unwrap_or(l).trim();
            let rest = if prefix.is_empty() {
                l
            } else {
                &l[l.find(prefix)? + prefix.len()..]
            };
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() || (prefix.is_empty() && digits.len() != rest.len()) {
                return None;
            }
            Some((i, digits.parse().ok()?))
        })
        .collect()
}

/// Where `check_seq` writes the transcript of a client whose sequence failed (`--dump DIR`).
pub static DUMP_DIR: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Gap {
    /// Catch-up must not happen: every line, once, in order, and no marker.
    Forbidden,
    /// Big output: complete, or a marker with order kept, no duplicates, a jump only at a marker,
    /// and the tail complete.
    Allowed,
}

/// Checks the numbers printed after `from` against `first..=last`. Exact: a lost, repeated or
/// reordered line is an error. With `Gap::Allowed` a skipped stretch is accepted only when the
/// gap marker was shown for it.
pub fn check_seq(
    c: &Client,
    from: usize,
    prefix: &str,
    first: u64,
    last: u64,
    gap: Gap,
) -> Result<String, String> {
    check_text(&c.text_from(from), prefix, first, last, gap).map_err(|e| match dump(c, from) {
        Some(path) => format!("{e}\n(the transcript is in {})", path.display()),
        None => e,
    })
}

/// Writes the client's whole plain and raw output to `--dump DIR`, if given: `.plain` and `.raw`
/// files named for this process and a counter, both with real host names and addresses scrubbed
/// out (`Targets::scrub`). The first line of the `.plain` file says at which of its bytes the
/// check started (byte `from` of the plain output).
fn dump(c: &Client, from: usize) -> Option<PathBuf> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = DUMP_DIR.get()?;
    let base = dir.join(format!(
        "dropin-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let (plain, at) = c.plain_text(from);
    let plain = format!("(the check starts at byte {at})\n{plain}");
    let ok = std::fs::create_dir_all(dir).is_ok()
        && std::fs::write(base.with_extension("plain"), plain).is_ok()
        && std::fs::write(base.with_extension("raw"), c.raw_text()).is_ok();
    ok.then_some(base)
}

/// `check_seq` on text (so that it can be tested without a client).
pub fn check_text(
    tail: &str,
    prefix: &str,
    first: u64,
    last: u64,
    gap: Gap,
) -> Result<String, String> {
    let nums = numbers(tail.as_bytes(), prefix);
    let markers = tail.matches("fil: skipped").count();
    let want: Vec<u64> = (first..=last).collect();
    let total = want.len();
    if nums == want {
        if markers > 0 && gap == Gap::Forbidden {
            return Err(format!(
                "{prefix}{first}..{last}: a gap marker was shown ({markers}) where nothing may be skipped"
            ));
        }
        return Ok(format!("{total} lines, complete and in order"));
    }
    let anomaly = nums
        .iter()
        .zip(&want)
        .position(|(a, b)| a != b)
        .map_or_else(
            || format!("{} lines seen, {total} wanted", nums.len()),
            |i| {
                format!(
                    "first difference at line {i}: saw {}, wanted {}",
                    nums[i], want[i]
                )
            },
        );
    let fail = |why: &str| {
        Err(format!(
            "{prefix}{first}..{last}: {why} ({anomaly}); {} lines seen, last {:?}, gap markers {markers}\n--- around the first difference ---\n{:?}",
            nums.len(),
            nums.last(),
            around(tail, prefix, &nums, &want)
        ))
    };
    if gap == Gap::Forbidden {
        return fail("lost, repeated or reordered output");
    }
    if markers == 0 {
        return fail("lines are missing and no gap marker says so");
    }
    if nums.first() != Some(&first) {
        return fail("the first line is not where it should be");
    }
    if let Some(i) = nums.windows(2).position(|w| w[1] <= w[0]) {
        return fail(&format!(
            "repeated or reordered lines; the first not above the one before it is {} after {}\n--- around it ---\n{}",
            nums[i + 1],
            nums[i],
            window(tail, prefix, i + 1, 20)
        ));
    }
    let jumps = nums.windows(2).filter(|w| w[1] != w[0] + 1).count();
    if jumps > markers {
        return fail(&format!("{jumps} jumps but only {markers} gap markers"));
    }
    if nums.last() != Some(&last) {
        return fail("the tail is not complete: the last line never arrived");
    }
    let run = nums
        .windows(2)
        .rev()
        .take_while(|w| w[1] == w[0] + 1)
        .count()
        + 1;
    Ok(format!(
        "{markers} gap marker(s): {} of {total} lines kept, in order, no repeats; the last {run} are contiguous up to {last}",
        nums.len()
    ))
}

/// The lines of `tail` from `context` before the `k`th number's line to `context` after it, each
/// with its index and escaped, the number's own line marked.
fn window(tail: &str, prefix: &str, k: usize, context: usize) -> String {
    let Some(&(at, _)) = numbered(tail.as_bytes(), prefix).get(k) else {
        return String::new();
    };
    tail.lines()
        .enumerate()
        .skip(at.saturating_sub(context))
        .take(2 * context + 1)
        .map(|(i, l)| format!("{}{i:6} {l:?}", if i == at { ">" } else { " " }))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text just after the line before the first difference (escaped), and whatever the output
/// says about skipped output anywhere.
fn around(tail: &str, prefix: &str, nums: &[u64], want: &[u64]) -> String {
    let Some(i) = nums.iter().zip(want).position(|(a, b)| a != b) else {
        return String::new();
    };
    let before = i.checked_sub(1).map(|j| nums[j]);
    let mut at = 0;
    if let Some(v) = before {
        let mut off = 0;
        for l in tail.split_inclusive('\n') {
            if numbers(l.as_bytes(), prefix) == [v] {
                at = off;
                break;
            }
            off += l.len();
        }
    }
    let end = (at + 700).min(tail.len());
    let end = (at..=end)
        .rev()
        .find(|&i| tail.is_char_boundary(i))
        .unwrap_or(at);
    let mut s = format!("{:?}", &tail[at..end]);
    if let Some(k) = tail.find("skipped") {
        let from = (k.saturating_sub(40)..=k)
            .find(|&i| tail.is_char_boundary(i))
            .unwrap_or(k);
        let to = (k + 80..=k + 120)
            .find(|&i| i <= tail.len() && tail.is_char_boundary(i))
            .unwrap_or(tail.len());
        s.push_str(&format!(
            "\n(the word \"skipped\" is at byte {k}: {:?})",
            &tail[from..to]
        ));
    }
    s
}

/// Starts the counting loop `TICK-1 .. TICK-N`, a quarter second apart.
pub fn start_ticks(c: &mut Client, n: u32) {
    c.send(&format!(
        "for i in $(seq 1 {n}); do echo TICK-$i; sleep 0.25; done\r"
    ));
}

/// The tick lines on the screen are consecutive: the status line, which is drawn over the bottom
/// row, left no damage behind it once it went.
pub fn screen_ticks_intact(c: &Client) -> Result<(), String> {
    let nums: Vec<u64> = c
        .rows()
        .iter()
        .filter_map(|r| r.trim().strip_prefix("TICK-")?.parse().ok())
        .collect();
    ensure(
        nums.len() >= 10 && nums.windows(2).all(|w| w[1] == w[0] + 1),
        || {
            format!(
                "the tick lines on the screen are not consecutive: {nums:?}\n{}",
                c.dump()
            )
        },
    )
}

// ---------------------------------------------------------------------------------------------
// Stopping a process group

/// Stops everything the client runs, as a sleeping laptop does: its own process group and the
/// group of each child it started (a reconnecting pipe runs in a group of its own, in the
/// background of the terminal). Continues them all when dropped, whatever happens in between.
pub struct Stopped(Vec<u32>);

impl Stopped {
    pub fn all(c: &Client) -> Stopped {
        let mut groups = vec![c.pid()];
        for child in c.children() {
            if let Some(g) = pgid_of(child)
                && !groups.contains(&g)
            {
                groups.push(g);
            }
        }
        for g in &groups {
            signal_group(*g, "STOP");
        }
        Stopped(groups)
    }

    /// How many process groups are stopped (the client's, and its children's if they differ).
    pub fn groups(&self) -> usize {
        self.0.len()
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        for g in &self.0 {
            signal_group(*g, "CONT");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// vim

/// vim open on a new line of `e.txt` (which holds `start`) in the shell's `$D`, in insert mode;
/// None if the host has no vim or vi.
pub fn vim_open(c: &mut Client) -> Result<Option<&'static str>, String> {
    let ed = if c.have("vim")? {
        "vim"
    } else if c.have("vi")? {
        "vi"
    } else {
        return Ok(None);
    };
    c.run("echo start > e.txt", CMD_TIMEOUT)?;
    c.send(&format!("{ed} e.txt\r"));
    c.want_alt(true, ATTACH_TIMEOUT)?;
    c.want_row("start", ATTACH_TIMEOUT)?;
    c.send("o");
    Ok(Some(ed))
}

/// Types `ROW-from` .. `ROW-to`, a line each, a little apart as a person would.
pub fn type_rows(c: &mut Client, from: u32, to: u32) {
    for i in from..=to {
        c.send(&format!("ROW-{i}\r"));
        thread::sleep(Duration::from_millis(150));
    }
}

/// Leaves insert mode, `:wq`, and compares the whole file with what was typed: `start`, then
/// `ROW-1` .. `ROW-last`, then the empty line the last Enter made. Nothing else, nothing twice.
pub fn vim_finish(c: &mut Client, last: u32) -> Result<(), String> {
    c.send(&format!("{ESC}:wq\r"));
    c.want_alt(false, CMD_TIMEOUT)?;
    // vim drops what is typed ahead while it leaves.
    thread::sleep(Duration::from_millis(500));
    let out = c.run(
        &format!(
            "{{ echo start; seq -f 'ROW-%g' 1 {last}; echo; }} > want.txt; cmp e.txt want.txt && echo EXACT-$((6*7)); wc -l < e.txt"
        ),
        CMD_TIMEOUT,
    )?;
    ensure(has_line(&out, "EXACT-42"), || {
        let got = c
            .run("cat -v e.txt | head -n 40", CMD_TIMEOUT)
            .unwrap_or_default();
        format!(
            "the file is not exactly start, ROW-1..ROW-{last} and an empty line\n{out}\n--- the file ---\n{got}"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticks(r: impl Iterator<Item = u64>) -> String {
        r.map(|n| format!("TICK-{n}\n")).collect()
    }

    const GAP: &str = "\u{1b}[2m── fil: skipped 48 MB of output ──\u{1b}[0m\n";

    #[test]
    fn a_complete_run_passes_both_ways() {
        let t = ticks(1..=40);
        assert!(check_text(&t, "TICK-", 1, 40, Gap::Forbidden).is_ok());
        assert!(check_text(&t, "TICK-", 1, 40, Gap::Allowed).is_ok());
    }

    #[test]
    fn dropped_ticks_with_a_marker_fail_where_catch_up_is_not_expected() {
        // The mutation the review made: ticks 13 to 40 lost, a marker shown.
        let t = format!("{}{GAP}{}", ticks(1..=12), ticks(41..=60));
        assert!(check_text(&t, "TICK-", 1, 60, Gap::Forbidden).is_err());
        // Where a flood may skip, it is accepted: the jump sits at the marker, the tail is whole.
        assert!(check_text(&t, "TICK-", 1, 60, Gap::Allowed).is_ok());
    }

    #[test]
    fn a_marker_does_not_excuse_repeats_reordering_or_a_missing_tail() {
        let dup = format!("{}{GAP}{}", ticks(1..=12), ticks(10..=60));
        assert!(check_text(&dup, "TICK-", 1, 60, Gap::Allowed).is_err());
        let order = format!("{}{GAP}{}{}", ticks(1..=12), ticks(50..=60), ticks(30..=40));
        assert!(check_text(&order, "TICK-", 1, 60, Gap::Allowed).is_err());
        let short = format!("{}{GAP}{}", ticks(1..=12), ticks(41..=59));
        assert!(check_text(&short, "TICK-", 1, 60, Gap::Allowed).is_err());
    }

    #[test]
    fn a_jump_without_a_marker_fails_even_where_gaps_are_allowed() {
        let t = format!("{}{}", ticks(1..=12), ticks(41..=60));
        assert!(check_text(&t, "TICK-", 1, 60, Gap::Allowed).is_err());
    }

    #[test]
    fn two_jumps_need_two_markers() {
        let t = format!("{}{GAP}{}{}", ticks(1..=10), ticks(20..=30), ticks(40..=60));
        assert!(check_text(&t, "TICK-", 1, 60, Gap::Allowed).is_err());
        let t = format!(
            "{}{GAP}{}{GAP}{}",
            ticks(1..=10),
            ticks(20..=30),
            ticks(40..=60)
        );
        assert!(check_text(&t, "TICK-", 1, 60, Gap::Allowed).is_ok());
    }

    #[test]
    fn a_complete_run_with_a_marker_still_fails_where_none_may_appear() {
        let t = format!("{GAP}{}", ticks(1..=60));
        assert!(check_text(&t, "TICK-", 1, 60, Gap::Forbidden).is_err());
    }

    #[test]
    fn a_status_line_in_the_middle_of_a_line_does_not_hide_the_number() {
        let t = "TICK-1\nTICK-2\n fil: disconnected 0s  reconnecting over pipe                TICK-3\nTICK-4\n";
        assert!(check_text(t, "TICK-", 1, 4, Gap::Forbidden).is_ok());
    }

    #[test]
    fn the_stump_of_a_line_cut_by_a_skip_is_not_a_line() {
        let t = format!(
            "TICK-1\nTICK-2\nTICK-3\nTICK-4\nTICK-5\nTICK-6\nTICK-7\nTICK-8\nTICK-9\nTICK-1{GAP}{}",
            ticks(40..=45)
        );
        assert_eq!(numbers(t.as_bytes(), "TICK-").len(), 9 + 6);
    }

    #[test]
    fn a_repeat_is_shown_with_the_lines_around_it() {
        let t = format!("{}{GAP}{}{}", ticks(1..=12), ticks(40..=50), ticks(49..=60));
        let e = check_text(&t, "TICK-", 1, 60, Gap::Allowed).unwrap_err();
        assert!(e.contains("is 49 after 50"), "{e}");
        assert!(e.contains(">"), "{e}");
        assert!(
            e.contains("\"TICK-30\"") || e.contains("\"TICK-50\""),
            "{e}"
        );
    }

    /// A catch-up just after the space that follows the number: the snapshot leaves the space as
    /// an empty cell and the transcript has none (`output::flood`).
    #[test]
    fn a_number_glued_to_letters_is_still_read() {
        let t =
            "LINE-029406 abcdefghij0123\nLINE-029407abcdefghij0123\nLINE-029408 abcdefghij0123\n";
        assert_eq!(numbers(t.as_bytes(), "LINE-"), [29406, 29407, 29408]);
    }

    #[test]
    fn plain_numbers_ignore_other_lines() {
        let t = "fil$ seq 1 3\n1\n2\n3\nFLOODEND-42\n";
        assert!(check_text(t, "", 1, 3, Gap::Forbidden).is_ok());
    }
}
