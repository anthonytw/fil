"""The workspace's crates in cargo-about's data (`just notices`; docs/m4/release.md section 7).

The nine published crates are not `publish = false`, so cargo-about lists them (about.toml,
[private]), and about.hbs leaves out every crate and every license entry whose first user has no
registry source. That is exact only if no license entry holds both workspace crates and registry
crates, and no license kind of the workspace's entries is also a registry crate's (the kind's
heading and overview line go with its first entry). This fails otherwise.

    python3 -I notice_workspace.py ABOUT.json   (cargo about generate --format json)
"""

import json
import sys


def main() -> int:
    with open(sys.argv[1], encoding="utf-8") as f:
        data = json.load(f)
    bad = []
    workspace_kinds = set()
    for lic in data["licenses"]:
        kinds = {u["crate"]["source"] is not None for u in lic["used_by"]}
        if len(kinds) > 1:
            names = ", ".join(u["crate"]["name"] for u in lic["used_by"])
            bad.append(f"{lic['id']}: {names}")
        if kinds == {False}:
            workspace_kinds.add(lic["id"])
    for lic in data["licenses"]:
        if lic["id"] in workspace_kinds and any(u["crate"]["source"] is not None for u in lic["used_by"]):
            bad.append(f"{lic['id']} is a license kind of both the workspace and a registry crate")
    if bad:
        print(
            "notices: a license entry mixes the workspace's crates with registry crates, "
            "which about.hbs would leave out: " + "; ".join(bad),
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
