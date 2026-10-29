#!/usr/bin/env python3
"""Deterministic CPython worker for the Hydro sidecar protocol v1.

Contract (docs/NATIVE_BINDINGS.md, `_hydro_sidecar`):

- stdin carries 4-byte little-endian length-prefixed frames, stdout carries
  only response frames. Logs, if any, go to stderr.
- payload := u8 version | u32le id | u8 kind | body
- request body := u32le method_len | method UTF-8 | u32le argc | value*
- ok body := value
- error body := u8 exception_tag | u32le message_len | message UTF-8
- the worker exits 0 when stdin reaches EOF on a frame boundary.

Methods used by `tea/tests/sidecar.rs`:

- ``__hello__`` with ``[1]`` -> ``"cpython-sidecar"`` (the spawn handshake),
- ``echo`` returns its single argument,
- ``sum`` adds two ints,
- ``fail`` answers with a ``ValueError`` (tag 0) and message
  ``worker failure``.

Options:

- ``--pid-file PATH`` appends this process id (one line per worker instance)
  so a test can prove no worker outlives the Hydro program,
- ``--speak-version N`` makes the worker answer with protocol version ``N``;
  ``2`` exercises the runtime's version rejection without needing a second
  script.

Only the standard library is used and nothing is written to stdout except
complete frames.
"""

from __future__ import annotations

import argparse
import os
import struct
import sys

PROTOCOL_VERSION = 1
MAX_FRAME = 16 * 1024 * 1024
MAX_DEPTH = 64
MAX_METHOD = 256

KIND_REQUEST = 1
KIND_OK = 2
KIND_ERROR = 3

TAG_NONE = 0
TAG_FALSE = 1
TAG_TRUE = 2
TAG_INT = 3
TAG_FLOAT = 4
TAG_STR = 5
TAG_LIST = 6
TAG_DICT = 7
TAG_BYTES = 8
TAG_TUPLE = 9

# Exception tags from docs/ARCHITECTURE.md §3.2.
VALUE_ERROR = 0
TYPE_ERROR = 1
RUNTIME_ERROR = 3

I64_MIN = -(1 << 63)
I64_MAX = (1 << 63) - 1


class ProtocolError(Exception):
    """The peer violated the wire protocol."""


class Reader:
    def __init__(self, data: bytes) -> None:
        self.data = data
        self.pos = 0

    def remaining(self) -> int:
        return len(self.data) - self.pos

    def take(self, count: int) -> bytes:
        if count < 0 or self.pos + count > len(self.data):
            raise ProtocolError("truncated payload")
        chunk = self.data[self.pos:self.pos + count]
        self.pos += count
        return chunk

    def u8(self) -> int:
        return self.take(1)[0]

    def u32(self) -> int:
        return struct.unpack("<I", self.take(4))[0]

    def i64(self) -> int:
        return struct.unpack("<q", self.take(8))[0]


def read_frame(stream) -> bytes | None:
    """Reads one frame; ``None`` at EOF on a frame boundary."""
    header = stream.read(4)
    if not header:
        return None
    if len(header) != 4:
        raise ProtocolError("truncated frame header")
    (length,) = struct.unpack("<I", header)
    if not 6 <= length <= MAX_FRAME:
        raise ProtocolError(f"invalid frame length {length}")
    payload = stream.read(length)
    if len(payload) != length:
        raise ProtocolError("truncated frame payload")
    return payload


def write_frame(stream, version: int, ident: int, kind: int, body: bytes) -> None:
    payload = bytes((version,)) + struct.pack("<I", ident) + bytes((kind,)) + body
    stream.write(struct.pack("<I", len(payload)))
    stream.write(payload)
    stream.flush()


def decode_value(reader: Reader, depth: int):
    if depth > MAX_DEPTH:
        raise ProtocolError("value nesting exceeds 64")
    tag = reader.u8()
    if tag == TAG_NONE:
        return None
    if tag == TAG_FALSE:
        return False
    if tag == TAG_TRUE:
        return True
    if tag == TAG_INT:
        return reader.i64()
    if tag == TAG_FLOAT:
        return struct.unpack("<d", reader.take(8))[0]
    if tag == TAG_STR:
        length = reader.u32()
        if length > reader.remaining():
            raise ProtocolError("truncated string")
        try:
            return reader.take(length).decode("utf-8")
        except UnicodeDecodeError as error:
            raise ProtocolError(f"invalid UTF-8 in string: {error}") from error
    if tag in (TAG_LIST, TAG_TUPLE):
        count = reader.u32()
        if count > reader.remaining():
            raise ProtocolError("truncated sequence")
        items = [decode_value(reader, depth + 1) for _ in range(count)]
        return items if tag == TAG_LIST else tuple(items)
    if tag == TAG_DICT:
        count = reader.u32()
        if count > reader.remaining():
            raise ProtocolError("truncated mapping")
        result = {}
        for _ in range(count):
            key = decode_value(reader, depth + 1)
            if isinstance(key, bool) or not isinstance(key, (int, str)):
                raise ProtocolError("dict keys must be str or non-bool int")
            result[key] = decode_value(reader, depth + 1)
        return result
    if tag == TAG_BYTES:
        length = reader.u32()
        if length > reader.remaining():
            raise ProtocolError("truncated bytes")
        return reader.take(length)
    raise ProtocolError(f"unknown value tag {tag}")


