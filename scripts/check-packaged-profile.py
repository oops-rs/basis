#!/usr/bin/env python3
"""Check the release profile that a crates.io CLI installation will read.

Run with Python 3.11 or newer. This inspects Cargo's normalized archive rather
than assuming the workspace profile is inherited by a published member.
"""

from pathlib import Path
import subprocess
import tarfile
import tempfile
import tomllib


def main() -> None:
    root = Path(__file__).resolve().parents[1]
    with (root / "Cargo.toml").open("rb") as manifest:
        workspace = tomllib.load(manifest)
    expected = workspace.get("profile", {}).get("release", {})
    required = {"strip", "opt-level", "lto", "codegen-units"}
    missing = required - expected.keys()
    if missing:
        raise SystemExit(f"workspace release profile is missing: {sorted(missing)}")
    if expected.get("panic", "unwind") != "unwind":
        raise SystemExit("workspace release profile must preserve panic unwinding")

    with tempfile.TemporaryDirectory(prefix="basis-packaged-profile-") as target:
        # No compilation or lockfile resolution: this must also work before the
        # workspace's sibling crate versions have been published to the registry.
        subprocess.run(
            [
                "cargo",
                "package",
                "-p",
                "basis-cli",
                "--no-verify",
                "--exclude-lockfile",
                "--allow-dirty",
                "--offline",
                "--target-dir",
                target,
            ],
            cwd=root,
            check=True,
        )
        archives = list((Path(target) / "package").glob("basis-cli-*.crate"))
        if len(archives) != 1:
            raise SystemExit(f"expected one CLI crate archive, found {len(archives)}")
        archive = archives[0]
        with tarfile.open(archive) as package:
            manifest = package.extractfile(f"{archive.stem}/Cargo.toml")
            if manifest is None:
                raise SystemExit("CLI crate archive has no Cargo.toml file")
            with manifest:
                packaged = tomllib.loads(manifest.read().decode("utf-8"))

    actual = packaged.get("profile", {}).get("release", {})
    if actual != expected:
        raise SystemExit(
            "packaged CLI release profile differs from the workspace profile; "
            "keep [profile.release] in basis-cli/Cargo.toml in sync with Cargo.toml"
            f"\nexpected: {expected}\npackaged: {actual}"
        )
    print(f"Packaged CLI release profile matches the workspace: {actual}")


if __name__ == "__main__":
    main()
