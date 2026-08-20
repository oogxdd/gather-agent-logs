"""Minimal Chrome DevTools Protocol client (stdlib only).

The ChatGPT desktop app is Chromium-based, so when it is started with
``--remote-debugging-port`` its page contexts become scriptable. Only what the
sync daemon needs is implemented: list targets, attach, evaluate JS.
"""

from __future__ import annotations

import base64
import json
import os
import socket
import struct
import urllib.error
import urllib.request


class CDPError(RuntimeError):
    pass


def http_json(path: str, port: int = 9222, timeout: float = 5.0):
    url = f"http://127.0.0.1:{port}{path}"
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:
            return json.load(response)
    except (urllib.error.URLError, OSError) as exc:
        raise CDPError(
            f"no debug port on 127.0.0.1:{port} ({exc}). "
            "Start the app with: open -a ChatGPT --args --remote-debugging-port=9222"
        ) from exc


class WebSocket:
    """Just enough RFC 6455 to talk to a CDP endpoint."""

    def __init__(self, url: str, timeout: float = 30.0):
        if not url.startswith("ws://"):
            raise CDPError(f"unexpected websocket url: {url}")
        hostport, _, path = url[len("ws://") :].partition("/")
        host, _, port = hostport.partition(":")
        self.sock = socket.create_connection((host, int(port or 80)), timeout=timeout)
        self.sock.settimeout(timeout)

        key = base64.b64encode(os.urandom(16)).decode()
        handshake = (
            f"GET /{path} HTTP/1.1\r\n"
            f"Host: {hostport}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(handshake.encode())

        buffer = b""
        while b"\r\n\r\n" not in buffer:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise CDPError("connection closed during handshake")
            buffer += chunk
        status_line = buffer.split(b"\r\n", 1)[0]
        if b" 101" not in status_line:
            raise CDPError(f"handshake rejected: {status_line!r}")
        self._buffer = buffer.split(b"\r\n\r\n", 1)[1]
        self._next_id = 0
        self.events: list[dict] = []

    def settimeout(self, timeout: float) -> None:
        self.sock.settimeout(timeout)

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass

    def __enter__(self) -> "WebSocket":
        return self

    def __exit__(self, *exc_info) -> None:
        self.close()

    def _read(self, count: int) -> bytes:
        while len(self._buffer) < count:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise CDPError("connection closed")
            self._buffer += chunk
        head, self._buffer = self._buffer[:count], self._buffer[count:]
        return head

    def _send_frame(self, opcode: int, payload: bytes) -> None:
        header = bytearray([0x80 | opcode])
        mask = os.urandom(4)
        length = len(payload)
        if length < 126:
            header.append(0x80 | length)
        elif length < 65536:
            header.append(0x80 | 126)
            header += struct.pack(">H", length)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", length)
        header += mask
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
        self.sock.sendall(bytes(header) + masked)

    def _recv_message(self) -> dict:
        """Reassemble one (possibly fragmented) text message."""
        chunks: list[bytes] = []
        while True:
            first, second = self._read(2)
            fin = bool(first & 0x80)
            opcode = first & 0x0F
            length = second & 0x7F
            if length == 126:
                length = struct.unpack(">H", self._read(2))[0]
            elif length == 127:
                length = struct.unpack(">Q", self._read(8))[0]
            payload = self._read(length)

            if opcode == 0x8:
                raise CDPError("closed by peer")
            if opcode == 0x9:  # ping -> pong, does not terminate a message
                self._send_frame(0xA, payload)
                continue
            if opcode == 0xA:  # pong
                continue

            chunks.append(payload)
            if fin:
                return json.loads(b"".join(chunks))

    def call(self, method: str, params: dict | None = None, session_id: str | None = None) -> dict:
        self._next_id += 1
        message_id = self._next_id
        message: dict = {"id": message_id, "method": method, "params": params or {}}
        if session_id:
            message["sessionId"] = session_id
        self._send_frame(0x1, json.dumps(message).encode())

        while True:
            reply = self._recv_message()
            if reply.get("id") == message_id:
                if "error" in reply:
                    raise CDPError(f"{method}: {reply['error']}")
                return reply.get("result", {})
            self.events.append(reply)


def evaluate(ws: WebSocket, expression: str, timeout: float | None = None):
    """Run an async JS expression in the attached context and return its value.

    The expression must evaluate to a promise resolving to a JSON string.
    """
    if timeout is not None:
        ws.settimeout(timeout)
    result = ws.call(
        "Runtime.evaluate",
        {"expression": expression, "returnByValue": True, "awaitPromise": True},
    )
    if result.get("exceptionDetails"):
        detail = result["exceptionDetails"]
        text = detail.get("exception", {}).get("description") or detail.get("text")
        raise CDPError(f"JS exception: {text}")
    value = result.get("result", {}).get("value")
    if value is None:
        raise CDPError("expression returned no value")
    return json.loads(value) if isinstance(value, str) else value
