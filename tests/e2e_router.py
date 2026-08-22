#!/usr/bin/env python3
"""Black-box Codex Model Router tests using loopback-only mock upstreams.

The suite deliberately uses only Python's standard library.  It never reads the
user's Codex configuration, credential vault, session database, or log files.
"""

from __future__ import annotations

import base64
import hashlib
import json
import os
import socket
import sqlite3
import struct
import subprocess
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
import uuid
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable


REPO_ROOT = Path(__file__).resolve().parents[1]
TEST_ACCOUNT_ID = "cmr-e2e-fixed-chatgpt-account"


class UpstreamState:
    """Thread-safe observations and counters for one fake upstream."""

    def __init__(self, kind: str, label: str) -> None:
        self.kind = kind
        self.label = label
        self.requests: list[dict[str, Any]] = []
        self._counter = 0
        self._lock = threading.Lock()

    def capture(
        self, method: str, path: str, headers: dict[str, str], body: Any = None
    ) -> None:
        with self._lock:
            self.requests.append(
                {"method": method, "path": path, "headers": headers, "body": body}
            )

    def next_id(self, prefix: str) -> str:
        with self._lock:
            self._counter += 1
            return f"{prefix}-{self._counter}"

    def snapshot(self) -> list[dict[str, Any]]:
        with self._lock:
            return list(self.requests)


def _message_text(message: dict[str, Any]) -> str:
    content = message.get("content")
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    parts: list[str] = []
    for part in content:
        if isinstance(part, str):
            parts.append(part)
        elif isinstance(part, dict) and isinstance(part.get("text"), str):
            parts.append(part["text"])
    return "".join(parts)


def _json_response(handler: BaseHTTPRequestHandler, status: int, value: Any) -> None:
    payload = json.dumps(value, separators=(",", ":")).encode("utf-8")
    _raw_response(handler, status, "application/json", payload)


def _raw_response(
    handler: BaseHTTPRequestHandler,
    status: int,
    content_type: str,
    payload: bytes,
) -> None:
    handler.send_response(status)
    handler.send_header("Content-Type", content_type)
    handler.send_header("Content-Length", str(len(payload)))
    handler.send_header("Connection", "close")
    handler.end_headers()
    if payload:
        handler.wfile.write(payload)


def _sse_response(handler: BaseHTTPRequestHandler, events: list[Any]) -> None:
    frames = []
    for event in events:
        data = event if isinstance(event, str) else json.dumps(event, separators=(",", ":"))
        frames.append(f"data: {data}\n\n")
    payload = "".join(frames).encode("utf-8")
    handler.send_response(200)
    handler.send_header("Content-Type", "text/event-stream")
    handler.send_header("Cache-Control", "no-cache")
    handler.send_header("Content-Length", str(len(payload)))
    handler.send_header("Connection", "close")
    handler.end_headers()
    handler.wfile.write(payload)


def _official_response(state: UpstreamState, body: dict[str, Any]) -> dict[str, Any]:
    response_id = state.next_id("official-response")
    model = body.get("model", "official-a")
    serialized_body = json.dumps(body, ensure_ascii=False, sort_keys=True)
    internal_operation = body.get("metadata", {}).get("cmr_internal_operation")
    if internal_operation == "portable_compaction_summary_v1":
        summary_text = (
            ""
            if "force-auto-compaction-summary-failure" in serialized_body
            else "portable automatic compaction summary"
        )
        return {
            "id": response_id,
            "object": "response",
            "created_at": 1_800_000_000,
            "status": "completed",
            "model": model,
            "output": [
                {
                    "type": "message",
                    "id": f"message-{response_id}",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": summary_text}],
                }
            ],
            "usage": {"input_tokens": 3, "output_tokens": 4, "total_tokens": 7},
        }
    if "force-auto-compaction" in serialized_body:
        compact_id = state.next_id("automatic-compaction")
        return {
            "id": response_id,
            "object": "response",
            "created_at": 1_800_000_000,
            "status": "completed",
            "model": model,
            "output": [
                {
                    "type": "compaction",
                    "id": compact_id,
                    "encrypted_content": f"opaque-{compact_id}",
                }
            ],
            "usage": {"input_tokens": 3, "output_tokens": 4, "total_tokens": 7},
        }
    incomplete = "force-official-incomplete" in json.dumps(
        body, ensure_ascii=False, sort_keys=True
    )
    response = {
        "id": response_id,
        "object": "response",
        "created_at": 1_800_000_000,
        "status": "incomplete" if incomplete else "completed",
        "model": model,
        "output": [
            {
                "type": "message",
                "id": f"message-{response_id}",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {
                        "type": "output_text",
                        "text": f"official continuation from {model}",
                    }
                ],
            }
        ],
        "usage": {"input_tokens": 3, "output_tokens": 4, "total_tokens": 7},
    }
    if incomplete:
        response["incomplete_details"] = {"reason": "max_output_tokens"}
    return response


def _external_response(state: UpstreamState, body: dict[str, Any]) -> dict[str, Any]:
    messages = body.get("messages", [])
    system_text = " ".join(
        _message_text(message)
        for message in messages
        if isinstance(message, dict) and message.get("role") == "system"
    )
    user_text = " ".join(
        _message_text(message)
        for message in messages
        if isinstance(message, dict) and message.get("role") == "user"
    )
    if "provider-neutral continuation summary" in system_text:
        message: dict[str, Any] = {
            "role": "assistant",
            "content": f"portable summary produced by {state.label}",
        }
        finish_reason = "stop"
    elif any(
        isinstance(item, dict) and item.get("role") == "tool" for item in messages
    ):
        message = {
            "role": "assistant",
            "content": f"{state.label} continued with {len(messages)} messages",
        }
        finish_reason = "stop"
    elif "call-tool" in user_text:
        message = {
            "role": "assistant",
            "content": None,
            "tool_calls": [
                {
                    "id": "call_a",
                    "type": "function",
                    "function": {
                        "name": "lookup",
                        "arguments": "{\"topic\":\"context\"}",
                    },
                }
            ],
        }
        finish_reason = "tool_calls"
    else:
        message = {
            "role": "assistant",
            "content": f"{state.label} continued with {len(messages)} messages",
        }
        finish_reason = "stop"
    return {
        "id": state.next_id("chat-completion"),
        "object": "chat.completion",
        "created": 1_800_000_000,
        "model": body.get("model", state.label),
        "choices": [
            {"index": 0, "message": message, "finish_reason": finish_reason}
        ],
        "usage": {"prompt_tokens": 5, "completion_tokens": 6, "total_tokens": 11},
    }


def _external_stream(state: UpstreamState, body: dict[str, Any]) -> list[Any]:
    stream_id = state.next_id("chat-stream")
    model = body.get("model", state.label)
    finish_reason = (
        "length"
        if "force-external-length" in json.dumps(
            body, ensure_ascii=False, sort_keys=True
        )
        else "stop"
    )
    return [
        {
            "id": stream_id,
            "object": "chat.completion.chunk",
            "created": 1_800_000_000,
            "model": model,
            "choices": [
                {
                    "index": 0,
                    "delta": {"role": "assistant", "content": "stream "},
                    "finish_reason": None,
                }
            ],
        },
        {
            "id": stream_id,
            "object": "chat.completion.chunk",
            "created": 1_800_000_000,
            "model": model,
            "choices": [
                {
                    "index": 0,
                    "delta": {"content": state.label},
                    "finish_reason": None,
                }
            ],
        },
        {
            "id": stream_id,
            "object": "chat.completion.chunk",
            "created": 1_800_000_000,
            "model": model,
            "choices": [
                {"index": 0, "delta": {}, "finish_reason": finish_reason}
            ],
            "usage": {"prompt_tokens": 2, "completion_tokens": 2, "total_tokens": 4},
        },
        "[DONE]",
    ]