def encode_value(value, out: bytearray) -> None:
    if value is None:
        out.append(TAG_NONE)
    elif value is False:
        out.append(TAG_FALSE)
    elif value is True:
        out.append(TAG_TRUE)
    elif isinstance(value, int):
        if not I64_MIN <= value <= I64_MAX:
            raise ProtocolError("integer does not fit in 64 bits")
        out.append(TAG_INT)
        out.extend(struct.pack("<q", value))
    elif isinstance(value, float):
        out.append(TAG_FLOAT)
        out.extend(struct.pack("<d", value))
    elif isinstance(value, str):
        encoded = value.encode("utf-8")
        out.append(TAG_STR)
        out.extend(struct.pack("<I", len(encoded)))
        out.extend(encoded)
    elif isinstance(value, (bytes, bytearray)):
        out.append(TAG_BYTES)
        out.extend(struct.pack("<I", len(value)))
        out.extend(value)
    elif isinstance(value, list):
        out.append(TAG_LIST)
        out.extend(struct.pack("<I", len(value)))
        for item in value:
            encode_value(item, out)
    elif isinstance(value, tuple):
        out.append(TAG_TUPLE)
        out.extend(struct.pack("<I", len(value)))
        for item in value:
            encode_value(item, out)
    elif isinstance(value, dict):
        out.append(TAG_DICT)
        out.extend(struct.pack("<I", len(value)))
        for key, item in value.items():
            if isinstance(key, bool) or not isinstance(key, (int, str)):
                raise ProtocolError("dict keys must be str or non-bool int")
            encode_value(key, out)
            encode_value(item, out)
    else:
        raise ProtocolError(f"unsupported value type {type(value).__name__}")


def is_int(value) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def dispatch(method: str, args: list):
    """Returns ``(kind, payload)`` for one request."""
    if method == "__hello__":
        if len(args) != 1 or not is_int(args[0]):
            return (KIND_ERROR,
                    (TYPE_ERROR, "__hello__() requires one int argument"))
        if args[0] != PROTOCOL_VERSION:
            return (KIND_ERROR,
                    (VALUE_ERROR,
                     f"__hello__() received protocol version {args[0]}"))
        return (KIND_OK, "cpython-sidecar")
    if method == "echo":
        if len(args) != 1:
            return (KIND_ERROR, (TYPE_ERROR, "echo() requires one argument"))
        return (KIND_OK, args[0])
    if method == "sum":
        if len(args) != 2 or not is_int(args[0]) or not is_int(args[1]):
            return (KIND_ERROR, (TYPE_ERROR, "sum() requires two int arguments"))
        total = args[0] + args[1]
        if not I64_MIN <= total <= I64_MAX:
            return (KIND_ERROR, (VALUE_ERROR, "sum() overflows 64 bits"))
        return (KIND_OK, total)
    if method == "fail":
        return (KIND_ERROR, (VALUE_ERROR, "worker failure"))
    return (KIND_ERROR, (TYPE_ERROR, f"unknown method {method!r}"))


def serve(stream_in, stream_out, response_version: int) -> int:
    while True:
        payload = read_frame(stream_in)
        if payload is None:
            return 0
        reader = Reader(payload)
        version = reader.u8()
        ident = reader.u32()
        kind = reader.u8()
        if version != PROTOCOL_VERSION:
            raise ProtocolError(
                f"request speaks protocol version {version}, expected {PROTOCOL_VERSION}")
        if kind != KIND_REQUEST:
            raise ProtocolError(f"unexpected request kind {kind}")
        method_len = reader.u32()
        if not 1 <= method_len <= MAX_METHOD:
            raise ProtocolError(f"invalid method length {method_len}")
        method = reader.take(method_len).decode("utf-8")
        argc = reader.u32()
        if argc > reader.remaining():
            raise ProtocolError("truncated argument list")
        args = [decode_value(reader, 0) for _ in range(argc)]
        if reader.remaining() != 0:
            raise ProtocolError("trailing bytes after request")

        result_kind, result = dispatch(method, args)
        if result_kind == KIND_OK:
            body = bytearray()
            encode_value(result, body)
            write_frame(stream_out, response_version, ident, KIND_OK, bytes(body))
        else:
            tag, message = result
            encoded = message.encode("utf-8")
            body = bytes((tag,)) + struct.pack("<I", len(encoded)) + encoded
            write_frame(stream_out, response_version, ident, KIND_ERROR, body)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        description="Hydro sidecar protocol v1 CPython worker")
    parser.add_argument("--pid-file", metavar="PATH", default=None,
                        help="append this worker's pid to PATH at startup")
    parser.add_argument("--speak-version", metavar="N", type=int,
                        default=PROTOCOL_VERSION,
                        help="protocol version to place in response frames")
    options = parser.parse_args(argv)

    if options.pid_file:
        try:
            with open(options.pid_file, "a", encoding="ascii") as handle:
                handle.write(f"{os.getpid()}\n")
                handle.flush()
        except OSError as error:
            print(f"sidecar_worker: cannot write pid file: {error}",
                  file=sys.stderr)
            return 1

    try:
        return serve(sys.stdin.buffer, sys.stdout.buffer,
                     options.speak_version)
    except ProtocolError as error:
        print(f"sidecar_worker: protocol error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
