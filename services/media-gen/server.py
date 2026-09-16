"""Reference media-generation server for Personal AI.

Implements the `pai media` provider protocols — one service covering all
three adapter contracts so the whole pipeline is testable end-to-end:

    GET  /                        -> 200 (liveness probe; `pai media status`)
    POST /generate                -> audio contract:  {"prompt",
                                     "duration_seconds"} -> audio/wav bytes
                                  -> onnx image contract: {"prompt",
                                     "width","height","image_b64"?}
                                     or explicit "kind":"image" -> PNG
                                  -> video contract: {"prompt",
                                     "kind":"video"} -> {"job_id"}
    GET  /jobs/{id}               -> {"status": queued|running|done|failed,
                                     "result_b64"?, "error"?}
    POST /v1/images/generations   -> sdcpp adapter's OpenAI-images
                                     contract -> {"data":[{"b64_json"}]}

Audio backends, picked by `--backend` (default: auto-detect):

- `sdcpp`    — shells out to stable-audio.cpp's `sad`/`stable-audio` binary.
             Set `PAI_AUDIO_SDCPP_BIN` or pass `--sdcpp-bin`.
- `musicgen` — Meta's MusicGen via the `audiocraft` package (pip extra:
             `pip install -r requirements-musicgen.txt`).
- `tone`     — zero-dependency test backend. Synthesizes a deterministic
             sine/sweep keyed off the prompt hash.

Images and video are `stub` outputs: deterministic, syntactically valid
containers (real PNG, minimal MP4) whose content is a prompt-hash
gradient — enough to verify wiring, job rows, blob storage, and export
without any model weights. Point pai at a real sdcpp/diffusers-onnx
deployment for real pixels.

Run:

    pip install -r requirements.txt
    uvicorn server:app --host 127.0.0.1 --port 8179
    # then:
    #   pai media configure --audio-gen-url http://127.0.0.1:8179 \
    #       --image-gen-url http://127.0.0.1:8179 --image-backend onnx \
    #       --video-gen-url http://127.0.0.1:8179
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import io
import math
import os
import struct
import subprocess
import tempfile
import time
import uuid
import wave
import zlib

from fastapi import FastAPI, HTTPException
from fastapi.responses import Response
from pydantic import BaseModel, Field

SAMPLE_RATE = 32000  # stable-audio / MusicGen both emit 32 kHz


class GenerateRequest(BaseModel):
    prompt: str = Field(min_length=1)
    # Absent kind: inferred — video only when "video"; image when a size
    # is given; otherwise audio (the original contract).
    kind: str | None = None
    duration_seconds: int = Field(default=10, ge=1, le=300)
    width: int | None = None
    height: int | None = None
    image_b64: str | None = None  # source image (edit/upscale)


class ImagesGenerationRequest(BaseModel):
    """stable-diffusion.cpp's OpenAI-images subset."""

    prompt: str
    size: str = "512x512"
    response_format: str = "b64_json"
    image: str | None = None  # data-URI source image (edits)


# ---------------------------------------------------------------------------
# Audio backends — each returns WAV bytes.
# ---------------------------------------------------------------------------


def _tone(prompt: str, seconds: int) -> bytes:
    """Deterministic stereo-pan sweep keyed off the prompt hash."""
    seed = sum(ord(c) for c in prompt) % 400
    freq = 220.0 + seed
    n = SAMPLE_RATE * seconds
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SAMPLE_RATE)
        frames = bytearray()
        for i in range(n):
            t = i / SAMPLE_RATE
            # gentle sweep + decay envelope so it sounds generated, not pure
            f = freq * (1.0 + 0.25 * math.sin(2 * math.pi * t / max(seconds, 1)))
            env = min(1.0, 4.0 * t) * min(1.0, 4.0 * (seconds - t))
            v = int(0.4 * env * 32767 * math.sin(2 * math.pi * f * t))
            frames += struct.pack("<h", v)
        w.writeframes(bytes(frames))
    return buf.getvalue()


def _sdcpp(prompt: str, seconds: int, binary: str) -> bytes:
    """stable-audio.cpp — `sad -p <prompt> -d <secs> -o <out.wav>`."""
    with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as tmp:
        out = tmp.name
    try:
        proc = subprocess.run(
            [binary, "-p", prompt, "-d", str(seconds), "-o", out],
            capture_output=True,
            text=True,
            timeout=seconds * 60 + 300,
        )
        if proc.returncode != 0:
            raise RuntimeError(f"sdcpp exited {proc.returncode}: {proc.stderr[-400:]}")
        with open(out, "rb") as f:
            return f.read()
    finally:
        os.unlink(out) if os.path.exists(out) else None


_musicgen_model = None


def _musicgen(prompt: str, seconds: int, model_name: str) -> bytes:
    """MusicGen via audiocraft (lazy-loaded — first call downloads weights)."""
    global _musicgen_model
    if _musicgen_model is None:
        from audiocraft.models import MusicGen  # type: ignore

        _musicgen_model = MusicGen.get_pretrained(model_name)
    _musicgen_model.set_generation_params(duration=seconds)
    wav = _musicgen_model.generate([prompt])[0].cpu()  # (channels, samples)
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(wav.shape[0])
        w.setsampwidth(2)
        w.setframerate(_musicgen_model.sample_rate)
        pcm = (wav.clamp(-1, 1) * 32767).to("int16")
        w.writeframes(pcm.t().contiguous().numpy().tobytes())
    return buf.getvalue()


# ---------------------------------------------------------------------------
# Stub image / video — deterministic synthetic output for contract testing.
# ---------------------------------------------------------------------------


