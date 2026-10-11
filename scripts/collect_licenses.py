#!/usr/bin/env python3
"""Copy original dependency license notices from target-filtered locked Cargo metadata."""
import argparse
import json
from pathlib import Path
import shutil


def collect(metadata, destination):
    packages = json.loads(metadata.read_text(encoding="utf-8-sig"))["packages"]
    destination.mkdir(parents=True, exist_ok=True)
    notices = []
    for package in sorted(packages, key=lambda item: (item["name"], item["version"])):
        if package["source"] is None:
            continue  # ScreenSense's own MIT license is at the archive root.
        root = Path(package["manifest_path"]).parent.resolve()
        files = [path for path in root.iterdir() if path.is_file()
                 and path.name.lower().startswith(("license", "licence", "copying", "copyright", "notice"))]
        if package["license_file"]:
            declared = (root / package["license_file"]).resolve()
            if declared.is_file() and declared.is_relative_to(root):
                files.append(declared)
        files = sorted({path.resolve() for path in files if path.resolve().is_relative_to(root)})
        if not files:
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
        notices.append({"crate": package["name"], "version": package["version"],
                        "license": package["license"], "notices": names})
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
    print(f"Collected original license notices for {collect(args.metadata, args.destination)} dependency packages.")