def make_handler(state: UpstreamState) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, _format: str, *_args: Any) -> None:
            return

        def _headers(self) -> dict[str, str]:
            return {name.lower(): value for name, value in self.headers.items()}

        def _body(self) -> dict[str, Any]:
            length = int(self.headers.get("Content-Length", "0"))
            raw = self.rfile.read(length)
            if not raw:
                return {}
            value = json.loads(raw.decode("utf-8"))
            if not isinstance(value, dict):
                raise ValueError("mock expected a JSON object")
            return value

        def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
            headers = self._headers()
            state.capture("GET", self.path, headers)
            path = self.path.split("?", 1)[0]
            if (
                state.kind == "official"
                and path == "/responses"
                and headers.get("upgrade", "").lower() == "websocket"
            ):
                websocket_key = headers.get("sec-websocket-key")
                if not websocket_key:
                    _json_response(self, 400, {"error": {"message": "missing key"}})
                    return
                accept = base64.b64encode(
                    hashlib.sha1(
                        (
                            websocket_key
                            + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
                        ).encode("ascii")
                    ).digest()
                ).decode("ascii")
                self.send_response(101, "Switching Protocols")
                self.send_header("Upgrade", "websocket")
                self.send_header("Connection", "Upgrade")
                self.send_header("Sec-WebSocket-Accept", accept)
                self.end_headers()

                while True:
                    try:
                        opcode, payload = _receive_ws_frame(self.connection)
                    except (ConnectionError, OSError, TimeoutError):
                        break
                    if opcode == 8:
                        _send_ws_server_frame(self.connection, b"", opcode=8)
                        break
                    if opcode == 9:
                        _send_ws_server_frame(self.connection, payload, opcode=10)
                        continue
                    if opcode != 1:
                        continue
                    try:
                        body = json.loads(payload.decode("utf-8"))
                    except (UnicodeDecodeError, json.JSONDecodeError):
                        _send_ws_server_frame(
                            self.connection,
                            json.dumps(
                                {
                                    "type": "error",
                                    "code": "invalid_json",
                                    "message": "invalid JSON",
                                    "param": None,
                                    "sequence_number": 0,
                                },
                                separators=(",", ":"),
                            ).encode("utf-8"),
                        )
                        continue
                    if not isinstance(body, dict):
                        continue
                    state.capture("WS", path, headers, body)
                    response = _official_response(state, body)
                    in_progress = dict(response)
                    in_progress["status"] = "in_progress"
                    in_progress["output"] = []
                    events = [
                        {
                            "type": "response.created",
                            "sequence_number": 0,
                            "response": in_progress,
                        },
                        {
                            "type": "response.output_item.done",
                            "sequence_number": 1,
                            "output_index": 0,
                            "item": response["output"][0],
                        },
                        {
                            "type": (
                                "response.incomplete"
                                if response.get("status") == "incomplete"
                                else "response.completed"
                            ),
                            "sequence_number": 2,
                            "response": response,
                        },
                    ]
                    for event in events:
                        _send_ws_server_frame(
                            self.connection,
                            json.dumps(event, separators=(",", ":")).encode("utf-8"),
                        )
                self.close_connection = True
                return
            if state.kind == "official" and path == "/models":
                auth_gate_case = headers.get("openai-cmr-test-catalog")
                if auth_gate_case == "no-content":
                    _raw_response(self, 204, "application/json", b"")
                    return
                if auth_gate_case == "malformed-json":
                    _raw_response(self, 200, "application/json", b'{"models":[')
                    return
                if auth_gate_case == "missing-models":
                    _json_response(self, 200, {"data": []})
                    return
                _json_response(
                    self,
                    200,
                    {
                        "models": [
                            {"slug": "official-a", "display_name": "Official A"},
                            {
                                "slug": "official-hidden",
                                "display_name": "Hidden Official",
                            },
                            {"slug": "official-b", "display_name": "Official B"},
                        ]
                    },
                )
                return
            _json_response(self, 404, {"error": {"message": "not found"}})

        def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
            try:
                body = self._body()
            except (ValueError, json.JSONDecodeError):
                _json_response(self, 400, {"error": {"message": "invalid JSON"}})
                return
            headers = self._headers()
            state.capture("POST", self.path, headers, body)
            path = self.path.split("?", 1)[0]
            if state.kind == "official" and path == "/responses/compact":
                compact_id = state.next_id("encrypted-compaction")
                _json_response(
                    self,
                    200,
                    {
                        "id": state.next_id("compact-response"),
                        "object": "response",
                        "created_at": 1_800_000_000,
                        "status": "completed",
                        "model": body.get("model", "official-a"),
                        "output": [
                            {
                                "type": "compaction",
                                "id": compact_id,
                                "encrypted_content": f"opaque-{compact_id}",
                            }
                        ],
                    },
                )
                return
            if state.kind == "official" and path == "/responses":
                response = _official_response(state, body)
                if body.get("stream") is True:
                    in_progress = dict(response)
                    in_progress["status"] = "in_progress"
                    in_progress["output"] = []
                    item = response["output"][0]
                    _sse_response(
                        self,
                        [
                            {
                                "type": "response.created",
                                "sequence_number": 0,
                                "response": in_progress,
                            },
                            {
                                "type": "response.output_item.done",
                                "sequence_number": 1,
                                "output_index": 0,
                                "item": item,
                            },
                            {
                                "type": (
                                    "response.incomplete"
                                    if response.get("status") == "incomplete"
                                    else "response.completed"
                                ),
                                "sequence_number": 2,
                                "response": response,
                            },
                        ],
                    )
                else:
                    _json_response(self, 200, response)
                return
            if state.kind == "external" and path == "/chat/completions":
                if body.get("stream") is True:
                    _sse_response(self, _external_stream(state, body))
                else:
                    _json_response(self, 200, _external_response(state, body))
                return
            _json_response(self, 404, {"error": {"message": "not found"}})

    return Handler


def start_mock(state: UpstreamState) -> tuple[ThreadingHTTPServer, threading.Thread]:
    server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread


def stop_mock(server: ThreadingHTTPServer, thread: threading.Thread) -> None:
    try:
        server.shutdown()
    finally:
        server.server_close()
        thread.join(timeout=5)
    if thread.is_alive():
        raise RuntimeError("mock upstream thread did not stop")


def reserve_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


def binary_path() -> Path:
    configured = os.environ.get("CMR_BIN")
    if configured:
        candidate = Path(configured).resolve()
        if candidate.is_file():
            return candidate
        raise FileNotFoundError(f"CMR_BIN does not name a file: {candidate}")
    target = Path(os.environ.get("CARGO_TARGET_DIR", REPO_ROOT / "target"))
    executable = "cmr.exe" if os.name == "nt" else "cmr"
    candidate = target / "debug" / executable
    if candidate.is_file():
        return candidate
    raise FileNotFoundError(
        f"router binary not found at {candidate}; run `cargo build -p cmr-cli` first"
    )


def router_config(
    router_port: int,
    official_port: int,
    external_a_port: int,
    external_b_port: int,
    *,
    collision: bool = False,
) -> str:
    # The collision fixture deliberately reuses a visible official ID.  It
    # verifies that a publishable external mapping can never impersonate an
    # entry from the authenticated official catalog.
    first_external_id = "official-a" if collision else "external-a"
    order = (
        '["external-b", "official-b", "official-a"]'
        if collision
        else '["external-b", "official-b", "external-a", "official-a"]'
    )
    return f'''version = 1
official_base_url = "http://127.0.0.1:{official_port}"
picker_capacity = 4
compatibility_policy = "warn"
catalog_order = {order}
hidden_models = ["official-hidden", "external-hidden"]
official_compaction_model = "official-a"

[server]
host = "127.0.0.1"
port = {router_port}
max_body_bytes = 16777216

[[providers]]
id = "official"
preset = "openai-responses"
enabled = true

[[providers]]
id = "provider-a"
preset = "ollama"
base_url = "http://127.0.0.1:{external_a_port}"
enabled = true

[[providers]]
id = "provider-b"
preset = "ollama"
base_url = "http://127.0.0.1:{external_b_port}"
enabled = true

[[providers]]
id = "provider-disabled"
preset = "ollama"
base_url = "http://127.0.0.1:{external_a_port}"
enabled = false

[[models]]
id = "{first_external_id}"
display_name = "External A"
provider = "provider-a"
upstream_model = "upstream-a"
order = 20
enabled = true
context_window = 200000
max_output_tokens = 16000

[[models]]
id = "external-b"
display_name = "External B"
provider = "provider-b"
upstream_model = "upstream-b"
order = 10
enabled = true
context_window = 200000
max_output_tokens = 16000

[[models]]
id = "external-hidden"
display_name = "External Hidden"
provider = "provider-a"
upstream_model = "upstream-hidden"
order = 30
enabled = true
context_window = 200000
max_output_tokens = 16000

[[models]]
id = "external-disabled"
display_name = "External Disabled"
provider = "provider-disabled"
upstream_model = "upstream-disabled"
order = 40
enabled = true
context_window = 200000
max_output_tokens = 16000
'''


def start_router(
    executable: Path, config_path: Path, state_db: Path
) -> subprocess.Popen[bytes]:
    environment = dict(os.environ)
    environment["RUST_LOG"] = "off"
    return subprocess.Popen(
        [
            str(executable),
            "--config",
            str(config_path),
            "--state-db",
            str(state_db),
            "serve",
        ],
        cwd=REPO_ROOT,
        env=environment,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )


