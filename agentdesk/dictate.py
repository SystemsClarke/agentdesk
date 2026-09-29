"""Local speech-to-text for the window's text boxes, run by the core's Python plugin host.

The window calls dictate.start, polls dictate.poll every ~150 ms for the words so far, and calls dictate.stop
(Ctrl+D again). Everything runs on this PC: Nemotron-Speech-Streaming-EN-0.6B (int8, ~660 MB, a FastConformer-RNNT
model) through sherpa-onnx's onnxruntime, fetched once into %LOCALAPPDATA%/AgentDesk/models. No audio leaves the machine.
The plugin host exits after a few idle minutes, which frees the model's RAM.
"""

from __future__ import annotations

import collections
import contextlib
import queue
import threading
import urllib.request

from . import paths

SAMPLE_RATE = 16000
_CHUNK = 1600  # 100 ms: small enough that partial words show up while you are still talking
_BASE = "https://huggingface.co/csukuangfj/sherpa-onnx-nemotron-speech-streaming-en-0.6b-int8-2026-01-14/resolve/main/"
MODEL_DIR = paths.DATA_DIR / "models" / "nemotron-speech-streaming-en-0.6b-int8"
_FILES = (("tokens.txt", 8_952), ("decoder.int8.onnx", 7_257_753), ("joiner.int8.onnx", 1_735_862), ("encoder.int8.onnx", 652_916_830))
_TOTAL = sum(size for _, size in _FILES)

_recognizer = None
_recognizer_lock = threading.Lock()
_job: "_Job | None" = None
_warming = False


def _downloaded() -> bool:
    return all((MODEL_DIR / name).exists() for name, _ in _FILES)


def _download(progress) -> None:
    """Fetches what is missing. A .part file renamed on completion, so a killed download never looks like a model."""
    MODEL_DIR.mkdir(parents=True, exist_ok=True)
    done = sum((MODEL_DIR / n).stat().st_size for n, _ in _FILES if (MODEL_DIR / n).exists())
    for name, _ in _FILES:
        dest = MODEL_DIR / name
        if dest.exists():
            continue
        part = dest.with_suffix(dest.suffix + ".part")
        urllib.request.urlretrieve(_BASE + name, part, reporthook=lambda b, size, total, base=done: progress(min(1.0, (base + min(b * size, total)) / _TOTAL)))
        part.rename(dest)
        done += dest.stat().st_size


def _model():
    global _recognizer
    with _recognizer_lock:
        if _recognizer is None:
            import sherpa_onnx  # the heavy import waits for the first Ctrl+D
            _recognizer = sherpa_onnx.OnlineRecognizer.from_transducer(
                tokens=str(MODEL_DIR / "tokens.txt"), encoder=str(MODEL_DIR / "encoder.int8.onnx"),
                decoder=str(MODEL_DIR / "decoder.int8.onnx"), joiner=str(MODEL_DIR / "joiner.int8.onnx"),
                num_threads=2, sample_rate=SAMPLE_RATE, feature_dim=128, model_type="nemotron", decoding_method="greedy_search")
        return _recognizer


class _Mic:
    """The pre-roll microphone. While a box has focus it is armed: it keeps only the last 2 seconds of audio, in RAM, so
    Ctrl+D catches what you said just before pressing it and needs no time to open the device. A dictation attaches to it:
    the buffered seconds come first, then live audio on the same open stream. Nothing is ever written anywhere."""

    def __init__(self, seconds: float = 2.0) -> None:
        self._buf: "collections.deque" = collections.deque(maxlen=max(1, int(seconds * SAMPLE_RATE / _CHUNK)))
        self._sink: "queue.Queue | None" = None
        self._stream = None
        self._want = False
        self._lock = threading.Lock()

    def arm(self) -> None:
        """Idempotent. Opens the device if it is not open; no microphone raises, and the window ignores it (Ctrl+D reports it)."""
        import sounddevice as sd
        with self._lock:
            self._want = True
            if self._stream is None:
                s = sd.InputStream(samplerate=SAMPLE_RATE, channels=1, dtype="float32", blocksize=_CHUNK, callback=self._callback)
                s.start()
                self._stream = s

    def disarm(self) -> None:
        with self._lock:
            self._want = False
            if self._sink is not None:
                return  # a dictation is using it: it lets go when it detaches
            stream, self._stream = self._stream, None
            self._buf.clear()
        if stream is not None:
            stream.close()  # outside the lock: close waits for the callback, which takes it

    def _callback(self, data, _frames, _time, _status) -> None:
        chunk = data[:, 0].copy()
        with self._lock:
            if self._sink is not None:
                self._sink.put(chunk)
            else:
                self._buf.append(chunk)

    def attach(self, q: "queue.Queue") -> bool:
        with self._lock:
            if self._stream is None:
                return False
            for chunk in self._buf:
                q.put(chunk)
            self._buf.clear()
            self._sink = q
            return True

    def detach(self) -> None:
        with self._lock:
            self._sink = None
            self._buf.clear()  # what was just dictated must not be dictated again by the next press
            release = not self._want
        if release:
            self.disarm()


