# A local Linux VM for the root-only tests

The real-network harness (`docs/m1/realnet.md`) needs Linux, Docker and root: network namespaces, `tc netem`, `iptables` NAT, a privileged systemd container. A Mac has none of those, so until now the only way to run it was GitHub's `linux-check` workflow. This page describes a Lima VM that does the same on the developer's own machine, for the edit-and-rerun loop.

**GitHub's `linux-check` stays the x86_64 reference.** The VM is aarch64 on Apple silicon, so it runs the same scripts against `aarch64-unknown-linux-musl` binaries, on Ubuntu 24.04 with Lima's kernel. A pass in the VM is good evidence; a pass on GitHub is the record. Run `gh workflow run linux-check -f ref=BRANCH -f suite=realnet` before relying on a change that touches the harness itself.

## What the VM is

One Lima instance, `fil` (override the name with `FIL_VM`): Ubuntu 24.04, `vz` virtualization, 6 CPUs, 12 GiB, 40 GiB disk, with Docker, iproute2, iptables, the `sch_netem` module, `musl-tools`, `strace`, `shellcheck`, git and rustup. `just vm-setup` adds what the image lacks: `conntrack`, `openssh-server`, the pinned toolchain, `just` and `cargo-deny` (the versions `linux-check` pins).

## Isolation

The VM can reach the network, but it can see nothing of the Mac, and root in it is root in a throwaway machine. That is the point: the harness creates users, namespaces and a privileged container, and some of it runs as root with the host's kernel.

- **No host mounts** (`mounts: []`). Nothing on the Mac's disk is visible in the VM, and a bug in a test cannot write to it. Code goes in by `rsync` over Lima's own ssh connection (`just vm-sync`), which copies the working tree, uncommitted work included, to `~/src/<worktree name>`.
- **No port forwards.** The three catch-all `ignore` rules switch off Lima's automatic forwarding of guest ports to the Mac, so a service a scenario starts (an sshd, a daemon, a QUIC endpoint on a documentation address) is not reachable from the Mac's network.
- **No host ssh keys and no agent.** `loadDotSSHPubKeys: false` and `forwardAgent: false`: the VM does not get the Mac user's public keys, and a compromised VM cannot use the Mac's ssh agent. Lima's own key for the instance is the only way in, and it only opens this VM.
- **No containerd**, to keep the VM to what it is used for. Docker is the one container engine.

Do not add mounts or forwards to make something convenient. If a scenario needs a file, sync it.

## Creating it

Install Lima (`brew install lima`), save this as `fil.yaml` and run `limactl create --name=fil fil.yaml && limactl start fil`:

```yaml
# fil's Linux VM for agents: realnet (netns, tc netem, Docker) and native aarch64 builds.
# Sees nothing of the Mac: no mounts, no port forwards, no host ssh keys or agent.
vmType: vz
arch: aarch64
images:
- location: "https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-arm64.img"
  arch: aarch64
cpus: 6
memory: 12GiB
disk: 40GiB
mounts: []
portForwards:
- guestIP: 0.0.0.0
  guestPortRange:
  - 1
  - 65535
  proto: any
  ignore: true
- guestIP: 127.0.0.1
  guestPortRange:
  - 1
  - 65535
  proto: any
  ignore: true
- guestIPMustBeZero: false
  guestIP: '::'
  guestPortRange:
  - 1
  - 65535
  proto: any
  ignore: true
ssh:
  loadDotSSHPubKeys: false
  forwardAgent: false
  forwardX11: false
containerd:
  system: false
  user: false
provision:
- mode: system
  script: |
    #!/bin/bash
    set -eux
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -q
    apt-get install -y -q docker.io iproute2 iptables "linux-modules-extra-$(uname -r)" \
      build-essential pkg-config musl-tools strace git curl python3 shellcheck
    modprobe sch_netem
    echo sch_netem > /etc/modules-load.d/netem.conf
    usermod -aG docker "{{.User}}"
- mode: user
  script: |
    #!/bin/bash
    set -eux
    if [ ! -x "$HOME/.cargo/bin/rustup" ]; then
      curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
    fi
    "$HOME/.cargo/bin/rustup" target list --installed >/dev/null 2>&1 || true
```

Then, once, from a checkout: `just vm-setup`.

## Recipes

All of them are in the `justfile`, run on the Mac, and call `tests/realnet/vm.sh`. Each fails with a message if `limactl` is missing or the instance does not exist or is not running.

