#!/usr/bin/env python3
"""Copy original dependency license notices from target-filtered locked Cargo metadata."""
import argparse
import json
from pathlib import Path
import shutil
import hashlib
from metadata_packages import resolved_packages


def collect(metadata, destination):
    packages = resolved_packages(metadata)
    upstream = Path(__file__).resolve().parent.parent / "packaging" / "windows" / "upstream-notices"
    overrides = json.loads((upstream / "index.json").read_text(encoding="utf-8")) if (upstream / "index.json").is_file() else {}
    destination.mkdir(parents=True, exist_ok=True)
    notices = []
    for package in sorted(packages, key=lambda item: (item["name"], item["version"])):
        if package["source"] is None:
            continue  # ScreenSense's own MIT license is at the archive root.
        root = Path(package["manifest_path"]).parent.resolve()
        files = [path for path in root.rglob("*") if path.is_file() and path.suffix.lower() not in (".rs", ".c", ".cpp", ".h", ".py", ".js", ".toml", ".json")
                 and (path.name.lower().startswith(("license", "licence", "copying", "copyright", "notice"))
                      or any(parent.name.lower() in ("licenses", "licences") for parent in path.relative_to(root).parents))]
        if package["license_file"]:
            declared = (root / package["license_file"]).resolve()
            if declared.is_file() and declared.is_relative_to(root):
                files.append(declared)
        files = sorted({path.resolve() for path in files if path.resolve().is_relative_to(root)})
        override = overrides.get(f"{package['name']}@{package['version']}")
        if not files and not override:
            raise ValueError(f"No source license notice found for {package['name']} {package['version']}")
        directory = destination / f"{package['name']}-{package['version']}"
        directory.mkdir(parents=True, exist_ok=True)
        names = []
        for path in files:
            relative = path.relative_to(root)
            target = directory / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, target)
            names.append(relative.as_posix())
        if override:
            vcs = json.loads((root / ".cargo_vcs_info.json").read_text(encoding="utf-8"))
            if vcs["git"]["sha1"] != override["commit"]:
                raise ValueError(f"Upstream notice revision mismatch for {package['name']}")
            for notice in override["files"]:
                original = (upstream / notice["file"]).resolve()
                if not original.is_relative_to(upstream.resolve()) or hashlib.sha256(original.read_bytes()).hexdigest() != notice["sha256"]:
                    raise ValueError(f"Upstream original notice hash mismatch for {package['name']}")
                target = directory / "upstream" / original.name
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(original, target)
                names.append(f"upstream/{original.name}")
        notices.append({"crate": package["name"], "version": package["version"],
                        "license": package["license"], "notices": names,
                        "upstream_notice_source": override})
    (destination / "index.json").write_text(json.dumps({
        "scope": "Target-filtered locked dependency set; includes build/dev dependencies, not an exact linked-code inventory.",
        "packages": notices,
    }, indent=2) + "\n", encoding="utf-8")
    return len(notices)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("metadata", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    print(f"Collected original/supplemental license notices for {collect(args.metadata, args.destination)} dependency packages.")
