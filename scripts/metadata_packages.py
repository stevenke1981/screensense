"""Select only packages reachable in Cargo's already platform-filtered graph."""
import json


def resolved_packages(path):
    metadata = json.loads(path.read_text(encoding="utf-8-sig"))
    if metadata.get("resolve") is None:
        raise ValueError("Full platform-filtered Cargo resolve metadata is required")
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    pending = list(metadata["workspace_members"])
    selected = set()
    while pending:
        package_id = pending.pop()
        if package_id in selected:
            continue
        selected.add(package_id)
        pending.extend(nodes[package_id]["dependencies"])
    packages = {package["id"]: package for package in metadata["packages"]}
    return [packages[package_id] for package_id in selected]