| Recipe | What it does |
|---|---|
| `just vm-setup` | Installs `conntrack`, `openssh-server`, the toolchain from `rust-toolchain.toml`, `just` and `cargo-deny` in the VM. Run once after creating it. |
| `just vm-sync` | `rsync` of this worktree to `~/src/<worktree name>` in the VM. It skips `.git`, `.claude`, `target/` and everything `.gitignore` ignores, and deletes files you deleted, so uncommitted work and untracked files arrive and a stale file does not linger. Unchanged files keep their modification time, so cargo rebuilds only what changed. |
| `just vm-run CMD...` | Sync, then run `CMD` in that directory in the VM with the pinned toolchain (installed on first use). `just vm-run just ci` is the merge gate on Linux; `just vm-run cargo test -p fil-transport` works too. |
| `just vm-realnet [PART] [SCENARIO...]` | Sync, then `just realnet ...` in the VM. Arguments as for `just realnet`: `udp`, `sysd`, `ssh` or none for all three, and scenario names. |
| `just vm-clean` | Removes this worktree's copy and build directory from the VM. Do it when a worktree is removed: each build directory is a few GiB. |

Three things `vm-run` and `vm-realnet` arrange:

- **`umask 022`.** Ubuntu's login default is `002`, and `fil-client`'s `offline` test makes its log directory with the process umask, so it came out group-writable and fil (correctly) refused it: `the_log_is_off_by_default_and_a_file_log_is_created_0600` failed deterministically in the VM. The recipes run with `022`, as the CI runners do.
- **One copy and one `CARGO_TARGET_DIR` per worktree** (`~/src/<name>`, `~/target/<name>`), so two agents on two branches never share build state. `tests/realnet/build.sh` honors `CARGO_TARGET_DIR`.
- **One realnet run at a time across the whole VM.** The harness uses fixed names (Docker containers and networks `rn-*`, namespaces `rn-*`, the users `rnclient` and `rnfil`), so two concurrent runs would break each other. `vm-realnet` holds an `flock` on `~/.fil-realnet.lock` for the whole run and says so if it has to wait. The run is killed after `FIL_VM_TIMEOUT` (default `60m`), which releases the lock.

Logs and per-scenario output are in `target/realnet` inside the synced directory in the VM (`just vm-run ls target/realnet/logs`), as on GitHub.

## What differs from GitHub

- **Architecture.** `build.sh` picks the musl target by `uname -m`, so the VM builds `aarch64-unknown-linux-musl` and GitHub builds `x86_64-unknown-linux-musl`. The Docker images are pinned by the multi-architecture index digest, so Docker pulls the arm64 variant here.
- **Kernel.** The guest runs Ubuntu's generic 6.8 kernel under Apple's virtualization framework (`vz`); GitHub's runners run a different (Azure) kernel build on different hardware. Timings are indicative only, and a scenario that sits on a timing boundary can differ (see below).

## Results in the VM

First runs on 2026-11-03 (Lima 2.2, Apple silicon, 6 CPUs, 12 GiB; GitHub reference: [run 41630436471](https://github.com/anthonytw/fil/actions/runs/41630436471), all three parts passed). Wall-clock per part includes the sync and the first, cold `cargo build`; later runs reuse the build.

| Part | VM result | Wall-clock |
|---|---|---|
| `ssh` | 24 of 24 scenarios pass | 2 min 32 s (Docker image build included) |
| `sysd` | 2 of 2 pass | 18 s |
| `udp` | 17 of 18 pass; `s_quiclink_blackouts` fails (below) | 10 min 28 s |

The measurements agree with the GitHub numbers in [realnet.md](../m1/realnet.md) where it matters (attach to a shell in 0.2 to 0.8 s, 0 of 10 lost detaches, a NAT rebind noticed in under 100 ms).

### `s_quiclink_blackouts`: the 3 s blackout does not reliably survive on the VM

The scenario cuts UDP for 3 s in the middle of a session and expects the link to survive (`dead_after` is 5 s). On the VM it usually does not: the daemon's link declares `Down(Dead)` at about 4.7 s after the cut (a sample-period bracket short of the 5 s limit), and the client, seeing the daemon close the connection, reconnects with a fresh handshake. About 7 runs in 8 fail this way, with `qlblackout-3.0s=LOST` and a very stable 4.67 s; the same scenario passes on GitHub's x86_64 runners, and passes every time when `tcpdump` is attached to the server's interface, so the outcome depends on the timing of the first packets after the network returns (the QUIC probe timeouts back off exponentially, and the first probe after the heal lands near the 4.7 s mark). The 4.5 s and 8 s cases still behave as expected (lost, with a fresh handshake back in about 0.2 s), and the other `quiclink` and `blackout` scenarios pass.

This is recorded, not hidden: the assertion is unchanged (`survive`) and the scenario shows `FAIL` in the VM. Whether a 3 s blackout should be survivable with reliably more margin is a question about the link's probe timing, not about the harness, and it is open.

`just vm-run just ci` passes in the VM (57 s after the first build, with the umask above).
