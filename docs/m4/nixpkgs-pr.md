# Draft: the nixpkgs PR for fil-shell

> **Status: a draft for after the 0.1.0 release (M4 PKG, `docs/m4/packaging.md` §5).** Nothing here is submitted. The PR is opened from a fork of nixpkgs by whoever maintains the package (owner question: who is listed in `maintainers`, which needs an entry in nixpkgs' `maintainers/maintainer-list.nix`, with a GitHub handle and a contact the owner chooses). The `hash` and `cargoHash` are filled in from a real build of the tag.

## Before opening it

- **The toolchain.** The workspace's `rust-version` is the pinned toolchain (1.99.1 today, `docs/m4/release.md` §1), and cargo refuses to build with an older rustc. nixpkgs' `rustc` was 1.99.0 at nixos-unstable `1d06008` (2026-11-03). Open the PR once nixpkgs carries the tag's toolchain or newer; nixpkgs builds Rust packages with its own `rustPlatform`, not with a pinned one.
- **The tests.** `doCheck = false` below, as in fil's own flake: the suite needs PTYs, sockets and real time, which the Nix sandbox does not give reliably. A reviewer may ask for a subset (`cargoTestFlags = [ "-p" "fil-proto" ]`, say), which runs offline and fast.
- **Check it** with `nix-build -A fil-shell` in the nixpkgs checkout, `nixpkgs-review pr <n>`, and `./result/bin/fil --version`.

## `pkgs/by-name/fi/fil-shell/package.nix`

```nix
{
  lib,
  rustPlatform,
  fetchFromGitHub,
  versionCheckHook,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "fil-shell";
  version = "0.1.0";

  src = fetchFromGitHub {
    owner = "anthonytw";
    repo = "fil";
    tag = "v${finalAttrs.version}";
    hash = lib.fakeHash;
  };

  cargoHash = lib.fakeHash;

  cargoBuildFlags = [
    "-p"
    "fil-shell"
    "--bin"
    "fil"
  ];

  # The workspace's tests need PTYs, sockets and real time.
  doCheck = false;

  nativeInstallCheckInputs = [ versionCheckHook ];
  doInstallCheck = true;
  versionCheckProgram = "${placeholder "out"}/bin/fil";

  meta = {
    description = "Remote shell for development over SSH that keeps your place";
    longDescription = ''
      fil keeps your place on a remote host: sessions survive reconnects, roaming and
      sleep, you can take one over from another machine, and port forwards outlive the
      link. It runs over ssh, or directly over QUIC.
    '';
    homepage = "https://github.com/anthonytw/fil";
    changelog = "https://github.com/anthonytw/fil/blob/v${finalAttrs.version}/CHANGELOG.md";
    license = lib.licenses.gpl3Plus;
    mainProgram = "fil";
    maintainers = [ ];
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
})
```

## The PR text

> **fil-shell: init at 0.1.0**
>
> [fil](https://github.com/anthonytw/fil) is a remote shell for development over SSH: sessions survive reconnects, roaming and sleep, can be taken over from another machine, and keep their port forwards across reconnects; it runs over an ssh pipe or directly over QUIC. GPL-3.0-or-later.
>
> The package is named `fil-shell`, as on crates.io (`fil` there is an unrelated crate); the program is `fil`. It builds the client from source; fil's release archives also carry static Linux binaries for `fil install` to upload to remote hosts, which this package does not include (a source build is not uploaded by `fil install`, which then asks for `--binary`).
>
> - Built on: x86_64-linux, aarch64-linux, aarch64-darwin (fill in what was tested)
> - `nixpkgs-review` result: (fill in)
> - Tested `fil --version` and an attach to a local daemon (`fil` with `pipe = ["fil", "server", "--stdio"]`)
>
> Things done:
> - [ ] Built on platform(s): …
> - [ ] Tested, as applicable: `versionCheckHook`
> - [ ] Ran `nixpkgs-review`
> - [ ] Tested basic functionality of the binary in `./result/bin/`
> - [ ] Fits [CONTRIBUTING.md](https://github.com/NixOS/nixpkgs/blob/master/CONTRIBUTING.md)

## After it merges

The nixpkgs update bot (`r-ryantm`) usually opens the version bumps itself; each one only needs a check that the new tag's toolchain is in nixpkgs. fil's own `flake.nix` stays, for building any commit with the pinned toolchain.
