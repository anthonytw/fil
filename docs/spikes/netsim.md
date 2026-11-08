# Spike B: the link simulator (`fil-netsim`)

> **Historical: the M0 spike record.** Written while the spike was run; the code has moved on since (the crates, the numbers and some names differ). The current design is `DESIGN.md`; take the findings below as history, not as a description of today's code.

`crates/fil-netsim` is the in-process link simulator that DESIGN.md §12 asks for. It is a
test-only crate (`publish = false`). It has two kinds of link:

- **Stream link:** stands in for the pipe (ssh).
- **Datagram link:** stands in for UDP, and runs real `quinn` endpoints.

Everything runs on tokio time, so tests use `#[tokio::test(start_paused = true)]` and a 60-second
blackout costs microseconds. The whole crate's test suite takes about 0.4 s.

## API

```rust
use fil_netsim::{Net, StreamConfig, stream_pair};

// Datagram link: a Net is both the network and the control handle (cheap to clone).
let net = Net::new(/* seed */ 1);
net.update_conditions(|c| {
    c.latency = Duration::from_millis(20);
    c.jitter = Duration::from_millis(10);   // in order per path; see jitter_reorders
    c.jitter_reorders = false;               // true: independent delay per datagram
    c.loss = 0.05;
    c.reorder = 0.1;                         // held back by reorder_delay
    c.reorder_delay = Duration::from_millis(50);
    c.duplicate = 0.01;
});
let server_sock = net.bind("192.0.2.10:4433".parse()?)?;               // Arc<SimSocket>
let client_sock = net.bind_behind_nat("10.0.0.2:5000".parse()?, "198.51.100.7".parse()?)?;
// SimSocket implements quinn::AsyncUdpSocket:
let ep = quinn::Endpoint::new_with_abstract_socket(
    quinn::EndpointConfig::default(), Some(server_cfg), server_sock, Arc::new(quinn::TokioRuntime))?;

// Mid-test control:
net.set_blackout(true);                       // or: net.blackout_for(Duration::from_secs(3)).await
client_sock.rebind();                         // new public port, same IP, effective immediately
client_sock.rebind_to_ip("203.0.113.9".parse()?); // new IP too (Wi-Fi to cellular)
net.update_conditions(|c| *c = Conditions::default());    // clean link again
net.stats();                                  // sent / queued / lost / blackholed / unroutable / duplicated / reordered

// Stream link: AsyncRead + AsyncWrite ends plus a control handle.
let (a, b, ctl) = stream_pair(/* seed */ 1, StreamConfig {
    latency: Duration::from_millis(30), jitter: Duration::from_millis(20),
    bandwidth: Some(1_000_000), ..Default::default()
});
ctl.stall();    // data is held (writes still succeed until the buffer fills)
ctl.release();  // held data is delivered
ctl.reset();    // both ends get ConnectionReset, in-flight data is discarded
```

Semantics worth knowing:

- **Stream link:** always in order and lossless. Jitter delays a chunk but never lets it pass the
  chunk before it. Dropping one end gives the other end EOF after the buffered data drains (and
  `BrokenPipe` on write). `shutdown()` gives EOF as usual. The in-flight buffer is bounded
  (`StreamConfig::buffer`, 64 KiB), so a slow link applies backpressure to writers.
- **Datagram link:** jitter keeps order per (source, destination) path by default. A datagram is
  due at `max(now + latency + jitter, the path's last due time)`, as with queueing delay on a real
  link, so reordering comes only from the explicit `reorder` and `reorder_delay` knobs. Set
  `Conditions::jitter_reorders` to give every datagram an independent delay instead (this
  reorders almost constantly at QUIC packet rates). Duplicates take their own delay. A
  blackout silently drops datagrams *sent* during it; datagrams already in flight still arrive.
