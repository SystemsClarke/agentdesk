"""Push-to-talk dictation into whatever text field has focus.

Ctrl+D behaves the way it does in Claude Code's own terminal UI: press it in
any text field to start listening, press it again to stop. Text streams in
live, word by word, as the model becomes confident of it -- not just dumped
in at the end -- because a dictation feature that only shows you its answer
after you stop talking is not telling you it heard you correctly.

Everything runs locally. Nemotron-Speech-Streaming-EN-0.6B (int8, ~660MB) is
a FastConformer-RNNT model fetched once from Hugging Face and cached under
paths.DATA_DIR -- no network round-trip per utterance, no audio ever leaves
the machine. sherpa-onnx runs it as plain onnxruntime, so there is no torch/
CUDA dependency to drag into this app.

Threading contract, same rule as pystray in app.py: audio capture and model
decoding happen entirely on a background thread. NOTHING here may touch a
tkinter widget directly -- callers get updates through a callback that they
are responsible for marshalling onto the tk thread (app.py does this through
its existing ui_queue/_drain pump, the same one pystray's callbacks use).
"""

from __future__ import annotations

import queue
import threading
import urllib.request
from pathlib import Path
from typing import Callable, Optional

from . import paths

SAMPLE_RATE = 16000
_CHUNK_SAMPLES = 1600  # 100ms at 16kHz -- small enough for live-feeling partials

_REPO = "csukuangfj/sherpa-onnx-nemotron-speech-streaming-en-0.6b-int8-2026-01-14"
_BASE_URL = f"https://huggingface.co/{_REPO}/resolve/main/"
MODEL_DIR = paths.DATA_DIR / "models" / "nemotron-speech-streaming-en-0.6b-int8"

# (filename, approximate bytes) -- the size is only used to size a progress
# bar; a wrong number here makes the bar lie, never makes the download wrong.
_MODEL_FILES = (
    ("tokens.txt", 8_952),
    ("decoder.int8.onnx", 7_257_753),
    ("joiner.int8.onnx", 1_735_862),
    ("encoder.int8.onnx", 652_916_830),
)
TOTAL_BYTES = sum(size for _name, size in _MODEL_FILES)


def is_downloaded() -> bool:
    return all((MODEL_DIR / name).exists() for name, _size in _MODEL_FILES)


def download_model(progress_cb: Optional[Callable[[int, int], None]] = None) -> None:
    """Fetch every model file that is not already on disk.

    Downloaded to a `.part` path and renamed on completion, so a download
    killed partway through (window closed, laptop slept) leaves no file that
    `is_downloaded` would mistake for a real one -- the next attempt starts
    that file over instead of trying to load a truncated encoder.

    `progress_cb(bytes_done, bytes_total)` is called from THIS thread, not
    the caller's -- see the module docstring's threading contract.
    """
    MODEL_DIR.mkdir(parents=True, exist_ok=True)
    done_bytes = sum(
        (MODEL_DIR / name).stat().st_size
        for name, _size in _MODEL_FILES
        if (MODEL_DIR / name).exists()
    )
    for name, _size in _MODEL_FILES:
        dest = MODEL_DIR / name
        if dest.exists():
            continue
        part = dest.with_suffix(dest.suffix + ".part")

        def _hook(block_count: int, block_size: int, total: int,
                  _name=name, _base=done_bytes) -> None:
            if progress_cb is not None:
                progress_cb(_base + min(block_count * block_size, total),
                            TOTAL_BYTES)

        urllib.request.urlretrieve(_BASE_URL + name, part, reporthook=_hook)
        part.rename(dest)
        done_bytes += dest.stat().st_size


_recognizer = None
_recognizer_lock = threading.Lock()


def _get_recognizer():
    global _recognizer
    with _recognizer_lock:
        if _recognizer is None:
            import sherpa_onnx  # deferred: this module must import even
            # before the optional dependency (and the model) are installed,
            # so `is_downloaded()` can be checked from app.py's Ctrl+D
            # handler without pulling in sherpa_onnx at all until needed.
            _recognizer = sherpa_onnx.OnlineRecognizer.from_transducer(
                tokens=str(MODEL_DIR / "tokens.txt"),
                encoder=str(MODEL_DIR / "encoder.int8.onnx"),
                decoder=str(MODEL_DIR / "decoder.int8.onnx"),
                joiner=str(MODEL_DIR / "joiner.int8.onnx"),
                num_threads=2,
                sample_rate=SAMPLE_RATE,
                feature_dim=128,
                model_type="nemotron",
                decoding_method="greedy_search",
            )
        return _recognizer


def warm() -> None:
    """Load the model and the audio library ahead of the first Ctrl+D (seconds, off the UI thread)."""
    if not is_downloaded():
        return

    def _load() -> None:
        try:
            import sounddevice  # noqa: F401
            _get_recognizer()
        except Exception:
            pass  # a warm-up failure resurfaces, with its real error, on the first Ctrl+D

    threading.Thread(target=_load, name="agentdesk-dictate-warm", daemon=True).start()


