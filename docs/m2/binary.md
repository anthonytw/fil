# The `fil` binary in M2 (`crates/fil`)

What M2 adds to the binary, by package. The M1 command line, exit statuses and dialer are in `docs/m1/binary.md`.

## F4 part 1: forward specs, config and `fil ports` rendering

Part 1 is the pure half of F4: parsing, validating, carrying and rendering. Nothing in it opens a socket, talks to the daemon or reaches the engine. Part 2 wires it (see the end).

### The spec grammar (`src/forward_spec.rs`)

```
-L PORT                          same as PORT:localhost:PORT
-L [BIND:]PORT:HOST:HOSTPORT     listens here, the host connects out
-R PORT                          same as PORT:localhost:PORT, listening on the host
-R [BIND:]PORT:HOST:HOSTPORT     listens on the host, this machine connects out; PORT 0: the host picks
-D [BIND:]PORT                   SOCKS on this machine
```

- **Fields** are separated by `:` outside square brackets. A bracket opens at the start of a field and closes at its end, and encloses an IP address and nothing else (`[::1]`, `[2001:db8::1]`).
- **PORT, HOSTPORT:** decimal digits, 1 to 65535. A `-R` listener's PORT may be 0.
- **BIND:** nothing, `*`, `localhost`, an IPv4 address, or a bracketed IP address.
  - **No BIND (a bare port, or a spec with three fields) is loopback**, on both ends: 127.0.0.1 and ::1.
  - `localhost` is the same.
  - `*` and the empty BIND (`:8080:db:5432`) are every interface, and must be written.
  - `0.0.0.0` and `::` are written too, and are wide.
  - A forward whose listener is beyond loopback is flagged `exposed` in `fil ports` and its JSON.
