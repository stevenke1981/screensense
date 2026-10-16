#!/usr/bin/env python3
"""Smoke-test a release archive using only fresh synthetic data and no screen observation."""
import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import struct
import subprocess
import tarfile
import tempfile
import zipfile


def extract(archive, destination):
    def target(name):
        result = (destination / name).resolve()
        if not result.is_relative_to(destination.resolve()):
            raise ValueError("archive member escapes extraction directory")
        return result

    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as packed:
            for member in packed.infolist():
                path = target(member.filename)
                if member.is_dir():
                    path.mkdir(parents=True, exist_ok=True)
                else:
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(packed.read(member))
    else:
        with tarfile.open(archive, "r:gz") as packed:
            for member in packed.getmembers():
                path = target(member.name)
                if member.isdir():
                    path.mkdir(parents=True, exist_ok=True)
                elif member.isfile():
                    path.parent.mkdir(parents=True, exist_ok=True)
                    with packed.extractfile(member) as source:
                        path.write_bytes(source.read())
                    path.chmod(member.mode & 0o755)
                else:
                    raise ValueError("release archive contains a link or special file")


def pe_imports(binary):
    raw = binary.read_bytes()
    pe = struct.unpack_from("<I", raw, 0x3C)[0]
    assert raw[pe : pe + 4] == b"PE\0\0"
    sections = struct.unpack_from("<H", raw, pe + 6)[0]
    optional_size = struct.unpack_from("<H", raw, pe + 20)[0]
    optional = pe + 24
    assert struct.unpack_from("<H", raw, optional)[0] == 0x20B, "expected Windows x64 PE"
    import_rva = struct.unpack_from("<I", raw, optional + 120)[0]
    section_table = optional + optional_size

    def offset(rva):
        for index in range(sections):
            entry = section_table + index * 40
            virtual_size, address, raw_size, file_offset = struct.unpack_from("<IIII", raw, entry + 8)
            if address <= rva < address + max(virtual_size, raw_size):
                return file_offset + rva - address
        raise ValueError("PE RVA has no section")

    found = []
    cursor = offset(import_rva)
    while any(raw[cursor : cursor + 20]):
        name = offset(struct.unpack_from("<I", raw, cursor + 12)[0])
        end = raw.index(b"\0", name)
        found.append(raw[name:end].decode("ascii"))
        cursor += 20
    return sorted(set(found), key=str.lower)


def workspace_version():
    """The `[workspace.package]` version, so the default follows Cargo.toml."""
    # No tomllib: the Linux release job runs Python 3.10.
    import re
    manifest = Path(__file__).resolve().parent.parent / "Cargo.toml"
    text = manifest.read_text(encoding="utf-8")
    section = text.split("[workspace.package]", 1)[1].split("\n[", 1)[0]
    return re.search(r'^version\s*=\s*"([^"]+)"', section, re.M).group(1)


