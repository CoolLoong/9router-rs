import http.server
import io
import json
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
from collections import Counter
from pathlib import Path

runner = Path(sys.argv[1]).resolve()
mib = 1024 * 1024
elf = b"\x7fELF" + bytes(mib - 4)
macho = b"\xcf\xfa\xed\xfe" + bytes(mib - 4)
pe = b"MZ" + bytes(58) + (64).to_bytes(4, "little") + b"PE\0\0" + bytes(mib - 68)
idle = {"downloading": False, "progress": 0}
complete = {"downloading": False, "progress": 100}
observed = Counter()


def archive(entries):
    memory = io.BytesIO()
    with tarfile.open(fileobj=memory, mode="w:gz", compresslevel=1) as tar:
        for name, content in entries:
            entry = tarfile.TarInfo(name)
            if content is None:
                entry.type = tarfile.SYMTYPE
                entry.linkname = "../outside"
                tar.addfile(entry)
            elif isinstance(content, int):
                entry.size = content

                class Zeroes:
                    def read(self, size):
                        return bytes(size)

                tar.addfile(entry, Zeroes())
            else:
                entry.size = len(content)
                tar.addfile(entry, io.BytesIO(content))
    return memory.getvalue()


positive_archive = archive([("LICENSE", b"fixture"), ("./cloudflared", macho)])
content = {
    "/release/elf": elf,
    "/release/pe": pe,
    "/release/invalid-pe": b"MZ" + bytes(mib - 2),
    "/release/macho": positive_archive,
    "/release/traversal": archive([("./cloudflared", macho), ("../outside", b"fixture")]),
    "/release/absolute": archive([("./cloudflared", macho), ("/outside", b"fixture")]),
    "/release/link": archive([("./cloudflared", None)]),
    "/release/duplicate": archive([("cloudflared", macho), ("./cloudflared", macho)]),
    "/release/oversized-archive": archive([("./cloudflared", macho), ("huge", 160 * mib)]),
    "/release/truncated-archive": positive_archive[:-4],
    "/release/wrong-magic": b"HTML" + bytes(mib - 4),
    "/release/truncated-elf": elf[:80],
}


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_GET(self):
        observed[self.path] += 1
        assert self.headers["accept-encoding"] == "identity"
        assert self.headers.get("authorization") is None
        try:
            if self.path in ["/release/redirect", "/release/redirect-loop", "/release/cross-origin"]:
                target = {
                    "/release/redirect": "/release/elf",
                    "/release/redirect-loop": "/release/redirect-loop",
                    "/release/cross-origin": f"http://127.0.0.1:{other.server_port}/release/disallowed",
                }[self.path]
                self.send_response(302)
                self.send_header("Location", target)
                self.send_header("Content-Length", "0")
                self.end_headers()
            elif self.path == "/release/oversized":
                self.send_response(200)
                self.send_header("Content-Length", str(129 * mib))
                self.end_headers()
                self.close_connection = True
            elif self.path == "/release/chunked":
                self.send_response(200)
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                block = f"{mib:X}\r\n".encode() + bytes(mib) + b"\r\n"
                for _ in range(129):
                    self.wfile.write(block)
                self.wfile.write(b"0\r\n\r\n")
            elif self.path == "/release/cancel":
                self.send_response(200)
                self.send_header("Content-Length", str(len(elf)))
                self.end_headers()
                self.wfile.write(elf[:mib // 2])
                self.wfile.flush()
                time.sleep(.6)
                self.wfile.write(elf[mib // 2:])
            else:
                data = content.get(self.path, b"not a release")
                self.send_response(200 if self.path in content else 404)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError, TimeoutError):
            pass


with tempfile.TemporaryDirectory(prefix="9router-cloudflared-install-contract-") as folder:
    base = Path(folder)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    other = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    for instance in (server, other):
        threading.Thread(target=instance.serve_forever, daemon=True).start()

    def run(directory, route, platform="linux", mode="normal"):
        url = f"http://127.0.0.1:{server.server_port}/release/{route}"
        result = subprocess.run([runner, directory, url, platform, mode], check=True,
                                capture_output=True, text=True, timeout=20)
        return json.loads(result.stdout)

    def rejected(route, expected, platform="linux", prior=None):
        directory = base / route
        candidate = directory / ("cloudflared.exe" if platform == "windows" else "cloudflared")
        if prior is not None:
            directory.mkdir()
            candidate.write_bytes(prior)
        result = run(directory, route, platform)
        assert result == {"error": expected, "status": idle}, (route, result)
        if prior is not None:
            assert candidate.read_bytes() == prior
            assert list(directory.iterdir()) == [candidate]
        elif directory.exists():
            assert list(directory.iterdir()) == []
        assert not (base / "outside").exists()

    try:
        linux = base / "linux"
        binary = linux / "cloudflared"
        assert run(linux, "redirect") == {"file": "cloudflared", "status": complete}
        assert binary.read_bytes() == elf and binary.stat().st_mode & 0o777 == 0o755
        assert list(linux.iterdir()) == [binary]
        assert observed["/release/elf"] == 1
        binary.chmod(0o600)
        assert run(linux, "missing") == {"file": "cloudflared", "status": idle}
        assert binary.stat().st_mode & 0o777 == 0o755 and observed["/release/missing"] == 0

        windows = base / "windows"
        assert run(windows, "pe", "windows") == {"file": "cloudflared.exe", "status": complete}
        assert (windows / "cloudflared.exe").read_bytes() == pe
        darwin = base / "darwin"
        assert run(darwin, "macho", "darwin") == {"file": "cloudflared", "status": complete}
        assert (darwin / "cloudflared").read_bytes() == macho
        assert [path.name for path in darwin.iterdir()] == ["cloudflared"]

        error = "Cloudflared download was not a valid binary"
        rejected("wrong-magic", error, prior=b"existing incomplete download")
        rejected("invalid-pe", error, "windows")
        rejected("truncated-elf", error)
        for route in ["traversal", "absolute", "link", "duplicate", "oversized-archive", "truncated-archive"]:
            rejected(route, "Invalid cloudflared archive", "darwin")
        for route in ["oversized", "chunked"]:
            rejected(route, "Cloudflared download exceeded its size limit")
        rejected("redirect-loop", "Cloudflared release download was unavailable")
        assert observed["/release/redirect-loop"] == 6
        rejected("cross-origin", "Cloudflared download redirected outside the release hosts")
        assert observed["/release/disallowed"] == 0

        outside = base / "elsewhere"
        outside.write_bytes(elf)
        symlink_dir = base / "symlink"
        symlink_dir.mkdir()
        link = symlink_dir / "cloudflared"
        link.symlink_to(outside)
        assert run(symlink_dir, "elf") == {"file": "cloudflared", "status": complete}
        assert not link.is_symlink() and outside.stat().st_mode & 0o111 == 0
        assert outside.read_bytes() == elf

        cancelled = base / "cancelled"
        assert run(cancelled, "cancel", mode="cancel") == {"error": "tunnel cancelled", "status": idle}
        assert list(cancelled.iterdir()) == []
        print("Cloudflared fixture passed all 3 binary formats, allowlist, archive and streaming limits, cancellation, and atomic install")
    finally:
        for instance in (server, other):
            instance.shutdown()
            instance.server_close()
