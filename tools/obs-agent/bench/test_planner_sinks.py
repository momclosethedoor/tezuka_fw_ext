import json
import os
import socket
import tempfile
import threading
import unittest
from collections import OrderedDict
from pathlib import Path
from unittest.mock import patch

from planner_sinks import Sinks


class RouteAckTest(unittest.TestCase):
    def test_replies_to_duplicate_with_cached_ack_only(self):
        with tempfile.TemporaryDirectory() as folder, patch.dict(os.environ, {
            "PLANNER_BENCH_ACK3_FAST": "1", "PLANNER_BENCH_SWEEP": "1",
        }, clear=True):
            sink = Sinks.__new__(Sinks)
            sink.folder = Path(folder)
            sink.stop = threading.Event()
            sink.completed = OrderedDict()
            sink.missing = False
            sink.drop_ack = 4
            for kind, expected in [(1, b"ACK 31\n"), (2, b"ACK2 9 5 7 6 31 1\n"),
                (3, b"ACK3 boot 9 5 1 mixer\n"), (4, b"ACK4 boot 9 5 7 6 31 1 mixer\n")]:
                event = dict(ack_version=kind, generation=9, version=5, epoch=7, position=6,
                    marker_id=31, route=1, required_path="mixer", boot_id="boot")
                if kind == 3:
                    event.update(event="prepare_path", source="ARM")
                for attempt in range(1, 7):
                    server, client = socket.socketpair()
                    with server, client:
                        client.sendall(json.dumps(event).encode() + b"\n")
                        sink.route(server)
                        server.shutdown(socket.SHUT_WR)
                        self.assertEqual(client.recv(1024), expected if attempt >= 5 else b"")
            records = [json.loads(line) for file in sink.folder.glob("*.ndjson")
                       for line in file.read_text().splitlines()]
            self.assertEqual(sum(not r["cached_ack"] for r in records), 4)
            self.assertEqual(sum(r["correct_ack"] for r in records), 8)
