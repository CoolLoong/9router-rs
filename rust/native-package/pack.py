import argparse
from dataclasses import dataclass
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zipfile


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
MANIFEST = ROOT / "rust/app/Cargo.toml"
LICENSE_NAME = re.compile(r"^(?:licen[cs]e|notice|copyright|copying|unlicense|third.?party.?notices?)(?:[-_. ].*)?$", re.I)
TARGET = re.compile(r"[A-Za-z0-9_-]+")
VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?")


@dataclass(frozen=True)
class Member:
    name: str
    data: Path | bytes
    mode: int = 0o644

    def size(self):
        return self.data.stat().st_size if isinstance(self.data, Path) else len(self.data)

    def open(self):
        return self.data.open("rb") if isinstance(self.data, Path) else io.BytesIO(self.data)


def capture(*command):
    return subprocess.check_output(command, cwd=ROOT, text=True)


def json_bytes(value):
    return (json.dumps(value, indent=2, ensure_ascii=False) + "\n").encode()


def crate_licenses(package, source):
    files = {}
    for path in source.iterdir():
        if path.is_file() and LICENSE_NAME.fullmatch(path.name):
            files[path.name] = path
        elif path.is_dir() and path.name.lower() in {"license", "licenses"}:
            for entry in path.rglob("*"):
                if entry.is_file():
                    files[entry.relative_to(source).as_posix()] = entry
    if package["license_file"]:
        path = source / package["license_file"]
        if not path.is_file():
            raise ValueError(f"Missing declared license for {package['name']} {package['version']}")
        files.setdefault(path.name, path)
    if files:
        return files

    rules = json.loads((HERE / "licenses/supplements.json").read_text(encoding="utf-8"))
    for rule in rules:
        package_name = package["name"]
        if package["version"] != rule["version"] or not (
            package_name == rule["name"] or
            rule.get("family") and package_name.startswith(rule["name"] + "-")
        ):
            continue
        if "repository" in rule and package["repository"] != rule["repository"]:
            break
        if "revision" in rule:
            revision = json.loads((source / ".cargo_vcs_info.json").read_text(encoding="utf-8"))["git"]["sha1"]
            if revision != rule["revision"]:
                break
        license_path = HERE / "licenses" / rule["file"]
        files[license_path.name] = license_path
        for related in rule.get("include", []):
            files[related] = source / related
        return files
    raise ValueError(f"Missing license text for {package['name']} {package['version']}")


def dependency_notices(target, cargo):
    metadata = json.loads(capture(cargo[0], "metadata", *cargo[1:], "--format-version", "1",
                                  "--filter-platform", target))
    packages = {item["id"]: item for item in metadata["packages"]}
    resolve = metadata["resolve"]
    nodes = {item["id"]: item for item in resolve["nodes"]}
    pending = [resolve["root"]]
    visited = set()
    members, notices = [], []
    while pending:
        identifier = pending.pop()
        if identifier in visited:
            continue
        visited.add(identifier)
        pending.extend(dependency["pkg"] for dependency in nodes[identifier]["deps"]
                       if any(kind["kind"] != "dev" for kind in dependency["dep_kinds"]))
        package = packages[identifier]
        source = Path(package["manifest_path"]).parent
        if package["source"] is None and source.is_relative_to(ROOT / "rust"):
            continue
        name, version = package["name"], package["version"]
        content = {f"licenses/crates/{name}-{version}/{file}": path
                   for file, path in crate_licenses(package, source).items()}
        members.extend(Member(name, path) for name, path in content.items())
        notices.append({"name": name, "version": version, "license": package["license"],
                        "repository": package["repository"], "files": sorted(content)})
    notices.sort(key=lambda item: (item["name"], item["version"]))
    return members, notices