def _stub_png(prompt: str, width: int, height: int, src: str | None = None) -> bytes:
    """Pure-stdlib PNG: horizontal gradient keyed off the prompt hash."""
    w = max(8, min(width or 512, 2048))
    h = max(8, min(height or 512, 2048))
    seed = hashlib.sha256((prompt + (src or "")).encode()).digest()
    r0, g0, b0 = seed[0], seed[8], seed[16]
    raw = bytearray()
    for y in range(h):
        raw.append(0)  # filter byte per scanline
        for x in range(w):
            t = x / max(w - 1, 1)
            u = y / max(h - 1, 1)
            raw += bytes(
                (
                    int(r0 * t + (255 - r0) * (1 - t)),
                    int(g0 * u + (255 - g0) * (1 - u)),
                    int(b0 * (1 - t) + (255 - b0) * t),
                )
            )

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)  # 8-bit RGB
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 6))
        + chunk(b"IEND", b"")
    )


def _stub_mp4(prompt: str, seconds: int) -> bytes:
    """Minimal MP4 container: ftyp + mdat filled with a prompt-hash stream.

    Valid box structure, synthetic payload — a wire contract, not footage.
    """
    payload_len = 4096 + (seconds * 256)
    stream = hashlib.shake_256((prompt + str(seconds)).encode()).digest(payload_len)
    ftyp = b"ftypisom" + struct.pack(">I", 0x200) + b"isommp42"
    mdat = b"mdat" + stream
    return struct.pack(">I", 8 + len(ftyp)) + ftyp + struct.pack(">I", 8 + len(mdat)) + mdat


def _parse_size(size: str) -> tuple[int, int]:
    try:
        w, h = size.lower().split("x", 1)
        return int(w), int(h)
    except (ValueError, IndexError):
        return 512, 512


# ---------------------------------------------------------------------------
# App
# ---------------------------------------------------------------------------


def make_app(backend: str, sdcpp_bin: str, musicgen_model: str) -> FastAPI:
    app = FastAPI(title="pai media-gen")

    def resolve(name: str) -> str:
        if name != "auto":
            return name
        if os.environ.get("PAI_AUDIO_SDCPP_BIN") or os.path.exists(sdcpp_bin):
            return "sdcpp"
        try:
            import audiocraft  # noqa: F401

            return "musicgen"
        except ImportError:
            return "tone"

    chosen = resolve(backend)

    # Async video jobs: id -> (created_at, prompt, seconds). `running`
    # until ~1.5s old, then `done` — enough state churn for poll paths.
    jobs: dict[str, tuple[float, str, int]] = {}

    @app.get("/")
    def health() -> dict:
        return {"ok": True, "backend": chosen, "protocol": "pai media-gen/1"}

    @app.post("/generate")
    def generate(req: GenerateRequest) -> Response:
        kind = req.kind
        if kind is None:
            kind = "image" if req.width or req.height else "audio"
        try:
            if kind in ("video", "text_to_video"):
                job_id = uuid.uuid4().hex
                jobs[job_id] = (time.time(), req.prompt, req.duration_seconds)
                return {"job_id": job_id, "status": "queued"}  # type: ignore[return-value]
            if kind in ("image", "text_to_image", "image_edit", "upscale"):
                data = _stub_png(
                    req.prompt,
                    req.width or 512,
                    req.height or 512,
                    req.image_b64,
                )
                return Response(content=data, media_type="image/png")
            if kind in ("audio", "text_to_audio"):
                if chosen == "sdcpp":
                    data = _sdcpp(req.prompt, req.duration_seconds, sdcpp_bin)
                elif chosen == "musicgen":
                    data = _musicgen(req.prompt, req.duration_seconds, musicgen_model)
                else:
                    data = _tone(req.prompt, req.duration_seconds)
                return Response(content=data, media_type="audio/wav")
            raise HTTPException(status_code=400, detail=f"unknown kind '{kind}'")
        except HTTPException:
            raise
        except Exception as e:  # backend failures surface as 502
            raise HTTPException(status_code=502, detail=f"{chosen}: {e}") from e

    @app.get("/jobs/{job_id}")
    def job_status(job_id: str) -> dict:
        job = jobs.get(job_id)
        if job is None:
            raise HTTPException(status_code=404, detail="unknown job")
        created, prompt, seconds = job
        age = time.time() - created
        if age < 0.5:
            return {"status": "queued"}
        if age < 1.5:
            return {"status": "running"}
        return {
            "status": "done",
            "result_b64": base64.b64encode(_stub_mp4(prompt, seconds)).decode(),
            "mime": "video/mp4",
        }

    @app.post("/v1/images/generations")
    def images_generations(req: ImagesGenerationRequest) -> dict:
        w, h = _parse_size(req.size)
        png = _stub_png(req.prompt, w, h, req.image)
        return {
            "created": int(time.time()),
            "data": [{"b64_json": base64.b64encode(png).decode()}],
        }

    return app


def main() -> None:
    ap = argparse.ArgumentParser(description="pai media-gen reference server")
    ap.add_argument("--backend", default="auto", choices=["auto", "sdcpp", "musicgen", "tone"])
    ap.add_argument(
        "--sdcpp-bin",
        default=os.environ.get("PAI_AUDIO_SDCPP_BIN", "sad"),
        help="stable-audio.cpp binary (default: $PAI_AUDIO_SDCPP_BIN or `sad`)",
    )
    ap.add_argument("--musicgen-model", default="facebook/musicgen-small")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8179)
    args = ap.parse_args()

    app = make_app(args.backend, args.sdcpp_bin, args.musicgen_model)

    import uvicorn

    uvicorn.run(app, host=args.host, port=args.port)


if __name__ == "__main__":
    main()