_mic = _Mic()


class _Job:
    """One dictation: state is downloading, loading, listening, or done (with text, and error if it failed)."""

    def __init__(self) -> None:
        self.state, self.text, self.error, self.progress = "loading", "", None, 0.0
        self._stop = threading.Event()
        threading.Thread(target=self._run, name="agentdesk-dictate", daemon=True).start()

    def stop(self) -> None:
        self._stop.set()

    def _run(self) -> None:
        shared = False
        try:
            if not _downloaded():
                self.state = "downloading"
                _download(lambda p: setattr(self, "progress", p))
            import sounddevice as sd
            audio: "queue.Queue" = queue.Queue()
            shared = _mic.attach(audio)  # armed: the last 2 s are already queued; else open the device now
            own = contextlib.nullcontext() if shared else sd.InputStream(
                samplerate=SAMPLE_RATE, channels=1, dtype="float32", blocksize=_CHUNK, callback=lambda data, _f, _t, _s: audio.put(data[:, 0].copy()))
            # The mic is open before the model loads: on a cold start the load takes seconds, and what you say meanwhile waits in the queue.
            with own:
                rec = _model()
                stream = rec.create_stream()

                def feed(chunk) -> None:
                    stream.accept_waveform(SAMPLE_RATE, chunk)
                    while rec.is_ready(stream):
                        rec.decode_stream(stream)
                    self.text = rec.get_result(stream).strip()

                self.state = "listening"
                while not self._stop.is_set():
                    try:
                        feed(audio.get(timeout=0.2))
                    except queue.Empty:
                        pass
            if shared:
                _mic.detach()  # audio after this goes back to the ring buffer, not to us
                shared = False
            while not audio.empty():  # the tail the device delivered after the stop
                feed(audio.get_nowait())
            stream.input_finished()
            while rec.is_ready(stream):
                rec.decode_stream(stream)
            self.text = rec.get_result(stream).strip() or self.text
        except Exception as exc:  # reported to the window, never left to die silently on a daemon thread
            self.error = f"{type(exc).__name__}: {exc}"
        finally:
            if shared:
                _mic.detach()
        self.state = "done"


def _status() -> dict:
    j = _job
    return {"state": j.state, "text": j.text, "error": j.error, "progress": j.progress} if j else {"state": "idle", "text": "", "error": None, "progress": 0.0}


def _warm() -> None:
    """Loads the model ahead of Ctrl+D (seconds, off the plugin's request thread); a failure resurfaces, with its real error, on the press."""
    global _warming
    if _warming or not _downloaded():
        return
    _warming = True

    def load() -> None:
        try:
            _model()
        except Exception:
            pass

    threading.Thread(target=load, name="agentdesk-dictate-warm", daemon=True).start()


def arm() -> dict:
    """A text box has focus and the pre-roll is on: keep the last 2 s, and get the model ready."""
    _mic.arm()
    _warm()
    return _status()


def disarm() -> dict:
    _mic.disarm()
    return _status()


def start() -> dict:
    global _job
    if _job is not None and _job.state != "done":
        return _status()  # already listening
    _job = _Job()
    return _status()


def poll() -> dict:
    return _status()


def stop() -> dict:
    if _job is not None:
        _job.stop()
    return _status()
