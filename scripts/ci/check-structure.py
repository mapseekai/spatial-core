#!/usr/bin/env python3
"""Check standalone Cargo boundaries and one-file-per-source Rust test ownership."""
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
errors = []
package = manifest["package"]
for name in ("name", "version", "edition", "rust-version"):
    if not isinstance(package.get(name), str):
        errors.append(f"package.{name} must be explicit, not workspace-inherited")
if package.get("publish") is not False:
    errors.append("package.publish must remain false until publication is authorized")
if package.get("license-file") != "LICENSE" or not (ROOT / "LICENSE").is_file():
    errors.append("the original LICENSE must remain included")
if "workspace" in manifest or "workspace" in manifest.get("lints", {}):
    errors.append("standalone crate must not inherit a workspace")
for section in ("dependencies", "dev-dependencies", "build-dependencies"):
    for name, dependency in manifest.get(section, {}).items():
        if isinstance(dependency, dict) and any(
            key in dependency for key in ("workspace", "path", "git")
        ):
            errors.append(f"{section}.{name} must be a registry dependency")
for dependency in tomllib.loads((ROOT / "Cargo.lock").read_text())["package"]:
    if dependency["name"] == package["name"]:
        continue
    if not dependency.get("source", "").startswith("registry+"):
        errors.append(f"lockfile has non-registry dependency: {dependency['name']}")

owners = {}
for folder in ("src", "tests", "examples"):
    for path in (ROOT / folder).rglob("*.rs"):
        relative = path.relative_to(ROOT)
        if any(not re.fullmatch(r"[a-z][a-z0-9_]*", part) for part in relative.with_suffix("").parts):
            errors.append(f"Rust path is not snake_case: {relative}")
        source = None
        if folder == "tests":
            source = ROOT / "src" / path.relative_to(ROOT / "tests")
        elif folder == "src" and path.name.endswith("_test.rs"):
            source = path.with_name(path.name.removesuffix("_test.rs") + ".rs")
        if source is not None:
            if not source.is_file():
                errors.append(f"test has no matching source: {relative}")
            owners.setdefault(source, []).append(relative)
for source, tests in owners.items():
    if len(tests) > 1:
        errors.append(f"multiple independent test files for {source.relative_to(ROOT)}: {tests}")
if errors:
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
print("Standalone manifest, registry dependencies, Rust naming and test ownership: OK")
