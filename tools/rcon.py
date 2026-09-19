#!/usr/bin/env python3
"""Minimal RCON client for the dev server, no dependencies.

    python3 tools/rcon.py [--props run/server.properties] <command> [<command> ...]

Host is localhost, port and password come from server.properties
(rcon.port, rcon.password). Prints each command's response on stdout.
Exit code 2 when the server does not answer.
"""
import argparse, os, socket, struct, sys

SERVERDATA_AUTH = 3
SERVERDATA_EXECCOMMAND = 2


def read_props(path):
    props = {}
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                k, v = line.split("=", 1)
                props[k.strip()] = v.strip()
    return props


class Rcon:
    def __init__(self, host, port, password, timeout=10.0):
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.req = 0
        self._send(SERVERDATA_AUTH, password)
        rid, _ = self._recv()
        if rid == -1:
            raise RuntimeError("rcon auth failed")

    def _send(self, kind, body):
        self.req += 1
        payload = struct.pack("<ii", self.req, kind) + body.encode() + b"\x00\x00"
        self.sock.sendall(struct.pack("<i", len(payload)) + payload)
        return self.req

    def _recv(self):
        raw = self._read(4)
        (length,) = struct.unpack("<i", raw)
        data = self._read(length)
        rid, _kind = struct.unpack("<ii", data[:8])
        return rid, data[8:-2].decode(errors="replace")

    def _read(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise RuntimeError("rcon connection closed")
            buf += chunk
        return buf

    def command(self, text):
        # Vanilla answers each command with exactly one packet.
        rid = self._send(SERVERDATA_EXECCOMMAND, text)
        got, body = self._recv()
        if got != rid:
            raise RuntimeError(f"rcon: response id {got} for request {rid}")
        return body

    def close(self):
        self.sock.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--props", default=os.path.join(os.path.dirname(__file__), "..", "run", "server.properties"))
    ap.add_argument("commands", nargs="+")
    args = ap.parse_args()
    props = read_props(args.props)
    port = int(props.get("rcon.port", 25575))
    password = props.get("rcon.password", "")
    # One connection per command: vanilla drops later commands on a reused socket.
    for cmd in args.commands:
        try:
            rc = Rcon("127.0.0.1", port, password)
        except OSError as e:
            print(f"rcon: {e}", file=sys.stderr)
            return 2
        try:
            print(rc.command(cmd))
        finally:
            rc.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