def stop_router(process: subprocess.Popen[bytes]) -> None:
    try:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
    finally:
        for stream in (process.stdout, process.stderr):
            if stream is not None:
                stream.close()


def http_request(
    base_url: str,
    path: str,
    *,
    method: str = "GET",
    value: Any = None,
    headers: dict[str, str] | None = None,
    timeout: float = 8,
) -> tuple[int, dict[str, str], bytes]:
    payload = None
    request_headers = dict(headers or {})
    if value is not None:
        payload = json.dumps(value, separators=(",", ":")).encode("utf-8")
        request_headers["Content-Type"] = "application/json"
    request = urllib.request.Request(
        f"{base_url}{path}",
        data=payload,
        headers=request_headers,
        method=method,
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return (
            int(response.status),
            {name.lower(): value for name, value in response.headers.items()},
            response.read(),
        )


def wait_for_router(base_url: str, process: subprocess.Popen[bytes]) -> None:
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        if process.poll() is not None:
            diagnostic = b""
            if process.stderr is not None:
                diagnostic = process.stderr.read(4096)
            raise RuntimeError(
                "router exited during startup: "
                + diagnostic.decode("utf-8", errors="replace")
            )
        try:
            status, _, _ = http_request(base_url, "/health", timeout=0.5)
            if status == 200:
                return
        except (OSError, urllib.error.URLError):
            time.sleep(0.1)
    raise TimeoutError("router did not become healthy on its temporary loopback port")


def parse_json_body(payload: bytes) -> Any:
    return json.loads(payload.decode("utf-8"))


def parse_sse(payload: bytes) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    normalized = payload.decode("utf-8").replace("\r\n", "\n")
    for frame in normalized.split("\n\n"):
        lines = []
        for line in frame.splitlines():
            if line.startswith("data:"):
                lines.append(line[5:].lstrip())
        if not lines:
            continue
        data = "\n".join(lines)
        if data == "[DONE]":
            continue
        value = json.loads(data)
        if isinstance(value, dict):
            events.append(value)
    return events


def _recv_exact(connection: socket.socket, length: int) -> bytes:
    chunks = bytearray()
    while len(chunks) < length:
        chunk = connection.recv(length - len(chunks))
        if not chunk:
            raise ConnectionError("WebSocket closed before the frame completed")
        chunks.extend(chunk)
    return bytes(chunks)


def _send_ws_frame(connection: socket.socket, payload: bytes, opcode: int = 1) -> None:
    first = 0x80 | opcode
    mask = os.urandom(4)
    length = len(payload)
    if length < 126:
        header = bytes([first, 0x80 | length])
    elif length < 65536:
        header = bytes([first, 0x80 | 126]) + struct.pack("!H", length)
    else:
        header = bytes([first, 0x80 | 127]) + struct.pack("!Q", length)
    masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    connection.sendall(header + mask + masked)


def _send_ws_server_frame(
    connection: socket.socket, payload: bytes, opcode: int = 1
) -> None:
    first = 0x80 | opcode
    length = len(payload)
    if length < 126:
        header = bytes([first, length])
    elif length < 65536:
        header = bytes([first, 126]) + struct.pack("!H", length)
    else:
        header = bytes([first, 127]) + struct.pack("!Q", length)
    connection.sendall(header + payload)


def _receive_ws_frame(connection: socket.socket) -> tuple[int, bytes]:
    first, second = _recv_exact(connection, 2)
    opcode = first & 0x0F
    masked = bool(second & 0x80)
    length = second & 0x7F
    if length == 126:
        length = struct.unpack("!H", _recv_exact(connection, 2))[0]
    elif length == 127:
        length = struct.unpack("!Q", _recv_exact(connection, 8))[0]
    mask = _recv_exact(connection, 4) if masked else b""
    payload = _recv_exact(connection, length)
    if masked:
        payload = bytes(
            byte ^ mask[index % 4] for index, byte in enumerate(payload)
        )
    return opcode, payload


def websocket_event_batches(
    host: str,
    port: int,
    request_values: list[
        dict[str, Any]
        | Callable[[list[list[dict[str, Any]]]], dict[str, Any]]
    ],
    *,
    headers: dict[str, str] | None = None,
) -> list[list[dict[str, Any]]]:
    connection = socket.create_connection((host, port), timeout=8)
    connection.settimeout(8)
    try:
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        extra_headers = ""
        for name, value in (headers or {}).items():
            if "\r" in name or "\n" in name or "\r" in value or "\n" in value:
                raise ValueError("WebSocket test headers cannot contain newlines")
            extra_headers += f"{name}: {value}\r\n"
        handshake = (
            "GET /v1/responses HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            f"{extra_headers}\r\n"
        ).encode("ascii")
        connection.sendall(handshake)
        response = bytearray()
        while b"\r\n\r\n" not in response:
            response.extend(connection.recv(4096))
        header, remainder = bytes(response).split(b"\r\n\r\n", 1)
        if not header.startswith(b"HTTP/1.1 101"):
            raise AssertionError("WebSocket upgrade did not return HTTP 101")
        expected = base64.b64encode(
            hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")
            ).digest()
        )
        accept_lines = [
            line.split(b":", 1)[1].strip()
            for line in header.split(b"\r\n")[1:]
            if line.lower().startswith(b"sec-websocket-accept:")
        ]
        if accept_lines != [expected]:
            raise AssertionError("WebSocket accept digest was invalid")
        if remainder:
            raise AssertionError("unexpected data arrived with the WebSocket handshake")

        batches: list[list[dict[str, Any]]] = []
        for request_value in request_values:
            if callable(request_value):
                request_value = request_value(batches)
            _send_ws_frame(
                connection,
                json.dumps(request_value, separators=(",", ":")).encode("utf-8"),
            )
            events: list[dict[str, Any]] = []
            while len(events) < 64:
                opcode, payload = _receive_ws_frame(connection)
                if opcode == 9:
                    _send_ws_frame(connection, payload, opcode=10)
                    continue
                if opcode == 8:
                    raise ConnectionError(
                        "WebSocket closed before the response reached a terminal event"
                    )
                if opcode != 1:
                    continue
                event = json.loads(payload.decode("utf-8"))
                if not isinstance(event, dict):
                    raise AssertionError("router WebSocket emitted a non-object event")
                events.append(event)
                if event.get("type") in {
                    "response.completed",
                    "response.incomplete",
                    "error",
                }:
                    break
            else:
                raise AssertionError("router WebSocket emitted too many non-terminal events")
            batches.append(events)
        return batches
    finally:
        connection.close()


def websocket_events(
    host: str,
    port: int,
    request_value: dict[str, Any],
    *,
    headers: dict[str, str] | None = None,
) -> list[dict[str, Any]]:
    return websocket_event_batches(host, port, [request_value], headers=headers)[0]


class RouterEndToEndTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.executable = binary_path()
        cls.temp = tempfile.TemporaryDirectory(prefix="cmr-e2e-")
        cls.addClassCleanup(cls.temp.cleanup)
        cls.temp_path = Path(cls.temp.name)

        cls.official_state = UpstreamState("official", "official")
        cls.external_a_state = UpstreamState("external", "provider-a")
        cls.external_b_state = UpstreamState("external", "provider-b")
        cls.mock_servers = []
        for state in (
            cls.official_state,
            cls.external_a_state,
            cls.external_b_state,
        ):
            mock = start_mock(state)
            cls.mock_servers.append(mock)
            cls.addClassCleanup(stop_mock, *mock)
        cls.official_port = cls.mock_servers[0][0].server_port
        cls.external_a_port = cls.mock_servers[1][0].server_port
        cls.external_b_port = cls.mock_servers[2][0].server_port

    def setUp(self) -> None:
        # Each case owns its router process and SQLite database.  This makes the
        # suite independent of unittest's name ordering and ensures a restart or
        # failed assertion cannot leave later cases with a stopped/shared router.
        fixture_id = f"{self._testMethodName}-{uuid.uuid4().hex}"
        self.router_port = reserve_port()
        self.base_url = f"http://127.0.0.1:{self.router_port}"
        self.config_path = self.temp_path / f"{fixture_id}.toml"
        self.state_db = self.temp_path / f"{fixture_id}.sqlite3"
        self.config_path.write_text(
            router_config(
                self.router_port,
                self.official_port,
                self.external_a_port,
                self.external_b_port,
            ),
            encoding="utf-8",
        )
        self.process: subprocess.Popen[bytes] | None = start_router(
            self.executable, self.config_path, self.state_db
        )
        self.addCleanup(self.stop_test_router)
        wait_for_router(self.base_url, self.process)

        # Every test must be runnable on its own. Prime the authenticated
        # official catalog in its isolated router instead of relying on another
        # case to establish the account binding and model allow-list.
        status, _, payload = http_request(
            self.base_url,
            "/v1/models?client_version=e2e-setup",
            headers=self.request_headers(),
        )
        if status != 200:
            raise AssertionError(f"catalog preflight returned HTTP {status}")
        catalog = parse_json_body(payload)
        if not isinstance(catalog, dict) or not isinstance(
            catalog.get("models"), list
        ):
            raise AssertionError("catalog preflight returned an invalid model list")
        catalog_ids = {
            model.get("slug")
            for model in catalog["models"]
            if isinstance(model, dict)
        }
        if not {"official-a", "external-a", "external-b"}.issubset(catalog_ids):
            raise AssertionError("catalog preflight omitted required test models")

    def stop_test_router(self) -> None:
        process = self.process
        self.process = None
        if process is not None:
            stop_router(process)

    @classmethod
    def request_headers(cls) -> dict[str, str]:
        # Authorization sentinels are ephemeral, while the stable account id
        # exercises the router's persistent same-workspace TOFU binding.
        return {
            "Authorization": "Bearer " + uuid.uuid4().hex,
            "ChatGPT-Account-ID": TEST_ACCOUNT_ID,
        }

    def post_response(self, value: dict[str, Any]) -> dict[str, Any]:
        status, _, payload = http_request(
            self.base_url,
            "/v1/responses",
            method="POST",
            value=value,
            headers=self.request_headers(),
        )
        self.assertEqual(status, 200)
        response = parse_json_body(payload)
        self.assertIsInstance(response, dict)
        return response

    def test_01_catalog_order_hiding_capacity_and_unique_ids(self) -> None:
        status, _, payload = http_request(
            self.base_url, "/v1/models", headers=self.request_headers()
        )
        self.assertEqual(status, 200)
        catalog = parse_json_body(payload)
        ids = [model["slug"] for model in catalog["models"]]
        self.assertEqual(
            ids, ["external-b", "official-b", "external-a", "official-a"]
        )
        self.assertEqual(len(ids), len(set(ids)))
        self.assertNotIn("official-hidden", ids)
        self.assertNotIn("external-hidden", ids)
        self.assertNotIn("external-disabled", ids)
        self.assertEqual(
            [model["priority"] for model in catalog["models"]], list(range(4))
        )

        health_status, _, health_payload = http_request(self.base_url, "/health")
        self.assertEqual(health_status, 200)
        health = parse_json_body(health_payload)
        self.assertEqual(health["external_models"], ["external-a", "external-b"])

        collision_port = reserve_port()
        collision_config = self.temp_path / "collision.toml"
        collision_db = self.temp_path / "collision.sqlite3"
        collision_config.write_text(
            router_config(
                collision_port,
                self.mock_servers[0][0].server_port,
                self.mock_servers[1][0].server_port,
                self.mock_servers[2][0].server_port,
                collision=True,
            ),
            encoding="utf-8",
        )
        collision_process = start_router(self.executable, collision_config, collision_db)
        collision_url = f"http://127.0.0.1:{collision_port}"
        try:
            wait_for_router(collision_url, collision_process)
            with self.assertRaises(urllib.error.HTTPError) as raised:
                http_request(
                    collision_url, "/v1/models", headers=self.request_headers()
                )
            self.assertEqual(raised.exception.code, 400)
            raised.exception.close()
        finally:
            stop_router(collision_process)

    def test_02_official_external_external_official_context_and_tools(self) -> None:
        session_id = "e2e-session-" + uuid.uuid4().hex
        first = self.post_response(
            {
                "model": "official-a",
                "stream": False,
                "metadata": {"cmr_session_id": session_id},
                "input": "root context",
            }
        )
        second = self.post_response(
            {
                "model": "external-a",
                "stream": False,
                "previous_response_id": first["id"],
                "input": "call-tool while retaining context",
                "tools": [
                    {
                        "type": "function",
                        "name": "lookup",
                        "description": "lookup a topic",
                        "parameters": {
                            "type": "object",
                            "properties": {"topic": {"type": "string"}},
                            "required": ["topic"],
                        },
                    }
                ],
            }
        )
        function_calls = [
            item for item in second["output"] if item.get("type") == "function_call"
        ]
        self.assertEqual(len(function_calls), 1)
        self.assertEqual(function_calls[0]["call_id"], "call_a")
        self.assertEqual(function_calls[0]["name"], "lookup")

        third = self.post_response(
            {
                "model": "external-b",
                "stream": False,
                "previous_response_id": second["id"],
                "input": [
                    {
                        "type": "function_call_output",
                        "call_id": "call_a",
                        "output": {"result": "tool-ok"},
                    }
                ],
            }
        )
        fourth = self.post_response(
            {
                "model": "official-b",
                "stream": False,
                "previous_response_id": third["id"],
                "input": "back on the official model",
            }
        )
        self.assertEqual(fourth["model"], "official-b")

        external_a_request = self.external_a_state.snapshot()[-1]["body"]
        external_a_text = json.dumps(external_a_request, sort_keys=True)
        self.assertIn("root context", external_a_text)
        self.assertIn("official continuation", external_a_text)
        self.assertIn("call-tool while retaining context", external_a_text)

        external_b_request = self.external_b_state.snapshot()[-1]["body"]
        external_b_messages = external_b_request["messages"]
        tool_messages = [
            message
            for message in external_b_messages
            if isinstance(message, dict) and message.get("role") == "tool"
        ]
        self.assertEqual(len(tool_messages), 1)
        self.assertEqual(tool_messages[0]["tool_call_id"], "call_a")
        self.assertIn("tool-ok", _message_text(tool_messages[0]))
        self.assertIn("call_a", json.dumps(external_b_request, sort_keys=True))

        official_posts = [
            request
            for request in self.official_state.snapshot()
            if request["method"] == "POST" and request["path"] == "/responses"
        ]
        final_official_request = official_posts[-1]["body"]
        final_text = json.dumps(final_official_request, sort_keys=True)
        self.assertNotIn("previous_response_id", final_official_request)
        for marker in (
            "root context",
            "call-tool while retaining context",
            "call_a",
            "tool-ok",
            "provider-b continued",
            "back on the official model",
        ):
            self.assertIn(marker, final_text)

        with closing(sqlite3.connect(self.state_db)) as connection:
            response_rows = connection.execute(
                "SELECT id, session_id, model_id FROM responses WHERE id IN (?, ?, ?, ?)",
                (first["id"], second["id"], third["id"], fourth["id"]),
            ).fetchall()
            switches = connection.execute(
                "SELECT from_model, to_model FROM model_switches WHERE session_id = ? "
                "ORDER BY created_at, rowid",
                (session_id,),
            ).fetchall()
        self.assertEqual(len(response_rows), 4)
        self.assertEqual({row[1] for row in response_rows}, {session_id})
        self.assertEqual(
            switches,
            [
                ("official/official-a", "provider-a/external-a"),
                ("provider-a/external-a", "provider-b/external-b"),
                ("provider-b/external-b", "official/official-b"),
            ],
        )

    def test_03_http_sse_has_complete_responses_lifecycle(self) -> None:
        status, headers, payload = http_request(
            self.base_url,
            "/v1/responses",
            method="POST",
            value={"model": "external-a", "stream": True, "input": "SSE please"},
            headers=self.request_headers(),
        )
        self.assertEqual(status, 200)
        self.assertTrue(headers["content-type"].startswith("text/event-stream"))
        events = parse_sse(payload)
        types = [event["type"] for event in events]
        self.assertEqual(types[0:2], ["response.created", "response.in_progress"])
        self.assertIn("response.output_text.delta", types)
        self.assertIn("response.output_text.done", types)
        self.assertEqual(types[-1], "response.completed")
        sequences = [event["sequence_number"] for event in events]
        self.assertEqual(sequences, list(range(len(events))))

    def test_04_websocket_response_create_uses_same_stream_contract(self) -> None:
        events = websocket_events(
            "127.0.0.1",
            self.router_port,
            {
                "type": "response.create",
                "model": "external-b",
                "input": "WebSocket please",
            },
            headers=self.request_headers(),
        )
        types = [event["type"] for event in events]
        self.assertEqual(types[0:2], ["response.created", "response.in_progress"])
        self.assertIn("response.output_text.delta", types)
        self.assertEqual(types[-1], "response.completed")
        self.assertNotIn("error", types)

    def test_05_official_websocket_warmup_replays_synthetic_history(self) -> None:
        warmup_marker = "official-warmup-" + uuid.uuid4().hex
        continuation_marker = "official-after-warmup-" + uuid.uuid4().hex
        official_ws_before = len(
            [
                request
                for request in self.official_state.snapshot()
                if request["method"] == "WS" and request["path"] == "/responses"
            ]
        )
        official_post_before = len(
            [
                request
                for request in self.official_state.snapshot()
                if request["method"] == "POST" and request["path"] == "/responses"
            ]
        )

        def continuation_request(
            completed_batches: list[list[dict[str, Any]]],
        ) -> dict[str, Any]:
            warmup_id = completed_batches[0][-1]["response"]["id"]
            return {
                "type": "response.create",
                "model": "official-a",
                "generate": True,
                "previous_response_id": warmup_id,
                "input": continuation_marker,
            }

        warmup_events, continuation_events = websocket_event_batches(
            "127.0.0.1",
            self.router_port,
            [
                {
                    "type": "response.create",
                    "model": "official-a",
                    "generate": False,
                    "input": warmup_marker,
                },
                continuation_request,
            ],
            headers=self.request_headers(),
        )
        self.assertEqual(
            [event["type"] for event in warmup_events],
            ["response.created", "response.completed"],
        )
        warmup_id = warmup_events[-1]["response"]["id"]
        self.assertTrue(warmup_id.startswith("cmr_warmup_"))

        continuation_types = [event["type"] for event in continuation_events]
        self.assertEqual(continuation_types[0], "response.created")
        self.assertEqual(continuation_types[-1], "response.completed")
        self.assertNotIn("error", continuation_types)

        official_ws_requests = [
            request
            for request in self.official_state.snapshot()
            if request["method"] == "WS" and request["path"] == "/responses"
        ]
        official_posts = [
            request
            for request in self.official_state.snapshot()
            if request["method"] == "POST" and request["path"] == "/responses"
        ]
        self.assertEqual(len(official_ws_requests), official_ws_before + 1)
        self.assertEqual(len(official_posts), official_post_before)
        upstream_body = official_ws_requests[-1]["body"]
        upstream_text = json.dumps(upstream_body, sort_keys=True)
        self.assertNotIn("previous_response_id", upstream_body)
        self.assertNotIn(warmup_id, upstream_text)
        self.assertIn(warmup_marker, upstream_text)
        self.assertIn(continuation_marker, upstream_text)

    def test_06_compaction_is_exactly_one_standard_output_item(self) -> None:
        with closing(sqlite3.connect(self.state_db)) as connection:
            mapping_rowid_before = int(
                connection.execute(
                    "SELECT COALESCE(MAX(rowid), 0) FROM compactions"
                ).fetchone()[0]
            )
        compact_before = len(
            [
                request
                for request in self.official_state.snapshot()
                if request["method"] == "POST"
                and request["path"] == "/responses/compact"
            ]
        )
        compacted_marker = "compacted-original-" + uuid.uuid4().hex
        nonstream = self.post_response(
            {
                "model": "external-a",
                "stream": False,
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_text", "text": compacted_marker}
                        ],
                    },
                    {"type": "compaction_trigger"},
                ],
            }
        )
        self.assertEqual(len(nonstream["output"]), 1)
        self.assertEqual(nonstream["output"][0]["type"], "compaction")
        self.assertTrue(nonstream["output"][0]["encrypted_content"])
        compaction_response_id = nonstream.get("id")
        self.assertIsInstance(compaction_response_id, str)
        self.assertTrue(compaction_response_id)

        external_b_before = len(self.external_b_state.snapshot())
        post_compaction_marker = "after-compaction-" + uuid.uuid4().hex
        status, _, payload = http_request(
            self.base_url,
            "/v1/responses",
            method="POST",
            value={
                "model": "external-b",
                "stream": False,
                "previous_response_id": compaction_response_id,
                "input": post_compaction_marker,
            },
            headers=self.request_headers(),
        )
        self.assertEqual(status, 200)
        continued = parse_json_body(payload)
        self.assertIsInstance(continued, dict)
        external_b_after = self.external_b_state.snapshot()
        self.assertEqual(len(external_b_after), external_b_before + 1)
        continuation_body = external_b_after[-1]["body"]
        continuation_messages = continuation_body["messages"]
        self.assertEqual(len(continuation_messages), 2)
        self.assertEqual(
            [message.get("role") for message in continuation_messages],
            ["developer", "user"],
        )
        self.assertEqual(
            _message_text(continuation_messages[0]),
            "portable summary produced by provider-a",
        )
        self.assertEqual(
            _message_text(continuation_messages[1]), post_compaction_marker
        )
        continuation_text = json.dumps(continuation_body, sort_keys=True)
        self.assertIn("portable summary produced by provider-a", continuation_text)
        self.assertIn(post_compaction_marker, continuation_text)
        self.assertNotIn(compacted_marker, continuation_text)
        self.assertNotIn(
            nonstream["output"][0]["encrypted_content"], continuation_text
        )

        status, _, payload = http_request(
            self.base_url,
            "/v1/responses",
            method="POST",
            value={
                "model": "external-b",
                "stream": True,
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_text", "text": "streamed compaction"}
                        ],
                    },
                    {"type": "compaction_trigger"},
                ],
            },
            headers=self.request_headers(),
        )
        self.assertEqual(status, 200)
        events = parse_sse(payload)
        done_items = [
            event["item"]
            for event in events
            if event.get("type") == "response.output_item.done"
        ]
        self.assertEqual(len(done_items), 1)
        self.assertEqual(done_items[0]["type"], "compaction")
        completed = [
            event for event in events if event.get("type") == "response.completed"
        ]
        self.assertEqual(len(completed), 1)
        output = completed[0]["response"]["output"]
        self.assertEqual(len(output), 1)
        self.assertEqual(output[0]["type"], "compaction")

        direct_marker = "direct-compaction-" + uuid.uuid4().hex
        status, _, payload = http_request(
            self.base_url,
            "/v1/responses/compact",
            method="POST",
            value={"model": "external-a", "input": direct_marker},
            headers=self.request_headers(),
        )
        self.assertEqual(status, 200)
        direct = parse_json_body(payload)
        self.assertEqual(len(direct["output"]), 1)
        self.assertEqual(direct["output"][0]["type"], "compaction")
        direct_id = direct.get("id")
        self.assertIsInstance(direct_id, str)
        self.assertTrue(direct_id.startswith("cmr_compact_"))

        official_posts_before = len(
            [
                request
                for request in self.official_state.snapshot()
                if request["method"] == "POST" and request["path"] == "/responses"
            ]
        )
        official_marker = "official-after-direct-compact-" + uuid.uuid4().hex
        official_continuation = self.post_response(
            {
                "model": "official-b",
                "stream": False,
                "previous_response_id": direct_id,
                "input": official_marker,
            }
        )
        self.assertEqual(official_continuation["model"], "official-b")
        official_posts = [
            request
            for request in self.official_state.snapshot()
            if request["method"] == "POST" and request["path"] == "/responses"
        ]
        self.assertEqual(len(official_posts), official_posts_before + 1)
        official_body = official_posts[-1]["body"]
        self.assertNotIn("previous_response_id", official_body)
        official_input = official_body["input"]
        self.assertEqual(len(official_input), 2)
        self.assertEqual(
            [item.get("type") for item in official_input],
            ["compaction", "message"],
        )
        self.assertEqual(
            official_input[0]["encrypted_content"],
            direct["output"][0]["encrypted_content"],
        )
        self.assertEqual(official_input[1].get("role"), "user")
        self.assertEqual(_message_text(official_input[1]), official_marker)
        official_body_text = json.dumps(official_body, sort_keys=True)
        self.assertNotIn(direct_id, official_body_text)
        self.assertNotIn(direct_marker, official_body_text)

        external_b_before_direct_replay = len(self.external_b_state.snapshot())
        external_marker = "external-after-direct-compact-" + uuid.uuid4().hex
        self.post_response(
            {
                "model": "external-b",
                "stream": False,
                "previous_response_id": direct_id,
                "input": external_marker,
            }
        )
        external_b_after_direct_replay = self.external_b_state.snapshot()
        self.assertEqual(
            len(external_b_after_direct_replay),
            external_b_before_direct_replay + 1,
        )
        external_body = external_b_after_direct_replay[-1]["body"]
        external_messages = external_body["messages"]
        self.assertEqual(len(external_messages), 2)
        self.assertEqual(
            [message.get("role") for message in external_messages],
            ["developer", "user"],
        )
        self.assertEqual(
            _message_text(external_messages[0]),
            "portable summary produced by provider-a",
        )
        self.assertEqual(_message_text(external_messages[1]), external_marker)
        external_body_text = json.dumps(external_body, sort_keys=True)
        self.assertNotIn(direct_id, external_body_text)
        self.assertNotIn(direct_marker, external_body_text)
        self.assertNotIn(
            direct["output"][0]["encrypted_content"], external_body_text
        )

        official_compactions = [
            request
            for request in self.official_state.snapshot()
            if request["method"] == "POST"
            and request["path"] == "/responses/compact"
        ]
        self.assertEqual(len(official_compactions) - compact_before, 3)
        new_official_compactions = official_compactions[compact_before:]
        self.assertTrue(
            all(
                request["headers"].get("authorization", "").startswith("Bearer ")
                for request in new_official_compactions
            )
        )
        self.assertTrue(
            all(
                request["headers"].get("chatgpt-account-id") == TEST_ACCOUNT_ID
                for request in new_official_compactions
            )
        )
        with closing(sqlite3.connect(self.state_db)) as connection:
            mappings = connection.execute(
                "SELECT source_provider, portable_summary, encrypted_item_json "
                "FROM compactions WHERE rowid > ?1 ORDER BY rowid",
                (mapping_rowid_before,),
            ).fetchall()
        # Other tests (or pre-seeded fixtures) may own older rows.  This case
        # asserts only the mappings created after its explicit baseline.
        self.assertEqual(len(mappings), 3)
        self.assertTrue(all(row[0] == "official" for row in mappings))
        self.assertTrue(all(row[1].strip() for row in mappings))
        encrypted_items = [json.loads(row[2]) for row in mappings]
        self.assertTrue(
            all(item.get("type") == "compaction" for item in encrypted_items)
        )
        self.assertTrue(
            all(item.get("encrypted_content") for item in encrypted_items)
        )

    def test_07_official_auth_is_forwarded_but_external_auth_is_isolated(self) -> None:
        official_before = len(self.official_state.snapshot())
        external_before = len(self.external_a_state.snapshot())
        headers = self.request_headers()
        headers.update(
            {
                "Cookie": "session=" + uuid.uuid4().hex,
                "X-Api-Key": uuid.uuid4().hex,
                "X-Goog-Api-Key": uuid.uuid4().hex,
                "X-OpenAI-Test": uuid.uuid4().hex,
            }
        )
        http_request(self.base_url, "/v1/models", headers=headers)
        http_request(
            self.base_url,
            "/v1/responses",
            method="POST",
            value={"model": "official-a", "stream": False, "input": "official auth"},
            headers=headers,
        )
        http_request(
            self.base_url,
            "/v1/responses",
            method="POST",
            value={"model": "external-a", "stream": False, "input": "external auth"},
            headers=headers,
        )

        official_requests = self.official_state.snapshot()[official_before:]
        external_requests = self.external_a_state.snapshot()[external_before:]
        self.assertGreaterEqual(len(official_requests), 2)
        self.assertEqual(len(external_requests), 1)
        for request in official_requests:
            self.assertEqual(
                request["headers"].get("authorization"), headers["Authorization"]
            )
            self.assertEqual(
                request["headers"].get("chatgpt-account-id"),
                headers["ChatGPT-Account-ID"],
            )
        blocked = {
            "authorization",
            "cookie",
            "chatgpt-account-id",
            "x-api-key",
            "x-goog-api-key",
            "x-openai-test",
        }
        self.assertTrue(blocked.isdisjoint(external_requests[0]["headers"]))

    def test_08_invalid_successful_catalogs_do_not_pass_official_auth_gate(self) -> None:
        external_before = len(self.external_a_state.snapshot())
        official_before = len(self.official_state.snapshot())

        for catalog_case in ("no-content", "malformed-json", "missing-models"):
            with self.subTest(catalog_case=catalog_case):
                headers = self.request_headers()
                headers["OpenAI-CMR-Test-Catalog"] = catalog_case
                with self.assertRaises(urllib.error.HTTPError) as raised:
                    http_request(
                        self.base_url,
                        "/v1/responses",
                        method="POST",
                        value={
                            "model": "external-a",
                            "stream": False,
                            "input": "must not reach the external upstream",
                        },
                        headers=headers,
                    )
                self.assertEqual(raised.exception.code, 401)
                raised.exception.close()

        self.assertEqual(len(self.external_a_state.snapshot()), external_before)
        official_requests = self.official_state.snapshot()[official_before:]
        catalog_requests = [
            request
            for request in official_requests
            if request["method"] == "GET" and request["path"] == "/models"
        ]
        self.assertEqual(len(catalog_requests), 3)
        self.assertEqual(
            {request["headers"].get("openai-cmr-test-catalog") for request in catalog_requests},
            {"no-content", "malformed-json", "missing-models"},
        )

    def test_09_external_max_output_limit_is_enforced_before_upstream(self) -> None:
        configured_limit = 16_000
        external_before = len(self.external_a_state.snapshot())
        response = self.post_response(
            {
                "model": "external-a",
                "stream": False,
                "max_output_tokens": configured_limit,
                "input": "max output at configured limit",
            }
        )
        self.assertEqual(response["model"], "external-a")
        external_after = self.external_a_state.snapshot()
        self.assertEqual(len(external_after), external_before + 1)
        upstream_body = external_after[-1]["body"]
        self.assertEqual(upstream_body.get("max_tokens"), configured_limit)
        self.assertNotIn("max_output_tokens", upstream_body)

        def upstream_counts() -> tuple[int, int, int]:
            return (
                len(self.official_state.snapshot()),
                len(self.external_a_state.snapshot()),
                len(self.external_b_state.snapshot()),
            )

        rejected_baseline = upstream_counts()
        with self.assertRaises(urllib.error.HTTPError) as raised:
            http_request(
                self.base_url,
                "/v1/responses",
                method="POST",
                value={
                    "model": "external-a",
                    "stream": False,
                    "max_output_tokens": configured_limit + 1,
                    "input": "must be rejected locally",
                },
                headers=self.request_headers(),
            )
        self.assertEqual(raised.exception.code, 400)
        raised.exception.close()
        self.assertEqual(upstream_counts(), rejected_baseline)

        ws_events = websocket_events(
            "127.0.0.1",
            self.router_port,
            {
                "type": "response.create",
                "model": "external-a",
                "max_output_tokens": configured_limit + 1,
                "input": "WebSocket must be rejected locally",
            },
            headers=self.request_headers(),
        )
        self.assertEqual([event.get("type") for event in ws_events], ["error"])
        self.assertEqual(upstream_counts(), rejected_baseline)

        with self.assertRaises(urllib.error.HTTPError) as raised:
            http_request(
                self.base_url,
                "/v1/responses/compact",
                method="POST",
                value={
                    "model": "external-a",
                    "max_output_tokens": configured_limit + 1,
                    "input": "compact must be rejected locally",
                },
                headers=self.request_headers(),
            )
        self.assertEqual(raised.exception.code, 400)
        raised.exception.close()
        self.assertEqual(upstream_counts(), rejected_baseline)

    def test_10_incomplete_responses_remain_chainable_across_models(self) -> None:
        official_marker = "force-official-incomplete-" + uuid.uuid4().hex
        status, headers, payload = http_request(
            self.base_url,
            "/v1/responses",
            method="POST",
            value={
                "model": "official-a",
                "stream": True,
                "input": official_marker,
            },
            headers=self.request_headers(),
        )
        self.assertEqual(status, 200)
        self.assertTrue(headers["content-type"].startswith("text/event-stream"))
        official_events = parse_sse(payload)
        official_terminal = official_events[-1]
        self.assertEqual(official_terminal["type"], "response.incomplete")
        official_response = official_terminal["response"]
        self.assertEqual(official_response["status"], "incomplete")
        self.assertEqual(
            official_response["incomplete_details"],
            {"reason": "max_output_tokens"},
        )
        self.assertTrue(official_response["id"])

        external_a_before = len(self.external_a_state.snapshot())
        official_followup = "after-official-incomplete-" + uuid.uuid4().hex
        continued = self.post_response(
            {
                "model": "external-a",
                "stream": False,
                "previous_response_id": official_response["id"],
                "input": official_followup,
            }
        )
        self.assertEqual(continued["model"], "external-a")
        external_a_requests = self.external_a_state.snapshot()
        self.assertEqual(len(external_a_requests), external_a_before + 1)
        replayed_official = json.dumps(
            external_a_requests[-1]["body"], ensure_ascii=False, sort_keys=True
        )
        self.assertIn(official_marker, replayed_official)
        self.assertIn(official_followup, replayed_official)
        self.assertIn("official continuation", replayed_official)

        external_marker = "force-external-length-" + uuid.uuid4().hex
        status, headers, payload = http_request(
            self.base_url,
            "/v1/responses",
            method="POST",
            value={
                "model": "external-a",
                "stream": True,
                "input": external_marker,
            },
            headers=self.request_headers(),
        )
        self.assertEqual(status, 200)
        self.assertTrue(headers["content-type"].startswith("text/event-stream"))
        external_events = parse_sse(payload)
        external_terminal = external_events[-1]
        self.assertEqual(external_terminal["type"], "response.incomplete")
        external_response = external_terminal["response"]
        self.assertEqual(external_response["status"], "incomplete")
        self.assertEqual(
            external_response["incomplete_details"],
            {"reason": "max_output_tokens"},
        )
        self.assertTrue(external_response["id"])

        external_b_before = len(self.external_b_state.snapshot())
        external_followup = "after-external-incomplete-" + uuid.uuid4().hex
        continued = self.post_response(
            {
                "model": "external-b",
                "stream": False,
                "previous_response_id": external_response["id"],
                "input": external_followup,
            }
        )
        self.assertEqual(continued["model"], "external-b")
        external_b_requests = self.external_b_state.snapshot()
        self.assertEqual(len(external_b_requests), external_b_before + 1)
        replayed_external = json.dumps(
            external_b_requests[-1]["body"], ensure_ascii=False, sort_keys=True
        )
        self.assertIn(external_marker, replayed_external)
        self.assertIn(external_followup, replayed_external)
        self.assertIn("stream provider-a", replayed_external)

    def test_11_instruction_items_preserve_text_and_order(self) -> None:
        first_instruction = "first-developer-" + uuid.uuid4().hex
        second_instruction = "second-system-" + uuid.uuid4().hex
        user_marker = "instruction-user-" + uuid.uuid4().hex
        external_before = len(self.external_a_state.snapshot())
        response = self.post_response(
            {
                "model": "external-a",
                "stream": False,
                "instructions": [
                    {
                        "type": "message",
                        "role": "developer",
                        "content": first_instruction,
                    },
                    {
                        "type": "message",
                        "role": "system",
                        "content": [
                            {"type": "input_text", "text": second_instruction}
                        ],
                    },
                ],
                "input": user_marker,
            }
        )
        self.assertEqual(response["model"], "external-a")
        external_requests = self.external_a_state.snapshot()
        self.assertEqual(len(external_requests), external_before + 1)
        messages = external_requests[-1]["body"]["messages"]
        located: list[tuple[int, str]] = []
        for index, message in enumerate(messages):
            if not isinstance(message, dict):
                continue
            text = _message_text(message)
            for marker in (first_instruction, second_instruction, user_marker):
                if marker in text:
                    located.append((index, marker))
        self.assertEqual(
            located,
            [
                (0, first_instruction),
                (1, second_instruction),
                (2, user_marker),
            ],
        )
        self.assertEqual(messages[0].get("role"), "developer")
        self.assertEqual(messages[1].get("role"), "system")
        self.assertEqual(messages[2].get("role"), "user")

    def test_12_invalid_websocket_model_does_not_replace_active_model(self) -> None:
        first_marker = "active-model-first-" + uuid.uuid4().hex
        third_marker = "active-model-third-" + uuid.uuid4().hex
        external_a_before = len(self.external_a_state.snapshot())
        external_b_before = len(self.external_b_state.snapshot())

        def upstream_counts() -> tuple[int, int, int]:
            return (
                len(self.official_state.snapshot()),
                len(self.external_a_state.snapshot()),
                len(self.external_b_state.snapshot()),
            )

        counts_around_invalid: dict[str, tuple[int, int, int]] = {}

        def invalid_request(
            _completed_batches: list[list[dict[str, Any]]],
        ) -> dict[str, Any]:
            counts_around_invalid["before"] = upstream_counts()
            return {
                "type": "response.create",
                "model": "model-that-does-not-exist",
                "input": "must be rejected locally",
            }

        def inherited_request(
            _completed_batches: list[list[dict[str, Any]]],
        ) -> dict[str, Any]:
            counts_around_invalid["after"] = upstream_counts()
            return {
                "type": "response.create",
                "input": third_marker,
            }

        first_events, invalid_events, inherited_events = websocket_event_batches(
            "127.0.0.1",
            self.router_port,
            [
                {
                    "type": "response.create",
                    "model": "external-a",
                    "input": first_marker,
                },
                invalid_request,
                inherited_request,
            ],
            headers=self.request_headers(),
        )
        self.assertEqual(first_events[-1]["type"], "response.completed")
        self.assertEqual(
            invalid_events,
            [
                {
                    "type": "error",
                    "code": "invalid_request",
                    "message": "unknown model: model-that-does-not-exist",
                    "param": None,
                    "sequence_number": 0,
                }
            ],
        )
        self.assertEqual(
            counts_around_invalid["after"],
            counts_around_invalid["before"],
            "an unknown model frame must not reach any official or external upstream",
        )
        self.assertEqual(inherited_events[-1]["type"], "response.completed")
        self.assertEqual(
            inherited_events[-1]["response"]["model"], "external-a"
        )

        external_a_requests = self.external_a_state.snapshot()
        self.assertEqual(len(external_a_requests), external_a_before + 2)
        self.assertEqual(len(self.external_b_state.snapshot()), external_b_before)
        inherited_body = external_a_requests[-1]["body"]
        inherited_text = json.dumps(
            inherited_body, ensure_ascii=False, sort_keys=True
        )
        self.assertIn(third_marker, inherited_text)
        self.assertNotIn("model-that-does-not-exist", inherited_text)

    def test_13_standard_context_management_is_never_silently_dropped(self) -> None:
        external_before = len(self.external_a_state.snapshot())
        with self.assertRaises(urllib.error.HTTPError) as raised:
            http_request(
                self.base_url,
                "/v1/responses",
                method="POST",
                value={
                    "model": "external-a",
                    "stream": False,
                    "input": "external context management must fail locally",
                    "context_management": [
                        {"type": "compaction", "compact_threshold": 20_000}
                    ],
                },
                headers=self.request_headers(),
            )
        response = raised.exception
        try:
            self.assertEqual(response.code, 400)
            payload = response.read()
        finally:
            response.close()
        error = parse_json_body(payload)
        self.assertIn("context_management", json.dumps(error, sort_keys=True))
        self.assertEqual(len(self.external_a_state.snapshot()), external_before)

        official_before = len(
            [
                request
                for request in self.official_state.snapshot()
                if request["method"] == "POST"
                and request["path"] == "/responses"
            ]
        )
        response = self.post_response(
            {
                "model": "official-a",
                "stream": False,
                "input": "official context management passthrough",
                "context_management": [
                    {"type": "compaction", "compact_threshold": 20_000}
                ],
            }
        )
        self.assertEqual(response["model"], "official-a")
        official_requests = [
            request
            for request in self.official_state.snapshot()
            if request["method"] == "POST" and request["path"] == "/responses"
        ]
        self.assertEqual(len(official_requests), official_before + 1)
        self.assertEqual(
            official_requests[-1]["body"].get("context_management"),
            [{"type": "compaction", "compact_threshold": 20_000}],
        )

    def test_14_automatic_official_compaction_maps_atomically_for_switching(self) -> None:
        catalog_status, _, catalog_payload = http_request(
            self.base_url,
            "/v1/models?client_version=e2e-test14",
            headers=self.request_headers(),
        )
        self.assertEqual(catalog_status, 200)
        catalog = parse_json_body(catalog_payload)
        self.assertIsInstance(catalog, dict)
        models = catalog.get("models")
        self.assertIsInstance(models, list)
        catalog_ids = {
            model.get("slug") for model in models if isinstance(model, dict)
        }
        self.assertIn("official-a", catalog_ids)
        self.assertIn("external-a", catalog_ids)

        def database_counts() -> tuple[int, int]:
            with closing(sqlite3.connect(self.state_db)) as connection:
                response_count = connection.execute(
                    "SELECT COUNT(*) FROM responses"
                ).fetchone()[0]
                compaction_count = connection.execute(
                    "SELECT COUNT(*) FROM compactions"
                ).fetchone()[0]
            return int(response_count), int(compaction_count)

        responses_before, mappings_before = database_counts()
        official_before = len(
            [
                request
                for request in self.official_state.snapshot()
                if request["method"] == "POST" and request["path"] == "/responses"
            ]
        )
        compacted_marker = "force-auto-compaction-success-" + uuid.uuid4().hex
        compacted = self.post_response(
            {
                "model": "official-a",
                "stream": False,
                "input": compacted_marker,
                "context_management": [
                    {"type": "compaction", "compact_threshold": 1}
                ],
            }
        )
        self.assertEqual(compacted.get("status"), "completed")
        self.assertEqual(len(compacted.get("output", [])), 1)
        self.assertEqual(compacted["output"][0].get("type"), "compaction")
        encrypted_content = compacted["output"][0].get("encrypted_content")
        self.assertIsInstance(encrypted_content, str)
        self.assertTrue(encrypted_content)
        compacted_response_id = compacted.get("id")
        self.assertIsInstance(compacted_response_id, str)
        self.assertTrue(compacted_response_id)

        official_posts = [
            request
            for request in self.official_state.snapshot()
            if request["method"] == "POST" and request["path"] == "/responses"
        ]
        new_official_posts = official_posts[official_before:]
        self.assertEqual(len(new_official_posts), 2)
        original_request, summary_request = new_official_posts
        self.assertEqual(
            original_request["body"].get("context_management"),
            [{"type": "compaction", "compact_threshold": 1}],
        )
        self.assertEqual(summary_request["body"].get("stream"), False)
        self.assertEqual(
            summary_request["body"].get("metadata", {}).get(
                "cmr_internal_operation"
            ),
            "portable_compaction_summary_v1",
        )
        self.assertIn(
            compacted_marker,
            json.dumps(summary_request["body"].get("input"), sort_keys=True),
        )
        self.assertTrue(
            summary_request["headers"].get("authorization", "").startswith("Bearer ")
        )
        self.assertEqual(
            summary_request["headers"].get("chatgpt-account-id"), TEST_ACCOUNT_ID
        )

        responses_after_success, mappings_after_success = database_counts()
        self.assertEqual(responses_after_success, responses_before + 1)
        self.assertEqual(mappings_after_success, mappings_before + 1)
        with closing(sqlite3.connect(self.state_db)) as connection:
            portable_summary, encrypted_item_json = connection.execute(
                "SELECT portable_summary, encrypted_item_json "
                "FROM compactions ORDER BY created_at DESC, rowid DESC LIMIT 1"
            ).fetchone()
        self.assertEqual(portable_summary, "portable automatic compaction summary")
        persisted_compaction = json.loads(encrypted_item_json)
        self.assertEqual(persisted_compaction.get("type"), "compaction")
        self.assertEqual(
            persisted_compaction.get("encrypted_content"), encrypted_content
        )

        external_before = len(self.external_a_state.snapshot())
        continuation_marker = "after-auto-compaction-" + uuid.uuid4().hex
        continued = self.post_response(
            {
                "model": "external-a",
                "stream": False,
                "previous_response_id": compacted_response_id,
                "input": continuation_marker,
            }
        )
        self.assertEqual(continued.get("status"), "completed")
        external_requests = self.external_a_state.snapshot()
        self.assertEqual(len(external_requests), external_before + 1)
        messages = external_requests[-1]["body"].get("messages", [])
        self.assertEqual([item.get("role") for item in messages], ["developer", "user"])
        self.assertEqual(
            _message_text(messages[0]), "portable automatic compaction summary"
        )
        self.assertEqual(_message_text(messages[1]), continuation_marker)
        replay_text = json.dumps(external_requests[-1]["body"], sort_keys=True)
        self.assertNotIn(compacted_marker, replay_text)
        self.assertNotIn(encrypted_content, replay_text)

        responses_before_failure, mappings_before_failure = database_counts()
        failure_marker = (
            "force-auto-compaction-summary-failure-" + uuid.uuid4().hex
        )
        with self.assertRaises(urllib.error.HTTPError) as raised:
            http_request(
                self.base_url,
                "/v1/responses",
                method="POST",
                value={
                    "model": "official-a",
                    "stream": False,
                    "input": failure_marker,
                    "context_management": [
                        {"type": "compaction", "compact_threshold": 1}
                    ],
                },
                headers=self.request_headers(),
            )
        response = raised.exception
        try:
            self.assertEqual(response.code, 502)
            payload = response.read()
        finally:
            response.close()
        failure = parse_json_body(payload)
        self.assertEqual(failure.get("error", {}).get("code"), "upstream_error")
        self.assertEqual(
            database_counts(), (responses_before_failure, mappings_before_failure)
        )

    def test_15_process_restart_recovers_journaled_tool_call_for_switching(self) -> None:
        # Bind the stable test account and create one ordinary response so the
        # crash fixture can use the exact generation-scoped provider owner that
        # the running router derives from this isolated config.
        status, _, _ = http_request(
            self.base_url,
            "/v1/models?client_version=e2e-restart",
            headers=self.request_headers(),
        )
        self.assertEqual(status, 200)
        owner_seed = self.post_response(
            {
                "model": "external-a",
                "stream": False,
                "input": "establish restart-test provider owner",
            }
        )
        owner_seed_id = owner_seed.get("id")
        self.assertIsInstance(owner_seed_id, str)

        with closing(sqlite3.connect(self.state_db)) as connection:
            owner_row = connection.execute(
                "SELECT provider_owner_id FROM responses WHERE id=?1",
                (owner_seed_id,),
            ).fetchone()
        self.assertIsNotNone(owner_row)
        provider_owner_id = owner_row[0]
        self.assertIsInstance(provider_owner_id, str)
        self.assertTrue(provider_owner_id)

        self.stop_test_router()

        response_id = "resp_restart_" + uuid.uuid4().hex
        session_id = "session_restart_" + uuid.uuid4().hex
        call_id = "call_restart_" + uuid.uuid4().hex
        input_marker = "before-router-restart-" + uuid.uuid4().hex
        tool_output = "recovered-tool-output-" + uuid.uuid4().hex
        input_items = [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": input_marker}],
            }
        ]
        journaled_call = {
            "type": "function_call",
            "id": "fc_" + uuid.uuid4().hex,
            "call_id": call_id,
            "name": "lookup",
            "arguments": '{"topic":"restart"}',
        }
        with closing(sqlite3.connect(self.state_db)) as connection:
            connection.execute(
                "INSERT INTO responses "
                "(id,session_id,previous_response_id,provider_id,provider_owner_id,"
                "model_id,input_json,output_json,status,incomplete_details_json,created_at) "
                "VALUES (?1,?2,NULL,?3,?4,?5,?6,'[]','in_progress',NULL,?7)",
                (
                    response_id,
                    session_id,
                    "provider-a",
                    provider_owner_id,
                    "external-a",
                    json.dumps(input_items, separators=(",", ":")),
                    "2026-08-12T00:00:00Z",
                ),
            )
            connection.execute(
                "INSERT INTO response_output_journal "
                "(response_id,output_index,item_json,journaled_at) "
                "VALUES (?1,0,?2,?3)",
                (
                    response_id,
                    json.dumps(journaled_call, separators=(",", ":")),
                    "2026-08-12T00:00:01Z",
                ),
            )
            connection.commit()

        self.process = start_router(self.executable, self.config_path, self.state_db)
        wait_for_router(self.base_url, self.process)

        with closing(sqlite3.connect(self.state_db)) as connection:
            recovered_row = connection.execute(
                "SELECT status,incomplete_details_json,output_json "
                "FROM responses WHERE id=?1",
                (response_id,),
            ).fetchone()
        self.assertIsNotNone(recovered_row)
        self.assertEqual(recovered_row[0], "incomplete")
        self.assertEqual(
            json.loads(recovered_row[1]), {"reason": "router_restart"}
        )
        recovered_output = json.loads(recovered_row[2])
        self.assertEqual(recovered_output, [journaled_call])

        external_before = len(self.external_b_state.snapshot())
        continued = self.post_response(
            {
                "model": "external-b",
                "stream": False,
                "previous_response_id": response_id,
                "input": [
                    {
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": tool_output,
                    }
                ],
            }
        )
        self.assertEqual(continued.get("status"), "completed")
        external_requests = self.external_b_state.snapshot()
        self.assertEqual(len(external_requests), external_before + 1)
        messages = external_requests[-1]["body"].get("messages", [])
        replay_text = json.dumps(messages, ensure_ascii=False, sort_keys=True)
        self.assertIn(input_marker, replay_text)

        assistant_tool_messages = [
            message
            for message in messages
            if message.get("role") == "assistant" and message.get("tool_calls")
        ]
        self.assertEqual(len(assistant_tool_messages), 1)
        replayed_calls = assistant_tool_messages[0]["tool_calls"]
        self.assertEqual(len(replayed_calls), 1)
        self.assertEqual(replayed_calls[0].get("id"), call_id)
        self.assertEqual(replayed_calls[0].get("function", {}).get("name"), "lookup")
        self.assertEqual(
            replayed_calls[0].get("function", {}).get("arguments"),
            '{"topic":"restart"}',
        )
        tool_messages = [
            message for message in messages if message.get("role") == "tool"
        ]
        self.assertEqual(len(tool_messages), 1)
        self.assertEqual(tool_messages[0].get("tool_call_id"), call_id)
        self.assertEqual(_message_text(tool_messages[0]), tool_output)


if __name__ == "__main__":
    unittest.main(verbosity=2)
