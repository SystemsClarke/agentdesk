"""Checks the pre-roll microphone (agentdesk/dictate.py _Mic) against a fake sounddevice: no hardware, no model.

Run: .venv/Scripts/python.exe scripts/check_dictate.py   (exits 1 on the first failure)
"""

import queue
import sys
import types
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))


class FakeStream:
    opened = 0
    closed = 0

    def __init__(self, callback):
        self.callback = callback
        FakeStream.opened += 1

    def start(self):
        pass

    def close(self):
        FakeStream.closed += 1

    def feed(self, n):
        import numpy as np
        self.callback(np.full((1600, 1), float(n), dtype="float32"), 1600, None, None)


streams = []


def fake_input_stream(**kw):
    s = FakeStream(kw["callback"])
    streams.append(s)
    return s


sys.modules["sounddevice"] = types.SimpleNamespace(InputStream=fake_input_stream)

from agentdesk import dictate  # noqa: E402


def check(cond, what):
    if not cond:
        print("FAIL:", what)
        sys.exit(1)
    print("ok:", what)


def drain(q):
    out = []
    while True:
        try:
            out.append(int(q.get_nowait()[0]))
        except queue.Empty:
            return out


mic = dictate._Mic(seconds=1.0)  # 10 chunks of 100 ms
check(mic.attach(queue.Queue()) is False, "an unarmed mic has nothing to attach to")

mic.arm()
mic.arm()
check(FakeStream.opened == 1, "arm is idempotent: one device open")
for i in range(25):
    streams[0].feed(i)
q = queue.Queue()
check(mic.attach(q) is True, "an armed mic attaches")
check(drain(q) == list(range(15, 25)), "the pre-roll is the last 10 chunks, in order")
streams[0].feed(100)
streams[0].feed(101)
check(drain(q) == [100, 101], "after attaching, live audio goes straight to the dictation")

mic.disarm()
check(FakeStream.closed == 0, "disarm while a dictation is attached does not pull the device out from under it")
streams[0].feed(102)
check(drain(q) == [102], "the dictation still gets audio after disarm")
mic.detach()
check(FakeStream.closed == 1, "when the dictation lets go and nothing wants the mic, the device closes")

mic2 = dictate._Mic(seconds=1.0)
mic2.arm()
s = streams[-1]
for i in range(5):
    s.feed(i)
q = queue.Queue()
mic2.attach(q)
drain(q)
mic2.detach()  # still wanted: stays armed, but what was just dictated is not kept for the next press
check(FakeStream.closed == 1, "an armed mic stays open when a dictation ends")
q2 = queue.Queue()
mic2.attach(q2)
check(drain(q2) == [], "the next press does not replay the last dictation's audio")
mic2.detach()
mic2.disarm()
check(FakeStream.closed == 2, "disarm closes an idle armed mic")
print("all pre-roll checks passed")