def archive_contents(binary, executable, version, target, cargo, toolchain_notice):
    members, notices = dependency_notices(target, cargo)
    assets = ROOT / "rust/app/src/pxpipe_assets"
    members.extend(Member(f"licenses/embedded/{path.name}", path) for path in assets.glob("*LICENSE*.txt"))
    if toolchain_notice is not None:
        members.append(Member(f"licenses/toolchain/{toolchain_notice.name}", toolchain_notice))
    with binary.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    members.extend([
        Member(executable, binary, 0o755),
        Member("README.txt", HERE / "package-readme.txt"),
        Member("LICENSE", ROOT / "LICENSE"),
        Member("licenses/CLI-LICENSE", ROOT / "cli/LICENSE"),
        Member("licenses/dependencies.json", json_bytes(notices)),
        Member("licenses/supplements.json", HERE / "licenses/supplements.json"),
        Member("manifest.json", json_bytes({
            "name": "9router", "version": version, "target": target,
            "kind": "native-preview", "executable": executable,
            "sha256": digest, "legacyRuntimeIncluded": False,
        })),
    ])
    if len({member.name for member in members}) != len(members):
        raise ValueError("Duplicate package member")
    return sorted(members, key=lambda member: member.name)


def write_archive(path, folder, members, windows):
    if windows:
        with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=6) as archive:
            for member in members:
                entry = zipfile.ZipInfo(f"{folder}/{member.name}", date_time=(1980, 1, 1, 0, 0, 0))
                entry.compress_type = zipfile.ZIP_DEFLATED
                entry.create_system = 3
                entry.external_attr = (0o100000 | member.mode) << 16
                with member.open() as reader, archive.open(entry, "w") as writer:
                    shutil.copyfileobj(reader, writer)
    else:
        with path.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0, compresslevel=6) as compressed:
            with tarfile.open(mode="w", fileobj=compressed) as archive:
                for member in members:
                    entry = tarfile.TarInfo(f"{folder}/{member.name}")
                    entry.mode = member.mode
                    entry.size = member.size()
                    with member.open() as reader:
                        archive.addfile(entry, reader)


def pack(args):
    target = args.target or next(line.removeprefix("host: ")
                                for line in capture("rustc", "-vV").splitlines() if line.startswith("host: "))
    if not TARGET.fullmatch(target):
        raise ValueError("Pass a Rust target triple")
    config = capture("rustc", "--print", "cfg", "--target", target).splitlines()
    platforms = {name for name in ("linux", "macos", "windows") if f'target_os="{name}"' in config}
    if len(platforms) != 1:
        raise ValueError("Native packages support Linux, macOS and Windows targets")
    windows = "windows" in platforms
    toolchain_notice = None
    if "linux" in platforms and {'target_env="musl"', 'target_feature="crt-static"'}.issubset(config):
        sysroot = Path(capture("rustc", "--print", "sysroot").strip())
        toolchain_notice = sysroot / "share/doc/rust/COPYRIGHT-library.html"
        if not toolchain_notice.is_file():
            raise ValueError(f"Missing Rust library notice: {toolchain_notice}. Install rust-docs for this toolchain.")
    version = json.loads((ROOT / "cli/package.json").read_text(encoding="utf-8"))["version"]
    if not VERSION.fullmatch(version):
        raise ValueError("Unsupported CLI package version")
    folder = f"9router-{version}-native-preview-{target}"
    directory = args.output.resolve()
    directory.mkdir(parents=True, exist_ok=True)
    output = directory / (folder + (".zip" if windows else ".tar.gz"))
    if output.exists():
        raise ValueError(f"Archive already exists: {output}")

    target_dir = args.target_dir.resolve()
    cargo = ("cargo", "--manifest-path", str(MANIFEST), "--offline", "--locked", "--features", "native-package")
    subprocess.run((cargo[0], "build", *cargo[1:], "--release", "--bin", "server",
                    "--target", target, "--target-dir", str(target_dir)), check=True, cwd=ROOT)
    binary = target_dir / target / "release" / ("server.exe" if windows else "server")
    executable = "9router.exe" if windows else "9router"
    members = archive_contents(binary, executable, version, target, cargo, toolchain_notice)
    with tempfile.TemporaryDirectory(dir=directory) as temporary:
        staged = Path(temporary) / "archive"
        write_archive(staged, folder, members, windows)
        if os.name == "nt":
            staged.rename(output)
        else:
            os.link(staged, output)
    return output


def main():
    parser = argparse.ArgumentParser(description="Build and archive the native 9router preview")
    parser.add_argument("--target", help="Rust target triple (default: compiler host)")
    parser.add_argument("--target-dir", type=Path, default=ROOT / "rust/app/target")
    parser.add_argument("--output", type=Path, default=HERE / "dist")
    try:
        print(pack(parser.parse_args()))
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