def smoke(archive, expected_commit, expected_version):
    report = {"archive": archive.name, "sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
              "screen_observation": "disabled", "persistent_integration": False, "checks": []}
    with tempfile.TemporaryDirectory(prefix="screensense release smoke ") as temporary:
        root = Path(temporary)
        unpacked = root / "clean extract with spaces"
        unpacked.mkdir()
        extract(archive, unpacked)
        executable = unpacked / ("screensense.exe" if os.name == "nt" else "screensense")
        assert executable.is_file(), "archive has no native executable at its root"
        report["binary_sha256"] = hashlib.sha256(executable.read_bytes()).hexdigest()
        report["clean_os_installation_tested"] = False
        for name in ["README.md", "README.zh-TW.md", "LICENSE", "THIRD-PARTY-NOTICES.md", "SECURITY.md",
                     "CHANGELOG.md", "BUILD-INFO.txt", "docs/CONFIG.md", "docs/PRIVACY.md",
                     "docs/OPEN-ISSUES.md", "docs/WINDOWS-VERIFICATION.md", "skills/screen/SKILL.md", "LICENSES/index.json"]:
            assert (unpacked / name).is_file(), f"missing packaged documentation: {name}"
        notices = json.loads((unpacked / "LICENSES/index.json").read_text(encoding="utf-8"))["packages"]
        assert notices, "archive has no dependency notices"
        for package in notices:
            for name in package["notices"]:
                assert (unpacked / "LICENSES" / f"{package['crate']}-{package['version']}" / name).is_file()
        report["dependency_notice_packages"] = len(notices)
        build = dict(line.split("=", 1) for line in (unpacked / "BUILD-INFO.txt").read_text(encoding="utf-8-sig").splitlines() if "=" in line)
        assert build["commit"] == expected_commit, build
        assert build["version"].removeprefix("v") == expected_version, build
        report["build_info"] = build
        if os.name == "nt":
            report["pe_imports"] = pe_imports(executable)

        data = root / "synthetic data"
        config = root / "synthetic config" / "config.toml"
        home = root / "synthetic home"
        home.mkdir()
        env = dict(os.environ)
        env.update(SCREENSENSE_NO_SCREEN="1", SCREENSENSE_DATA=str(data), SCREENSENSE_CONFIG=str(config),
                   SCREENSENSE_LANG="en", HOME=str(home), USERPROFILE=str(home),
                   APPDATA=str(home / "AppData/Roaming"), LOCALAPPDATA=str(home / "AppData/Local"),
                   XDG_CONFIG_HOME=str(home / ".config"), XDG_DATA_HOME=str(home / ".local/share"))
        env.pop("SCREENSENSE_CHILD", None)
        env.pop("DISPLAY", None)
        env.pop("WAYLAND_DISPLAY", None)

        def run(args, code=0, stdin=None):
            result = subprocess.run([str(executable), *args], input=stdin, capture_output=True,
                                    text=True, encoding="utf-8", env=env, cwd=root, timeout=20)
            assert result.returncode == code, (args, result.returncode, result.stdout, result.stderr)
            report["checks"].append({"command": args, "exit": result.returncode})
            return result.stdout

        assert run(["--version"]).strip() == f"screensense {expected_version}"
        assert "Usage:" in run(["--help"])
        run(["config", "path"])
        run(["config", "show"])
        run(["config", "init"])
        run(["config", "check"])
        doctor = run(["doctor"])
        assert "null" in doctor and "screen observation disabled" in doctor, doctor
        run(["status"], code=3)
        run(["record", "--shell", "bash", "--cwd", "synthetic-fixture", "--", "cargo", "build", "--release"])
        assert "cargo build --release" in run(["commands"])
        run(["pause", "60"])

        def deadline():
            with closing(sqlite3.connect(data / "screensense.db")) as connection:
                return connection.execute("SELECT value FROM meta WHERE key='control.paused_until'").fetchone()[0]

        paused_until = deadline()
        requests = [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "synthetic-release-smoke", "version": "1"}}},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
            {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "screen_commands"}},
            {"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "screen_pause", "arguments": {"minutes": 1}}},
        ]
        responses = [json.loads(line) for line in run(["mcp"], stdin="".join(json.dumps(item) + "\n" for item in requests)).splitlines()]
        assert [item["id"] for item in responses] == [1, 2, 3, 4], responses
        names = {item["name"] for item in responses[1]["result"]["tools"]}
        assert names == {"screen_now", "screen_timeline", "screen_errors", "screen_search", "screen_text", "screen_commands", "screen_pause"}, names
        assert all(item["jsonrpc"] == "2.0" for item in responses)
        assert responses[3]["result"]["isError"] is False
        assert deadline() == paused_until, "agent shortened user's 60-minute pause"
        report["mcp_tools"] = sorted(names)
        run(["purge", "--all", "--yes"])
        assert deadline() == paused_until, "full purge cleared pause"
        assert "Recording paused until" in run(["status"], code=3)
        assert "cargo build --release" not in run(["commands"])
        config.write_text("[privacy]\nhidden_title_regex = [\"(broken\"]\n", encoding="utf-8")
        run(["config", "check"], code=1)
        run(["record", "--", "synthetic-blocked-command"])
        with closing(sqlite3.connect(data / "screensense.db")) as connection:
            assert connection.execute("SELECT count(*) FROM commands").fetchone()[0] == 0
        config.write_text("language = \"en\"\n", encoding="utf-8")
        run(["config", "check"])

        if os.name == "nt":
            snippet = run(["shell-init", "powershell"])
            parser = "$taskTokens=$null;$taskErrors=$null;[System.Management.Automation.Language.Parser]::ParseInput([Console]::In.ReadToEnd(),[ref]$taskTokens,[ref]$taskErrors)|Out-Null;if($taskErrors.Count){$taskErrors|ForEach-Object{[Console]::Error.WriteLine($_.Message)};exit 1};'PARSE_OK'"
            shells = [Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"]
            if shutil.which("pwsh"):
                shells.append(Path(shutil.which("pwsh")))
            for shell in shells:
                result = subprocess.run([str(shell), "-NoProfile", "-NonInteractive", "-Command", parser], input=snippet,
                                        text=True, encoding="utf-8", capture_output=True, env=env, timeout=20)
                assert result.returncode == 0 and "PARSE_OK" in result.stdout, (shell.name, result.stdout, result.stderr)
                report["checks"].append({"shell_parse_only": shell.name, "exit": 0})
        else:
            snippet = run(["shell-init", "bash"])
            result = subprocess.run(["bash", "-n"], input=snippet, text=True, capture_output=True, env=env, timeout=20)
            assert result.returncode == 0, result.stderr
            report["checks"].append({"shell_parse_only": "bash", "exit": 0})
        report["result"] = "passed"
        report["cleanup"] = "ephemeral MCP EOF exit; temporary extraction/config/data removed"
    return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--version", default=workspace_version())
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    result = smoke(args.archive.resolve(), args.commit, args.version)
    if args.output:
        args.output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, indent=2))