class Mic:
    """A pre-roll microphone: while armed it keeps only the last few seconds of audio in memory.

    Armed while an editable text box has focus. When a Session attaches, the buffered
    seconds are handed over first and live audio follows on the same open stream, so
    Ctrl+D both skips opening the device and keeps what was said just before the press.
    Audio is only ever held in this ring buffer in RAM; nothing is written anywhere.
    """

    def __init__(self, seconds: float = 2.0) -> None:
        from collections import deque
        self._buf = deque(maxlen=max(1, int(seconds * SAMPLE_RATE / _CHUNK_SAMPLES)))
        self._sink: Optional["queue.Queue"] = None
        self._stream = None
        self._lock = threading.Lock()
        self._want = False

    @property
    def live(self) -> bool:
        return self._stream is not None

    def arm(self) -> None:
        self._want = True
        if self._stream is not None:
            return

        def _open() -> None:
            try:
                import sounddevice as sd
                s = sd.InputStream(samplerate=SAMPLE_RATE, channels=1, dtype="float32",
                                   blocksize=_CHUNK_SAMPLES, callback=self._callback)
                s.start()
                with self._lock:
                    if self._want and self._stream is None:
                        self._stream = s
                        return
                s.close()
            except Exception:
                pass  # no mic, or it's busy: Ctrl+D falls back to opening its own stream

        threading.Thread(target=_open, name="agentdesk-mic-arm", daemon=True).start()

    def disarm(self) -> None:
        with self._lock:
            self._want = False
            if self._sink is not None:
                return  # a session is using it; release when it detaches
            stream, self._stream = self._stream, None
            self._buf.clear()
        if stream is not None:
            try:
                stream.close()
            except Exception:
                pass

    def _callback(self, indata, _frames, _time, _status) -> None:
        chunk = indata[:, 0].copy()
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
            release = not self._want
        if release:
            self.disarm()


class Session:
    """One press-to-stop dictation session, running on its own thread.

    `on_text(text, is_final)` is called every time the recognized text
    changes and once more, with is_final=True, after stop() has drained the
    last of the audio. Like every other callback in this module, it fires
    from the background thread.

    `on_error(exc)` is called instead, at most once, if the microphone or the
    model raises -- captured here rather than left to crash a daemon thread
    silently, which is what a background thread's uncaught exception
    otherwise does.
    """

    def __init__(self, on_text: Callable[[str, bool], None],
                 on_error: Callable[[Exception], None], mic: Optional[Mic] = None) -> None:
        self._mic = mic
        self._on_text = on_text
        self._on_error = on_error
        self._audio_q: "queue.Queue[Optional[object]]" = queue.Queue()
        self._stream_handle = None
        self._worker = threading.Thread(target=self._run, daemon=True)
        self._stopping = threading.Event()

    def start(self) -> None:
        self._worker.start()

    def stop(self) -> None:
        """Signal end-of-audio. Non-blocking; the final callback still comes
        asynchronously once the tail of the buffered audio is decoded."""
        self._stopping.set()
        self._audio_q.put(None)  # wakes the worker if it is blocked waiting

    def _run(self) -> None:
        try:
            import sounddevice as sd

            def _callback(indata, _frames, _time, status) -> None:
                # Runs on PortAudio's own thread, a THIRD thread besides this
                # one and the tk thread -- so this may not touch the
                # recognizer either. It only ever hands samples across a
                # queue, same discipline as pystray's callbacks in app.py.
                self._audio_q.put(indata[:, 0].copy())

            last_text = ""
            # The mic opens BEFORE the model is fetched: on a cold start the model
            # takes seconds to load, and audio captured meanwhile waits in the
            # queue instead of being lost.
            def _loop(recognizer, stream, last_text):
                while True:
                    try:
                        chunk = self._audio_q.get(timeout=0.5)
                    except queue.Empty:
                        if self._stopping.is_set():
                            break
                        continue
                    if chunk is None:
                        break
                    stream.accept_waveform(SAMPLE_RATE, chunk)
                    while recognizer.is_ready(stream):
                        recognizer.decode_stream(stream)
                    text = recognizer.get_result(stream).strip()
                    if text and text != last_text:
                        last_text = text
                        self._on_text(text, False)
                return last_text

            if self._mic is not None and self._mic.attach(self._audio_q):
                # Pre-roll path: the mic is already open and the last seconds are queued.
                try:
                    recognizer = _get_recognizer()
                    stream = recognizer.create_stream()
                    last_text = _loop(recognizer, stream, last_text)
                finally:
                    self._mic.detach()
            else:
                with sd.InputStream(samplerate=SAMPLE_RATE, channels=1,
                                    dtype="float32", blocksize=_CHUNK_SAMPLES,
                                    callback=_callback):
                    recognizer = _get_recognizer()
                    stream = recognizer.create_stream()
                    last_text = _loop(recognizer, stream, last_text)

            # Drain whatever audio is still queued (the mic keeps producing
            # for a moment after the InputStream context exits) before the
            # true final decode, or the last word or two is dropped.
            while True:
                try:
                    chunk = self._audio_q.get_nowait()
                except queue.Empty:
                    break
                if chunk is not None:
                    stream.accept_waveform(SAMPLE_RATE, chunk)

            stream.input_finished()
            while recognizer.is_ready(stream):
                recognizer.decode_stream(stream)
            final_text = recognizer.get_result(stream).strip()
            self._on_text(final_text or last_text, True)
        except Exception as exc:  # noqa: BLE001 - reported, not swallowed
            self._on_error(exc)
