# media-gen reference server

`pai media gen` (and `pai audio gen`) talks to HTTP services implementing
small per-kind contracts. This directory is the reference implementation
covering **all three**, so the whole feature works out of the box:

```
GET  /                       → 200 (liveness; `pai media status` probes it)
POST /generate               → audio:  {"prompt", "duration_seconds"} → WAV
                             → image:  {"prompt", "width", "height",
                                        "image_b64"?} or "kind":"image" → PNG
                             → video:  {"prompt", "kind":"video"} → {"job_id"}
GET  /jobs/{id}              → {"status": queued|running|done|failed,
                               "result_b64"?, "error"?}
POST /v1/images/generations  → sdcpp adapter's OpenAI-images subset:
                               {"prompt","size"("WxH"),"image"?}
                               → {"data":[{"b64_json"}]}
```

## Backends

Audio picks one backend (`--backend`, default auto-detect):

| Backend | Command | Needs |
|---|---|---|
| `tone` (default fallback) | `python server.py --backend tone` | nothing — deterministic sine/sweep, for pipeline tests |
| `sdcpp` | `python server.py --sdcpp-bin /path/to/sad` | a built [stable-audio.cpp](https://github.com/audieleon/stable-audio.cpp) binary + model files |
| `musicgen` | `pip install -r requirements-musicgen.txt && python server.py --backend musicgen` | downloads `facebook/musicgen-small` on first call |

**Image and video are stubs**: deterministic, syntactically valid
containers (a real PNG gradient; a minimal MP4) keyed on the prompt
hash. They exercise the full pai pipeline — configure → status → gen →
job row → blob store → export — with no model weights. Point pai at a
real stable-diffusion.cpp or diffusers-onnx deployment for real output
(the wire contracts above are what those adapters speak).

## Run it

```bash
pip install -r requirements.txt
python server.py                 # auto-detects a backend, else tone
# or
uvicorn server:app --port 8179
```

## Wire it up

```bash
pai media configure --audio-gen-url http://127.0.0.1:8179 \
    --image-gen-url http://127.0.0.1:8179 --image-backend onnx \
    --video-gen-url http://127.0.0.1:8179
pai media status                     # all three should say "reachable"
pai media gen "rainy jazz cafe" --kind audio --seconds 5
pai media gen "studio portrait"      --kind image --width 512 --height 512
pai media gen "waves on a beach"     --kind video
```

`--image-backend sdcpp` instead selects the OpenAI-images adapter
(`/v1/images/generations` above — the shape stable-diffusion.cpp
serves).

On a paired-device setup, run this server on the worker host; that
device then advertises `media-run` and remote `pai media gen --device
any` jobs land on it.
