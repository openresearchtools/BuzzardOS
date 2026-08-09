#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Small TCP/UDP echo fixture used by integration-acceptance.sh.

The same source runs on the host and inside the guest so the acceptance test
does not depend on netcat variants or on an externally installed test server.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import socket
import socketserver
import sys
import tempfile
import threading
from pathlib import Path


class ReusableThreadingTCPServer(socketserver.ThreadingMixIn, socketserver.TCPServer):
    allow_reuse_address = True
    daemon_threads = True


class ReusableThreadingUDPServer(socketserver.ThreadingMixIn, socketserver.UDPServer):
    allow_reuse_address = True
    daemon_threads = True


class TcpHandler(socketserver.BaseRequestHandler):
    def handle(self) -> None:
        payload = self.request.recv(65_535)
        self.request.sendall(self.server.reply_prefix + payload)


class UdpHandler(socketserver.BaseRequestHandler):
    def handle(self) -> None:
        payload, endpoint = self.request
        endpoint.sendto(self.server.reply_prefix + payload, self.client_address)


def atomic_json(path: Path, value: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as temporary:
            json.dump(value, temporary, indent=2)
            temporary.write("\n")
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary_name, path)
    except BaseException:
        try:
            os.unlink(temporary_name)
        except FileNotFoundError:
            pass
        raise


def server(args: argparse.Namespace) -> int:
    address = args.address
    prefix = args.prefix.encode("utf-8") + b":"
    tcp = ReusableThreadingTCPServer((address, args.tcp_port), TcpHandler)
    udp = ReusableThreadingUDPServer((address, args.udp_port), UdpHandler)
    tcp.reply_prefix = prefix
    udp.reply_prefix = prefix
    stopping = threading.Event()

    def request_stop(_number: int, _frame: object) -> None:
        stopping.set()

    signal.signal(signal.SIGTERM, request_stop)
    signal.signal(signal.SIGINT, request_stop)
    threads = [
        threading.Thread(target=tcp.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True),
        threading.Thread(target=udp.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True),
    ]
    for thread in threads:
        thread.start()
    atomic_json(
        Path(args.ready),
        {
            "pid": os.getpid(),
            "address": address,
            "tcp_port": tcp.server_address[1],
            "udp_port": udp.server_address[1],
            "prefix": args.prefix,
        },
    )
    stopping.wait()
    tcp.shutdown()
    udp.shutdown()
    tcp.server_close()
    udp.server_close()
    for thread in threads:
        thread.join(timeout=1)
    return 0


def client(args: argparse.Namespace) -> int:
    payload = args.message.encode("utf-8")
    expected = args.expect_prefix.encode("utf-8") + b":" + payload
    if args.protocol == "tcp":
        with socket.create_connection((args.host, args.port), args.timeout) as endpoint:
            endpoint.settimeout(args.timeout)
            endpoint.sendall(payload)
            response = endpoint.recv(65_535)
    else:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as endpoint:
            endpoint.settimeout(args.timeout)
            endpoint.sendto(payload, (args.host, args.port))
            response, _peer = endpoint.recvfrom(65_535)
    if response != expected:
        print(
            f"unexpected {args.protocol} reply: expected {expected!r}, received {response!r}",
            file=sys.stderr,
        )
        return 2
    print(response.decode("utf-8"))
    return 0


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser()
    commands = root.add_subparsers(dest="command", required=True)
    serve = commands.add_parser("server")
    serve.add_argument("--address", default="0.0.0.0")
    serve.add_argument("--tcp-port", type=int, default=0)
    serve.add_argument("--udp-port", type=int, default=0)
    serve.add_argument("--prefix", required=True)
    serve.add_argument("--ready", required=True)
    serve.set_defaults(run=server)
    call = commands.add_parser("client")
    call.add_argument("--protocol", choices=("tcp", "udp"), required=True)
    call.add_argument("--host", required=True)
    call.add_argument("--port", type=int, required=True)
    call.add_argument("--message", required=True)
    call.add_argument("--expect-prefix", required=True)
    call.add_argument("--timeout", type=float, default=3.0)
    call.set_defaults(run=client)
    return root


if __name__ == "__main__":
    arguments = parser().parse_args()
    raise SystemExit(arguments.run(arguments))