- **NAT:** a `bind_behind_nat` socket has a private address (quinn's `local_addr`) and a public
  one (the source the peer sees). `rebind` moves the public mapping. The old mapping stops
  working at once, so replies the peer sends to the old address are dropped (`unroutable`). That is
  harsher than most real NATs, which keep the old mapping for a while.
- **Determinism:** the simulator's RNG is seeded. Quinn's is not, unless you also set
  `EndpointConfig::rng_seed`. The tests do, and then runs are bit-for-bit repeatable (the NAT test
  prints identical timing and counters on every run).

## Tests

All in `just ci` unless noted. `cargo test -p fil-netsim` runs in about 0.4 s after compiling.

| Test | What it shows |
|---|---|
| `tests/sim.rs` (16 tests) | latency applied exactly; stream order kept under 200 ms jitter; bandwidth limit (100 KB at 10 KB/s takes 10 s); stall/release; reset errors both ends, including a waiting reader; datagram loss rate in 18-22% for 20% set; explicit reordering happens; jitter keeps order by default and reorders when `jitter_reorders` is set; duplication; blackout; NAT rebind changes the source and kills the old mapping; same seed gives the same outcome |
| `quic_echo_5pct_loss_with_jitter` | 3 MiB echo over 20 ms ± 10 ms (in-order) jitter and 5% loss, intact, one handshake (36 s virtual) |
| `quic_survives_nat_rebinding_mid_transfer` | two rebinds during a 3 MiB echo, intact, one handshake |
| `quic_baseline_without_rebinding` | the same transfer without rebinds, for comparison |
| `quic_survives_blackout_shorter_than_idle_timeout` | 4.5 s blackout, 5 s idle timeout: survives |
| `quic_ends_on_blackout_longer_than_idle_timeout` | blackout with a 5 s idle timeout: `TimedOut` after 5 s, then a fresh handshake works |
| `stream_echo_intact`, `stream_reset_mid_transfer_is_an_error` | the same echo over the stream link; a reset surfaces as `ConnectionReset` on read and write |
| `soak_harsh_conditions_many_seeds` (`just netsim-soak`, ignored) | 25 seeds with 8% loss, 30 ms jitter, 10% reordering, 3% duplication, 6 rebinds and 3 blackouts per run; all intact, one handshake each (about 2 s real); the reorder knob is on in the soak |

The client in the QUIC tests pins the server's exact self-signed certificate (made with `rcgen`)
as its only trust anchor. No TOFU.

## What quinn did

All numbers are virtual time unless marked, from a debug build.

**NAT rebinding** (3 MiB echo, 20 ms one way, 1% loss; rebinds at 0.3 s and 0.8 s):

- The connection survived with **no new handshake**: the server saw exactly one incoming
  connection, and `stable_id` stayed the same.
- The server's `remote_address()` followed the client: `198.51.100.7:40000`, then `:40001`, then
  `203.0.113.9:40002`. The second rebind changed the IP as well as the port.
- Data was intact. The transfer took 5.2 s, against 7.9 s for the same run without rebinds. Treat
  that as noise: quinn resets its congestion state on a new-IP migration (a same-IP port change
  keeps it, since it "looks like NAT rebinding"), and a reset can help on a lossy path. The point
  is that rebinding did not hurt.
- 37 datagrams were dropped as `unroutable`, because they were sent to a mapping that had just
  died. Quinn retransmitted and recovered.
- Migration is server-side: quinn migrates when it sees a new source address on the highest-numbered
  non-probing packet, then runs path validation. Clients never accept packets from a new server
  address.

**Blackouts** (5 s idle timeout, 1 s keep-alive, 20 ms latency, link quiet before the blackout):

| Blackout | Result |
|---|---|
| 3.5, 4.0, 4.5, 4.8, 4.9 s | survives; the first echo after the link returns completes in 40 ms (one RTT) |
| 5.0, 5.1, 5.5 s | connection ends with `ConnectionError::TimedOut` |
| indefinite | `TimedOut` exactly 5 s after the last packet received |

The cutoff is sharp and equals `max_idle_timeout`. Quinn's PTO backoff did not delay recovery in
these runs. So **`max_idle_timeout` plus `keep_alive_interval` is the dead-link knob** for
DESIGN.md §4.2, with the numbers there (1 s heartbeat, 5 s dead) mapping to
`keep_alive_interval = 1 s`, `max_idle_timeout = 5 s`. After the timeout, a fresh handshake over
the healed link worked, which is the "new QUIC handshake with the same key" path of §4.2.

**Loss, jitter and reordering** (2 MiB echo, 40 ms one way, one-off debug run, no rebinds):

| Conditions | Time |
|---|---|
| clean link (baseline) | 0.8 s |
| 3% duplication | 0.8 s |
| 30 ms jitter, in order (default), no loss | 1.2 s |
| 100 ms jitter, in order (default), no loss | 2.1 s |
| 8% loss | 34 s |
| 30 ms jitter, in order, with 5% loss | 63 s |
| 1% reordering by 80 ms, no loss | 12 s |
| 10% reordering by 10 ms, no loss | 26 s |
| 10% reordering by 80 ms, no loss | 31 s |
| 30 ms jitter, **opt-in `jitter_reorders`**, no loss | 53 s |

Order-preserving jitter is cheap for quinn: 30 ms of jitter on a 40 ms path cost about 50% more
time than a clean link (1.2 s against 0.8 s), and 100 ms cost under 3x (2.1 s). The cost is
likely delay variation inflating the RTT estimate (not measured separately); the link
stayed in order and lost nothing.

Reordering is where quinn is sensitive, because RFC 9002 loss detection (a packet-number
threshold of 3 and a time threshold) declares late packets lost, causing congestion events and a
shrunken window. Even 1% of datagrams arriving 80 ms late costs 15x the clean time. The
opt-in `jitter_reorders` row is the opt-in `jitter_reorders` mode: independent per-datagram delay,
which reorders almost constantly at these packet rates. It is not how real path jitter behaves,
and the first version of this report wrongly read it as quinn being slow on jittery paths. A path
that really reorders, such as multipath or load balancing across links, would see something
between the two. Tune M1 against in-order jitter and loss, and treat reordering as its own case.

Loss behaves oddly too: the 5% loss plus in-order jitter row has less loss than the 8% row but
takes about twice as long. These are single seeded runs, so treat differences of 2x between
rows with loss as unexplained rather than as a measured effect.

## Surprises

- **Quinn works with paused time out of the box.** `quinn::TokioRuntime::now()` and its timers use
  `tokio::time::Instant`, so virtual time drives every quinn timer. Nothing needed papering over.
  The only wall-clock use is rustls checking certificate validity against `SystemTime`, which is
  harmless with `rcgen` certificates.
- **Quinn is not deterministic by default** (random connection IDs and path challenges), so the
  same simulator seed gave different timings on each run until `EndpointConfig::rng_seed` was set.
- **Quinn's `UdpPollHelper` is private**, so `SimSocket` has its own always-writable `UdpPoller`.
  The datagram link has no send-side backpressure or queue limit.
- **Repeated rebinds can outlast a 10 s idle timeout.** In the first soak (idle timeout 10 s,
  rebinds about every 1 s, plus 1.5 s blackouts, 8% loss) several runs out of 25 seeds died with `TimedOut` (timing varied between runs, since quinn was not yet seeded).
  The server was still addressing an old client port while the link was healthy. **Unverified
  hypothesis:** with the old mapping dead, the server can only learn the new address from a
  client packet, and the client's sends back off exponentially (PTO) after repeated losses, so the
  next packet that reaches the server can arrive after the client's idle timer fires. The soak now
  rebinds every 1.5 s or more with a 30 s idle timeout, and passes 25 of 25. This is an extreme
  scenario, but it is a reason to keep §4.2's fallback to a new handshake.
- The `rcgen` and `ring` features are chosen so that `aws-lc-rs` and `aws-lc-sys` do not appear in
  `Cargo.lock`; `cargo deny` passes. `quinn` uses `default-features = false` with `runtime-tokio`
  and `rustls-ring`.

## Deviations from DESIGN.md, and gaps

- **§8.3 pinning:** the tests trust the single expected certificate through a root store. DESIGN.md
  says the daemon's *fingerprint* is pinned; the real transport needs a custom
  `ServerCertVerifier` that compares the fingerprint. Not done here (out of scope).
- **§4.2 liveness:** the design describes app-level heartbeats. These tests exercise QUIC's own
  idle timeout and keep-alive. Either can implement the 5 s rule; the idle timeout is simpler but
  only measures "no packets received", not RTT growth.
- **Not modelled:** MTU (quinn stays at 1200 because `may_fragment` is true), GSO/GRO, ECN,
  bandwidth limits or queues on the datagram link, IPv6, and a NAT that keeps its old mapping.
  `tc netem` on Linux CI (§12) is out of scope for this spike.
