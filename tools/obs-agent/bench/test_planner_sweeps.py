"""Exercise pass identity independently of overlap and UDP loss."""
import struct
import tempfile
import unittest
from pathlib import Path

from planner_sweeps import Sweeps


GEN, VERSION, ANCHOR = 800, 41, 10


def view(epoch):
    return struct.pack("<8s7Q", b"GVIEW4\0\0", epoch, 0, epoch, epoch * 100, 2, 0, GEN) + bytes(512)


def end(identifier, first, last, observed):
    return struct.pack("<8s13Q", b"GSWEEP1\0", GEN, VERSION, ANCHOR, identifier, identifier % 2,
                       first, last, first, last, last * 100, last-first+1, observed, 1)


class SweepTest(unittest.TestCase):
    def run_case(self, complete_a, start_late=False):
        rows = [dict(transition_only=False,sweep_phase=p,rf_ranges=[[100,120]]) for p in "AABBB"]
        rows.insert(2,dict(transition_only=True))
        actual = dict(generation=GEN,spectrum=dict(map_epoch=ANCHOR,version=VERSION,positions=rows))
        with tempfile.TemporaryDirectory() as tmp:
            sink = Sweeps(actual, Path(tmp)/"passes.json")
            if not start_late:
                sink.accept(view(10))
            if complete_a or start_late:
                sink.accept(view(11))
            sink.accept(end(0,10,11,2))  # never requires a B observation
            self.assertEqual([*sink.notes],[0])
            # No packet for the transition at epoch 12.
            sink.accept(view(13)+view(14))
            sink.accept(view(15))
            sink.accept(end(1,13,15,3))
            results=sink.result()
            self.assertEqual(results["passes_complete"],2 if complete_a else 1)
            self.assertEqual(sink.notes[0]["missing"],0 if complete_a else 1)
            self.assertTrue(sink.notes[1]["delivery_complete"])
            self.assertLessEqual(sink.notes[0]["host_ns"],sink.notes[1]["host_ns"])

    def test_planned_dc_gaps_not_delivery_loss(self):
        self.run_case(True)

    def test_lost_record_remains_lost_despite_same_rf_in_b(self):
        self.run_case(False)

    def test_attach_in_middle_of_pass(self):
        self.run_case(False,start_late=True)


if __name__=="__main__":
    unittest.main()