- **Loopback, however written:** `::ffff:127.0.0.1` counts as loopback (not exposed, and allowed for `-R`); IPv6 zone ids (`fe80::1%en0`) are refused.
- **HOST:** an IPv4 address, a bracketed IP address, or a host name the wire accepts (`DnsName`: letters, digits and inner hyphens, labels of 1 to 63 bytes, 253 in all, the last label not all digits). It is kept as written.
- **Config form:** `"-L 5173"`, `"-R 9000:db:5432"`, `"-D 1080"` (the flag, then the spec).
- **Limits:** a spec is ASCII without spaces or control characters and at most 512 bytes. A plan holds at most 64 forwards (`MAX_FORWARDS`).
- **Errors never repeat the refused text raw.** The caller quotes it ASCII-escaped and cut to 60 characters (`forward_spec::quote`), because a spec can come from a config file or a script. The command line does it with its own clap value parser (clap's own "invalid value 'X'" would echo the bytes on a terminal), and config errors are escaped where they are displayed (`config::escape_unprintable`), so a terminal-title escape in any value, `predict` included, shows as `\u{1b}`.

### Where fil differs from OpenSSH (`ssh(1)`, `-L`, `-R`, `-D`)

| fil | OpenSSH | Why |
|---|---|---|
| `-L PORT` and `-R PORT` are the same port on the other side's `localhost` | `-L` needs host and port; `-R PORT` alone is a SOCKS proxy on the remote | DESIGN §7.3 writes `fil orin -L 5173 -R 9000`. A remote SOCKS proxy is not in M2; `-R PORT:x` (two fields) is refused with a message that says so. |
| No Unix-socket forms (`-L /path:…`, `-R …:/path`) | supported | Non-goal for M2 (plan open question 5). A `/` in a spec gets its own message. |
| Port 0 only for a `-R` listener | `0` is accepted in more places | The wire's rule (`ForwardSpec`): the host reports the port it got, and nothing reports one for `-L`/`-D`. A target port is never 0. |
| A bind is an IP address, `localhost`, `*` or empty | any host name, resolved | `BindAddr` has no name. A name would also make the bind depend on the resolver. `localhost` is loopback on both families. |
| Ports are digits | also service names (`http`) | A service name depends on the machine's services database. |
| Brackets enclose IP literals only; IPv4 is dotted-quad | any host in brackets; `127.1`, octal and hex forms reach `getaddrinfo` | The wire keeps an address written as a name out of `TargetHost::Name` (§7.1), so policy sees every literal address. `127.1` is refused, not read as 127.0.0.1. |
| Host names: letters, digits, hyphens | also `_` and anything else the resolver takes | `DnsName`. |
| A `-R` listener beyond loopback parses and is **refused by policy** | honoured if the server's `GatewayPorts` allows | OD-7 is the owner's and open (plan §4.6). `forward_spec::check_policy` refuses it from the command line and the config, naming OD-7; flipping one constant allows it. |
| The bind default does not depend on any `GatewayPorts` setting | follows `GatewayPorts` (default no, so loopback) | The loopback default is a security requirement here (§7.1, plan §4.2), not a preference. |
| Repeated specs: an equal one is kept once; one with the same `(kind, bind, port)` and another target replaces it; the order is config, then command line | the last wins per listener | The same rule as the daemon's table (§7.2). Every `-R 0` is its own listener. |
| Two different entries that would listen on the same port of one machine (`-L 8080` with `-L 127.0.0.1:8080:…`, `[::1]:8080:…`, `*:8080:…` or `-D 8080`; two `-R 9000` binds that overlap) are refused when the plan is made | the second bind fails at run time | They are different table entries (their identities differ) but would fight over one socket. Overlap is by address: loopback with any loopback address, a wildcard (`*`, empty, `0.0.0.0`, `::`) with everything. `-L` and `-D` share the laptop; `-R` is on the host; port 0 never collides. |
| `-A` never lowers `agent = "confirm"`; `-a` switches the agent off; the last of the two wins | `-A`/`-a` set `ForwardAgent` | A flag asking for the agent must not remove a question the config asked. |

### Config (`src/config.rs`)

Keys in `[defaults]` and `[host.NAME]`; a host's value replaces the default's, a table field by field (the existing rule). The command line is over both.

| Key | Values | Default |
|---|---|---|
| `forward` | a list of specs (`["-L 5173", "-R 9000"]`), or a table (below) | none |
| `agent` | `"off"`, `"on"`, `"confirm"` | `"off"` |
| `agent_keys` | SHA256 fingerprints (`"SHA256:…"`, 43 base64 characters), at most 64 | `[]`: every key |
| `remote_requests` | `"ask"`, `"deny"` | `"ask"` |
| `forward_failure` | `"warn"`, `"exit"` (status 255) | `"warn"` |

`forward` is a list **or** a table, because a TOML key cannot be both: with tunables, write `[defaults.forward]` and put the specs in `specs = [...]`. Tunables (plan §2.9), checked when the file is read:

| `[defaults.forward]` key | Range | Default |
|---|---|---|
| `window` | `"16KiB"` to `"4MiB"` | 1 MiB |
| `buffer` | `"16KiB"` to `"16MiB"` | 1 MiB |
| `open_wait` | `"0s"` to `"1h"` (`"30s"`, `"1500ms"`, a number is seconds) | 30 s |
| `linger` | `"0s"` to `"24h"` | 10 min |

`HostSettings` holds them parsed (`forward: Vec<ForwardSpec>`, `forward_tunables`, `agent`, `agent_keys`, `remote_requests`, `forward_failure`); `ForwardTunables::mux_config()` is the `MuxConfig` they make. Specs are checked (grammar and policy) when the file is read, and an error names the section, the key and the index.

### The run's plan

`ForwardPlan::resolve(&HostSettings, &CliForwards)` is what a run carries: the specs (config, then `-L`, `-R`, `-D`), the resolved agent mode, and the other settings. `Action::Attach` carries the parsed flags. `app.rs` turns the plan into the client's `ForwardSet` (F3) and binds its `-L` and `-D` listeners before the attach:

- a port that cannot be bound is printed before the terminal goes raw; with `forward_failure = "exit"` nothing is attached and the status is 255 (ssh's `ExitOnForwardFailure`), otherwise it is a warning and the forward is still added to the session (reported as not listening, tried again at every fresh attach). A forward of the run's own that fails later (the host refuses it, or cannot bind a `-R`) ends the run with 255 under `"exit"` and is a notice otherwise;
- the agent (A1): `-A` or a config `agent` of `on` or `confirm` relays the local agent at `SSH_AUTH_SOCK` as it is when `fil` starts (never a path from the host), with `agent_keys` and `confirm` carried into the client's `ForwardSet::agent`; with no `SSH_AUTH_SOCK` it warns and carries on (the session's socket then refuses, as `ssh -A` without an agent);
- a bad spec is a usage error (exit 2), including `-R` beyond loopback (OD-7); two forwards that would take one port are an error (exit 1) when the plan is made.

### `fil ports` rendering (`src/ports_view.rs`)

`PortsAnswer::from_parts(&[Response])` collects the parts of an answer (rows capped at `MAX_PORTS_ROWS`; it notes an answer that was cut or ended early). `render_text` makes the DESIGN §7.4 table and `render_json` the JSON, both from a `ViewOptions { host, now_unix_ms, utc_offset_secs }`, so they are pure.

- **Text:**
  - One block per session: a header, `FORWARDED`, `LISTENING IN THIS SESSION, NOT FORWARDED`, `AGENT`.
  - After the sessions: `OTHER LISTENERS YOU OWN ON THIS HOST` and `LISTENERS OF OTHER USERS ON THIS HOST (port and address only)`.
  - Loopback binds are written `localhost` on the laptop and `127.0.0.1` on the host. IPv6 addresses are bracketed in the table (`[2001:db8::1]:443`).
  - A detail line under a forward gives the probe (with a URL for an HTTP service behind a `-L`), a failed or missing bind, waiting connections, a management-origin entry and whether it was confirmed, and any listener beyond loopback (`⚠`).
  - Times are `HH:MM` for today and `YYYY-MM-DD HH:MM` otherwise, in the zone given. `now` is the last 5 seconds.
  - Sizes use 1024 and the labels KB, MB.
- **Every string from the host goes through `fil_proto::text::sanitize`:** process names, working directories, probe labels and the attached client's host label, in the text and in the JSON. Control characters (escape sequences included) and invisible or reordering characters are removed and line breaks become spaces. The wire caps lengths (32, 128 and 64 bytes); the view does not rely on that. A name that is empty after cleaning shows as `(unnamed)`.
- **JSON:** `schema` is 1; a field is never renamed or retyped within a schema. Times are Unix milliseconds (`null` for none); enums are lowercase strings; `bind` is `"loopback"`, `"any"` or an address; `spec` is the forward in full form, as a flag reads it; every key is always present. Addresses (`bind`, `target.host`, a listener's `addr`) are bare, never bracketed; `spec` is the one field written in spec syntax. Pinned by `tests/golden/ports.json` and a key-set test.
- **`fil ls`** has a `FORWARDS` column (the table size, and `agent` when the relay is on; `-` for neither). It is pure rendering of `SessionSummary`, so it is here.

### Tests

- `tests/forward_spec.rs`: the grammar table, the loopback default (`a_bare_port_binds_loopback_never_wide`), hostile input (a flag as a spec, control characters, overlong names, port 0 on `-L`, out-of-range ports, unbalanced brackets, Unix sockets), a round trip through the formatter, random-input no-panic, OD-7, the plan's precedence and the agent flags.
- `tests/ports_view.rs`: the text golden (`tests/golden/ports.txt`) and JSON golden for a fixed answer (IPv6 targets, binds and listeners included), the JSON key set, an escape sequence in a process name, path, probe label and host label rendered inert, multi-part assembly, truncation.
- `tests/binary.rs`: the forward flags and `-A` reach the attach, bad specs are usage errors, a refused spec is not echoed raw on a pty (clap styles its output there), and a `-L` on a port in use exits 255 under `forward_failure = "exit"` and warns otherwise.
- Unit tests in `config.rs`, `cli.rs` and `mgmt.rs`.

Mutations, applied by hand and reverted: the no-BIND default changed to `Any` fails `a_bare_port_binds_loopback_never_wide`, the grammar table and four more; `ports_view::clean` made the identity fails `escape_sequences_from_the_host_are_rendered_inert` and `a_name_that_is_only_escapes_still_shows_a_row`.

### Left for part 2

- `fil forward` (laptop and in-session), `--rm`, the ambiguity error, `$FIL_SESSION` and the daemon socket of the right revision.
- `fil ports [HOST [SESSION]] [--json]` as a command: send `Request::Ports`, collect every `Response::Ports` part (today `mgmt::ask` reads one response), choose the host's local time zone for `utc_offset_secs`, and print with `render_text`/`render_json`.
- `fil detach` with no arguments (it needs the in-session daemon socket).
- The hint under an unforwarded listener says `fil forward -L PORT`; it may need a host and session when run from the laptop.
