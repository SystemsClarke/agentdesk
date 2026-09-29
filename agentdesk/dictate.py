"""Local speech-to-text for the window's text boxes, run by the core's Python plugin host.

The window calls dictate.start, polls dictate.poll every ~150 ms for the words so far, and calls dictate.stop
(Ctrl+D again). Everything runs on this PC: Nemotron-Speech-Streaming-EN-0.6B (int8, ~660 MB, a FastConformer-RNNT
model) through sherpa-onnx's onnxruntime, fetched once into %LOCALAPPDATA%/AgentDesk/models. No audio leaves the machine.
The plugin host exits after a few idle minutes, which frees the model's RAM.
"""

from __future__ import annotations

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


class _Job:
    """One dictation: state is downloading, loading, listening, or done (with text, and error if it failed)."""

    def __init__(self) -> None:
        self.state, self.text, self.error, self.progress = "loading", "", None, 0.0
        self._stop = threading.Event()
        threading.Thread(target=self._run, name="agentdesk-dictate", daemon=True).start()

    def stop(self) -> None:
        self._stop.set()

    def _run(self) -> None:
        try:
            if not _downloaded():
                self.state = "downloading"
                _download(lambda p: setattr(self, "progress", p))
            import sounddevice as sd
            audio: "queue.Queue" = queue.Queue()
            # The mic opens before the model loads: on a cold start the load takes seconds, and what you say meanwhile waits in the queue.
            with sd.InputStream(samplerate=SAMPLE_RATE, channels=1, dtype="float32", blocksize=_CHUNK,
                                callback=lambda data, _f, _t, _s: audio.put(data[:, 0].copy())):
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
            while not audio.empty():  # the tail the device delivered after the stop
                feed(audio.get_nowait())
            stream.input_finished()
            while rec.is_ready(stream):
                rec.decode_stream(stream)
            self.text = rec.get_result(stream).strip() or self.text
        except Exception as exc:  # reported to the window, never left to die silently on a daemon thread
            self.error = f"{type(exc).__name__}: {exc}"
        self.state = "done"


def _status() -> dict:
    j = _job
    return {"state": j.state, "text": j.text, "error": j.error, "progress": j.progress} if j else {"state": "idle", "text": "", "error": None, "progress": 0.0}


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
