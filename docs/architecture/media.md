# Media, voice & vision

These subsystems are interfaces at this stage — the contracts are designed
so implementations slot in without touching the agent or the FFI.

## pai-media — generation & transcode jobs

```
MediaJob { id, kind, state, input, params, output? }
MediaJobKind = ImageGen | ImageEdit | VideoGen | AudioGen | Transcode | …
JobState     = Queued | Running | Done | Failed | Cancelled
```

Jobs are durable (in the store) so a 10-minute video render survives an app
restart; `JobState` transitions are audited. Backends plug in per kind —
free/local first: stable-diffusion.cpp / whisper.cpp / ffmpeg for transcode.

## pai-vision — understanding, not generation

`VisionRequest { image, task (Describe|Ocr|Detect|Classify), detail }` →
`VisionResult { text, detections[], confidence }`. First impl targets
llama.cpp multimodal (LLaVA-class GGUFs) and ONNX detection models — both
free/local. Results feed the agent as `Content::Image` + untrusted
`ToolResult` text.

## pai-voice — the pipeline

```rust
trait Vad  { fn frame(&mut self, pcm) -> VoiceActivity }
trait Stt  { fn transcribe(&self, pcm) -> Result<String> }
trait Tts  { fn synthesize(&self, text) -> Result<AudioBuffer> }

VoicePipeline { vad, stt, tts }.turn(pcm) -> transcript/response audio
```

whisper.cpp for STT, piper/silero for VAD+TTS — all free, all local, all
routable through `pai-broker` (a phone can ask a trusted desktop to run the
heavy STT model under `LocalPreferred`).

## Multimodal `Content`

`pai_core::Content` already carries `Image | Audio | Video | DocumentRef |
ToolCall | ToolResult` — messages are multimodal today even though the
perception backends are interface-stage. `MediaAsset` rows reference
content-addressed blobs, so media dedupes across conversations.

## Why interfaces first

- Model backends here move fast; the trait is stable, the impl churns.
- The expensive part is *policy* (may the agent synthesize speech? store an
  image? send it anywhere?) — that already works through the same
  permission/audit path as text tools.
