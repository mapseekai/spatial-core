#!/usr/bin/env python3
"""Exercise build-path guards without network access or a real signing key."""
import os
from pathlib import Path
import subprocess
import tempfile

script = Path(__file__).resolve().with_name("build-duckdb-spatial.sh")
with tempfile.TemporaryDirectory(prefix="spatial-build-paths-") as temporary:
    root = Path(temporary).resolve()
    deployment = root / "deployment"
    deployment.mkdir()
    inside = deployment / "placeholder.pem"
    outside = root / "placeholder.pem"
    inside.write_text("test placeholder")
    outside.write_text("test placeholder")
    alias = root / "deployment-alias"
    alias.symlink_to(deployment, target_is_directory=True)
    key_alias = root / "key-alias.pem"
    key_alias.symlink_to(inside)
    sibling = root / "deployment-extra"
    sibling.mkdir()
    sibling_key = sibling / "placeholder.pem"
    sibling_key.write_text("test placeholder")
    commands = root / "bin"
    commands.mkdir()
    git = commands / "git"
    git.write_text("#!/bin/sh\necho offline-git-sentinel >&2\nexit 91\n")
    git.chmod(0o755)
    cases = [
        (deployment, inside, 2), (alias, inside, 2),
        (deployment, key_alias, 2), (Path("/"), inside, 2),
        (deployment, outside, 91), (deployment, sibling_key, 91),
    ]
    case_alias = root / "DEPLOYMENT" / inside.name
    if case_alias.exists() and case_alias.samefile(inside):
        cases.append((deployment, case_alias, 2))
    for output, key, expected in cases:
        result = subprocess.run(
            ["bash", str(script), str(root / "build"), str(output), str(key)],
            env={**os.environ, "PATH": str(commands) + os.pathsep + os.environ["PATH"]},
            capture_output=True, text=True,
        )
        assert result.returncode == expected, (output, key, result.returncode, result.stderr)
        message = "Keep the private key outside" if expected == 2 else "offline-git-sentinel"
        assert message in result.stderr, result.stderr
print(f"Build path guards: {len(cases)} cases passed")
