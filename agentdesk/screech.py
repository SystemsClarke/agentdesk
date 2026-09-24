"""A synthesized dial-up handshake, played when the window connects.

Built from its parts rather than shipped as a recording: dial tone, DTMF digits,
ringback, the 2100 Hz answer tone with its phase reversals, V.21 FSK chirps, and the
scrambled training hiss. Rendered once to a WAV in LOCALAPPDATA, then played async.
"""

from __future__ import annotations

import array
import math
import random
import sys
import wave

from agentdesk import paths

RATE = 22050
_DTMF = {"1": (697, 1209), "2": (697, 1336), "3": (697, 1477), "4": (770, 1209),
         "5": (770, 1336), "6": (770, 1477), "7": (852, 1209), "8": (852, 1336),
         "9": (852, 1477), "0": (941, 1336)}
VERSION = 1


def _tones(freqs, secs, amp=0.35, phase_flip_every=None):
    n = int(RATE * secs)
    out = []
    flip = 1.0
    for i in range(n):
        if phase_flip_every and i and i % int(RATE * phase_flip_every) == 0:
            flip = -flip
        t = i / RATE
        out.append(flip * amp * sum(math.sin(2 * math.pi * f * t) for f in freqs) / len(freqs))
    return out


def _silence(secs):
    return [0.0] * int(RATE * secs)


def _fsk(secs, rng, lo=980, hi=1180, baud=300, amp=0.3):
    out, phase = [], 0.0
    per_bit = RATE // baud
    for _ in range(int(secs * baud)):
        f = hi if rng.random() < 0.5 else lo
        for _ in range(per_bit):
            phase += 2 * math.pi * f / RATE
            out.append(amp * math.sin(phase))
    return out


def _training(secs, rng, amp=0.32):
    n = int(RATE * secs)
    carriers = [(rng.uniform(600, 3200), rng.uniform(0, 6.28)) for _ in range(9)]
    out, smooth = [], 0.0
    for i in range(n):
        t = i / RATE
        smooth = 0.55 * smooth + 0.45 * rng.uniform(-1, 1)
        wobble = 0.6 + 0.4 * math.sin(2 * math.pi * 7.5 * t)
        tone = sum(math.sin(2 * math.pi * f * t + p) for f, p in carriers) / len(carriers)
        out.append(amp * wobble * (0.55 * tone + 0.45 * smooth))
    return out


def _fade(samples, secs=0.25):
    n = min(len(samples), int(RATE * secs))
    for i in range(n):
        samples[-1 - i] *= i / n
    return samples


def render():
    rng = random.Random(1994)
    s = []
    s += _tones((350, 440), 0.55)
    for digit in "5550199":
        s += _tones(_DTMF[digit], 0.09, amp=0.4) + _silence(0.06)
    s += _silence(0.25) + _tones((440, 480), 0.9, amp=0.3) + _silence(0.35)
    s += _tones((2100,), 1.3, amp=0.3, phase_flip_every=0.45)
    s += _fsk(0.45, rng) + _silence(0.05)
    s += _tones((1200, 2400), 0.18, amp=0.3) + _tones((1800,), 0.12, amp=0.3)
    s += _fsk(0.3, rng, lo=1650, hi=1850) + _training(1.7, rng)
    return _fade(s)


def wav_path():
    return paths.DATA_DIR / f"screech-v{VERSION}.wav"


def ensure_wav():
    path = wav_path()
    if path.exists():
        return path
    paths.ensure_dirs()
    pcm = array.array("h", (max(-32767, min(32767, int(x * 32767))) for x in render()))
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(pcm.tobytes())
    return path


def play() -> None:
    if sys.platform != "win32":
        return
    try:
        import winsound
        winsound.PlaySound(str(ensure_wav()),
                           winsound.SND_FILENAME | winsound.SND_ASYNC | winsound.SND_NODEFAULT)
    except Exception:
        pass


def stop() -> None:
    if sys.platform != "win32":
        return
    try:
        import winsound
        winsound.PlaySound(None, 0)
    except Exception:
        pass
