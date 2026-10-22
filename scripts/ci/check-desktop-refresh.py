#!/usr/bin/env python3
"""Check that the desktop keeps polling after its initial request, without hardware.

Requires dbus-run-session and xvfb-run. The private mock IPC socket answers with a
recoverable daemon error, so this also checks retrying after a temporary failure.
"""

import asyncio
import json
import os
from pathlib import Path
import signal
import struct
import subprocess
import sys
import tempfile
import time


async def check(binary: Path) -> None:
    requests = 0

    async def answer(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        nonlocal requests
        try:
            size = struct.unpack(">I", await reader.readexactly(4))[0]
            await reader.readexactly(size)
            requests += 1
            # Bincode's DaemonResponse::Error(String): LE enum index + string length.
            message = b"temporary mock failure"
            response = struct.pack("<IQ", 3, len(message)) + message
            writer.write(struct.pack(">I", len(response)) + response)
            await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()

    with tempfile.TemporaryDirectory(prefix="linux-quest-refresh-") as directory:
        server = await asyncio.start_unix_server(answer, path=f"{directory}/daemon.sock")
        env = dict(os.environ)
        env.update(
            LINUX_QUEST_RUNTIME_DIR=directory,
            GDK_BACKEND="x11",
            GSK_RENDERER="cairo",
            GTK_A11Y="none",
        )
        # Inherited screenshot mode would make the app quit before this check finishes.
        env.pop("LINUX_QUEST_SCREENSHOT", None)
        with tempfile.TemporaryFile() as log:
            process = subprocess.Popen(
                ["dbus-run-session", "--", "xvfb-run", "-a", str(binary)],
                env=env,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            try:
                started = time.monotonic()
                for _ in range(30):
                    await asyncio.sleep(0.5)
                    if process.poll() is not None or requests >= 4:
                        break
                if process.poll() is not None or requests < 4:
                    log.seek(0)
                    raise AssertionError(
                        f"Desktop stopped refreshing: requests={requests}, "
                        f"exit={process.poll()}\n{log.read().decode(errors='replace')}"
                    )
                print(json.dumps({"requests": requests, "seconds": round(time.monotonic() - started, 2), "result": "passed"}))
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait()
                server.close()
                await server.wait_closed()


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(f"Usage: {sys.argv[0]} /path/to/linux-quest")
    asyncio.run(check(Path(sys.argv[1]).resolve(strict=True)))
