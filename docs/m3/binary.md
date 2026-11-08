# The `fil` binary in M3 (`crates/fil`)

> Where this file and `DESIGN.md` or the code differ, they win.

What M3 adds to the binary: the `predict` key and the `--predict` flag (E1b, `docs/m3/plan.md` §3 and §2.7). The M1 command line, exit statuses and dialer are in `docs/m1/binary.md`; M2's forward, `ports` and agent additions in `docs/m2/binary.md`. The client's side of prediction (the predictor factory, `ClientConfig::predict`, the attach's `echo_ack`) is E1a's, in `fil-client`.

## From 0.2.0

0.1.0 has no prediction: no `predict` key (its parser refuses it as unknown) and no `--predict` flag (decided, owner 2026-11-04: N9). Both arrive with 0.2.0, with the default of `DESIGN.md` §9 (PC-1).

## The key and the flag

```toml
[defaults]
predict = "adaptive"      # off | adaptive | always

[host.devbox]
predict = "off"
```

```
fil devbox --predict off|adaptive|always
```

| Value | What runs | The attach's `echo_ack` |
|---|---|---|
| `adaptive` (default) | the default predictor; guesses shown when its own estimate of the round trip is above 30 ms, hidden below 20 ms, also for 30 s after a glitch | `true` |
| `always` | the same predictor, shown at any round trip | `true` |
| `off` | no predictor, nothing painted | `false`: no `EchoAck`, the M1 mode poll |

- **Precedence:** `--predict` over the `[host.NAME]` section the HOST selects (`user@name` first, then `name`) over `[defaults]` over the default, `adaptive` (`config::DEFAULT_PREDICT`). `app::attach_settings` applies the flag to the config's `HostSettings`, and `app::client_config` carries `predict` into the client's `ClientConfig`, which picks the predictor (`fil_client::predictor_factory`).
- **A wrong value** is refused with the three listed. On the command line it is clap's usage error (exit status 2, `[possible values: off, adaptive, always]`), before anything is dialed. In the file it is serde's `unknown variant`, which names the file and the section and lists `` `off`, `adaptive`, `always` ``, when the file is read, for any section (exit status 1); the value is escaped as every config error is (`config::escape_unprintable`).
- **Before a subcommand** (`fil --predict off ls HOST`) the flag would be ignored, so it is a usage error, as `--transport` is.
- **`fil config show HOST`** has a `predict` row with its source (`default`, `[defaults]` or `[host.NAME]`); its header lists `--predict` among the command-line options it does not apply.
- **Logs** (`docs/m3/plan.md` §4.3): with prediction on, one info line per attach, `prediction on (adaptive)` or `prediction on (always)`; with it off, a debug line `prediction off` with `echo_ack = false`. The mode only: nothing typed, and the host stays at debug as before. The predictor's counts are E1a's debug line at the end of a run.
- The thresholds and windows are constants of the predictor (`MoshConfig`), not keys (PC-7).

## Tests

- `tests/predict_flag.rs`: the precedence table through the real command-line parser, the config parser and `app::attach_settings`/`app::client_config` (the default; `[defaults]`; a host's section, also for `user@host`, and not for another host; the flag over each, before and after the host); `--predict maybe` refused with exit status 2 and the valid values, and `predict = "maybe"` in `[defaults]` or in another host's section refused by `fil config show` with the valid values; `fil config show` showing the row and its source; and through the real client loop (`fil_client::run` over the real `PathDialer` and the scripted daemon) the first `Attach` has `echo_ack = false` for `off` from the flag or from the host's section, and `true` for the default, `always` and `adaptive`.
- Unit tests: `config.rs` (the default and the layering, the `config show` row and its read-back, the bad values, the escaped error, the key lists, the DESIGN sketch's `predict`), `cli.rs` (`predict_override`: each value, before and after the host, `maybe` refused with the values, the stray-flag error).
- `tests/examples.rs`: the shipped `docs/user/examples/config.toml` sets the default in `[defaults]`. `tests/docs_sync.rs`: `config.md` documents the key, its values and the flag (and every flag of `fil --help`, `--predict` included, is in the guide); the man page is regenerated.

### Mutations

Applied by hand, one at a time, and reverted:

| Mutation | Fails |
|---|---|
| `attach_settings` ignores the flag | `the_flag_is_over_the_host_over_defaults_over_the_default`, `off_attaches_without_echo_ack_and_the_others_with_it` |
| `client_config` leaves `ClientConfig::predict` at its `Off` | the same two |
| `resolve` drops `predict` | `predict_defaults_to_adaptive_and_layers_host_over_defaults`, `config_show_has_the_predict_row_with_its_source` |
| `[defaults]` over the host's section (`base.predict.or(self.predict)`) | the same two |
| the default `Off` | the same two |
| `--predict off` mapped to `adaptive` | `predict_override` |
| `--predict` left out of the stray-flag check | `predict_override` |

## Not changed

- `tests/realnet/predict.sh` (T3, then V1) already handles both builds: it detects `--predict` in `fil --help`, writes `[defaults] predict = "off"` only for a build that has the flag, and passes `--predict MODE`, which is over the file. With this change its adaptive and always cells run with nothing edited.
- The drop-in examples (`crates/fil/examples/dropin/`) set no `predict` and pass no flag, so they now run with the default, `adaptive`; V1's `predict.rs` will choose modes explicitly.
