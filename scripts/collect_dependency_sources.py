"""Bundle the original locked Cargo package sources used by the Windows build."""
import argparse
import json
from pathlib import Path
import zipfile
from metadata_packages import resolved_packages

parser = argparse.ArgumentParser()
parser.add_argument("metadata", type=Path)
parser.add_argument("output", type=Path)
args = parser.parse_args()
packages = resolved_packages(args.metadata)
index = []
with zipfile.ZipFile(args.output, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=6, strict_timestamps=False) as archive:
    for package in sorted(packages, key=lambda item: (item["name"], item["version"])):
        if package["source"] is None:
            continue
        root = Path(package["manifest_path"]).parent.resolve(strict=True)
        prefix = f"{package['name']}-{package['version']}"
        for file in sorted(root.rglob("*")):
            if file.is_file() and not file.is_symlink() and file.resolve().is_relative_to(root):
                archive.write(file, f"{prefix}/{file.relative_to(root).as_posix()}")
        index.append({"crate": package["name"], "version": package["version"],
                      "source": package["source"], "license": package["license"]})
    archive.writestr("SOURCE-INDEX.json", json.dumps(index, indent=2) + "\n")
    archive.writestr("README.txt", "Original Cargo registry sources for the locked Windows dependency set. "
                     "Includes build/dev dependencies. ScreenSense source, Cargo.lock and packaging instructions "
                     "are in the adjacent source.tar. This ZIP is an original source collection, not a configured "
                     "Cargo vendor directory. Cargo can obtain these same versions from their listed registries.\n")
print(f"Bundled sources for {len(index)} dependency packages.")
