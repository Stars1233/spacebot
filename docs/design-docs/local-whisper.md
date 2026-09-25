# Local Whisper: Voice Transcription That Works Out Of The Box

Today, voice notes only work if you've already configured a model. `RoutingConfig::voice` defaults to `String::new()` (`src/llm/routing.rs:56`), and an empty route short-circuits in `transcribe_audio_attachment`:

```
[Audio attachment received but no voice model is configured in routing.voice: voice-message.ogg]
```

That's the first thing a new instance says when someone sends it a voice note, and it's a bad first impression. Worse, the fix isn't obvious — the working models are a short allowlist (`KNOWN_VOICE_TRANSCRIPTION_MODELS` in `src/api/models.rs:83`, currently all Gemini), because the transcription path is an OpenAI-compatible `/v1/chat/completions` call carrying an `input_audio` part. Anthropic endpoints are rejected outright.

This doc adds a local Whisper engine and makes it the default. Cloud routes stay exactly as they are, and become the override rather than the requirement.

The scope is real: this is an ASR engine plus an audio decode pipeline inside the Rust binary, not a defaults tweak.

---

## Target behavior

- A fresh instance transcribes voice notes with no configuration and no API key.
- The model file downloads on first use, like Chrome does for the browser tool.
- `routing.voice` set to a cloud model keeps today's behavior verbatim.
- Steady-state memory returns to baseline when nobody has sent a voice note in a while.

---

## Phase 1 — Audio decode

New module `src/voice.rs` + `src/voice/`, following the existing `foo.rs` + `foo/` layout.

Whisper takes 16 kHz mono `f32` PCM. What actually arrives is nothing like that:

| Source | Container / codec |
|---|---|
| Telegram voice | ogg/opus (`src/messaging/telegram.rs:1347`) |
| Discord voice message | ogg/opus |
| Slack | m4a / mp4 (AAC) |
| Email, uploads | mp3, wav, flac |

`symphonia` 0.6.1 (features `aac`, `isomp4`, `mp3`, `ogg`, `wav`, `flac`, `alac`) covers everything except Opus — symphonia still ships no Opus decoder. Opus is filled separately: the `ogg` crate demuxes the pages, and an Opus decoder turns packets into 48 kHz PCM (see Decision 1).

`src/voice/audio.rs` exposes one entry point:

```rust
/// Decode arbitrary audio bytes to the 16 kHz mono f32 PCM whisper expects.
pub fn decode_to_pcm16k(bytes: &[u8], mime_type: &str, filename: &str) -> Result<Vec<f32>, AudioError>;
```

Internally: probe with the MIME type and filename extension as hints, decode, downmix to mono by averaging channels, resample to 16 kHz with `rubato` 5. The probe reads the container header (`OggS` plus `OpusHead` for the Opus path), so a wrong hint does not fail a decodable file.

Adapters lie about MIME often enough that the dispatch in `download_attachments` (`src/agent/channel_attachments.rs:35`) has to agree. Today it only routes `audio/*` to transcription; anything else falls through to the metadata-only branch before the decoder is reached. A shared `is_audio_attachment(mime_type, filename)` predicate replaces the `starts_with("audio/")` checks there and in the saved-attachment branches of `src/agent/channel.rs`, accepting `audio/*` plus a supported extension (`ogg`, `oga`, `opus`, `m4a`, `mp3`, `wav`, `flac`) under a generic MIME such as `application/octet-stream` or `application/ogg`.

Bounds are 25 MB and `max_duration_secs` (default 10 minutes), and neither trusts the file:

- **Size** is enforced during download. The audio path rejects an adapter-reported `size_bytes` or `Content-Length` over the limit before reading, and streams the body with a running byte count that aborts at the limit. Today `download_attachment_bytes` buffers the whole body.
- **Duration** is enforced during decode. Container-reported duration only allows early rejection; the decoder counts output samples and aborts once they exceed `max_duration_secs` at 16 kHz, so a highly compressed or mislabeled file cannot expand past the limit.

Either violation returns an `AudioError` that becomes a text marker in the turn rather than a multi-minute CPU stall on a shared-cpu box.

Tests use short fixture clips — one per container — asserting sample rate, mono, and approximate duration. Negative fixtures cover a truncated file, a file whose header understates its duration, a long silent clip that compresses far below the byte limit, and an audio file delivered as `application/octet-stream`.

## Phase 2 — Whisper engine

`src/voice/whisper.rs`, built on `whisper-rs` 0.16 (bindings to whisper.cpp). Metal is enabled on macOS only:

```toml
[target.'cfg(target_os = "macos")'.dependencies]
whisper-rs = { version = "0.16", features = ["metal"] }

[target.'cfg(not(target_os = "macos"))'.dependencies]
whisper-rs = "0.16"
```

**Model storage.** `{instance_dir}/whisper/ggml-{size}-{hash12}.bin`, fetched from Hugging Face on first use. This mirrors two patterns already in the codebase: the Chrome fetcher (`src/tools/browser.rs:2567`) and the fastembed model cache (`src/main.rs:1044`). Download to a temp file, `rename` into place atomically, and single-flight the whole thing behind a mutex — two voice notes landing in the same second must not both pull 150 MB.

Each model size is pinned in source to a full Hugging Face commit SHA (the URL uses `resolve/{sha}/`, never `main`) and the file's SHA-256. The temp file is hashed and compared before the rename; any failure, including a hash mismatch, deletes the temp file and never touches the cached path. The cached filename carries the first 12 hex characters of the pinned hash, so bumping a pin changes the path; the old file for that size is deleted after the new one verifies. A cached file that fails to load is treated as corrupt: it is deleted and fetched once more, and a second failure is a local-engine failure.

**Lifecycle.** `WhisperEngine` holds:

```rust
pub struct WhisperEngine {
    state: Arc<std::sync::Mutex<EngineState>>,
    shutdown: Arc<AtomicBool>,
    config: VoiceConfig,
}

struct EngineState {
    context: Option<WhisperContext>,
    last_used: Instant,
}
```

Inference is CPU-bound and blocking, so `transcribe()` wraps it in `spawn_blocking`. The context and `last_used` share one mutex, and a transcription holds it for the whole check/use/update sequence: lock, load the context if it is `None`, run inference, set `last_used` to now, release. The lock doubles as a work queue — serializing transcription is desirable, since two clips decoding at once on a 2-core box is slower than doing them in order — and concurrent first use loads the context exactly once.

A background task drops the context after `unload_after_idle_secs` (default 600). It takes the lock with `try_lock` and skips the tick when the lock is held, since a held lock means a transcription is running. Holding the lock, it unloads only if `last_used` is older than the idle window. Because both sides read and write under the same lock, the idle task cannot unload mid-inference or act on a stale timestamp. The next voice note reloads the context from the already-downloaded file, which is fast.

On shutdown the engine sets `shutdown` and cancels the idle task. A `spawn_blocking` task cannot be aborted from outside, so inference passes whisper.cpp's abort callback a closure that reads `shutdown`; an in-flight transcription returns promptly with an error that becomes the failure marker. An in-progress model download is dropped and its temp file removed.

Tests cover concurrent first use (one load, both transcripts returned), idle unload skipped while a transcription holds the lock, idle unload skipped after a use that refreshed `last_used`, reload after unload, and shutdown during inference returning within a bound.

This idle unload isn't optional polish. `fly.toml` provisions `shared-cpu-2x` with **1 gb** of memory, and the `base` model is roughly 300 MB resident alongside LanceDB and fastembed.

**Silence handling.** Whisper hallucinates on silence and background noise — "Thank you.", "[BLANK_AUDIO]", subtitle-credit boilerplate. Without a filter these arrive in the channel as if a human said them, which is materially worse than an empty transcript. Two mitigations, both in `whisper.rs`:

- whisper.cpp's own `no_speech_thold` / `logprob_thold` segment gating
- a small phrase blocklist applied to the final transcript, dropping it to empty when it matches

Params otherwise: `n_threads = min(voice.threads, available_parallelism)`, no timestamps, language from config. Config load rejects `threads = 0`. Values above the machine's parallelism are clamped at runtime rather than rejected, since the same config may move between hosts.

## Phase 3 — Wire into the attachment path

`transcribe_audio_attachment` (`src/agent/channel_attachments.rs:190`) gains a route decision at the top:

```rust
let voice_model = routing.voice.trim();
if voice_model.is_empty() || voice_model.starts_with("local/") {
    return transcribe_locally(deps, attachment, bytes).await;
}
// existing input_audio path, unchanged
```

The `<voice_transcript name= mime=>` wrapper is emitted identically by both paths, so nothing downstream in the prompt or the conversation history moves.

Placement is unchanged from today. The cloud request already runs inline: `handle_message` and `handle_message_batch` in `src/agent/channel.rs` await `download_attachments` before `run_agent_turn`, so the channel's run loop does not process other messages or events until the transcript returns. The local path keeps that placement, with `spawn_blocking` keeping inference off the async workers. What changes is the worst case: a cloud call is one bounded HTTP request, while the local path can include a first-use model download and inference on a clip up to `max_duration_secs`. See Decision 4.

Failure handling reuses the existing per-model fallback chains (`RoutingConfig::get_fallbacks`, `src/llm/routing.rs:109`, today consumed by `SpacebotModel` in `src/llm/model.rs:809`). `routing.voice` stays a single route; a cloud fallback for a local route is an explicit entry:

```toml
[defaults.routing.fallbacks]
"local/whisper-base" = ["gemini/gemini-2.5-flash"]
```

If the local engine fails — model download blocked, codec unsupported — each non-`local/` entry in the chain is tried in order through the `input_audio` path. A size or duration bound violation does not fall back. With no chain, or when every entry fails, the existing failure marker is emitted. Nothing falls through to a cloud model the user did not name for this purpose.

`SPACEBOT_VOICE_MODEL` is only read by `Config::load_from_env` (`src/config/load.rs:1023`), the env-only path used when no config file exists, where it replaces `routing.voice`. That path has no fallback table, so a local route configured through the env var has no cloud fallback.

## Phase 4 — Config and surfaces

**Routing default.** `RoutingConfig::for_model` sets `voice: "local/whisper-base".into()`. That touches `src/llm/routing.rs:56` plus the ~15 test constructors in the same file that spell the struct out literally.

**New `[voice]` section** for the local-only knobs, threaded through `src/config/toml_schema.rs`, `src/config/types.rs`, and `src/config/load.rs`:

```toml
[voice]
language = "auto"               # or "en", "es", ...
threads = 4
unload_after_idle_secs = 600
max_duration_secs = 600
```

The model size has one source: the route. `local/whisper-small` selects `ggml-small`, which is the value the dashboard dropdown writes, so `[voice]` carries no `model` key. Config load rejects a `local/` route that does not name a supported local model (the synthetic entries below), and the same check applies to `local/` entries in fallback chains.

The `SPACEBOT_VOICE_MODEL` env override (`src/config/load.rs:1023`) already exists and keeps working. It sets the route, so for a local route it also selects the model size.

**Model list.** `src/api/models.rs` injects synthetic entries for `local/whisper-{tiny,base,small,medium,large-v3}` with `input_audio: true`, and `is_known_voice_transcription_model` accepts the `local/` prefix. This is what makes them selectable in the dashboard dropdown — `ConfigSectionEditor.tsx:216` filters the voice row on capability `voice_transcription` — and `spacebot model list --capability voice_transcription` picks them up with no CLI changes.

## Phase 5 — Build and packaging

whisper-rs needs cmake, a C++ compiler, and libclang for bindgen. The first two are already present everywhere; libclang is not.

- **`Dockerfile`** — the builder installs `cmake` and inherits g++ from `rust:bookworm`. Add `clang`/`libclang-dev`, or set `WHISPER_DONT_GENERATE_BINDINGS=1` to use the crate's pre-generated bindings and skip bindgen entirely.
- **`Dockerfile.cross-aarch64`** — use `WHISPER_DONT_GENERATE_BINDINGS=1` here regardless; cross-compiling bindgen is not worth the maintenance. Set `CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++` (the toolchain is already installed) and force `GGML_NATIVE=OFF` so cmake doesn't emit `-march=native` for the host arch.
- **`flake.nix`** — both devShells carry cmake but no clang. Add `llvmPackages.libclang` and `LIBCLANG_PATH`.

Clean builds gain a couple of minutes for whisper.cpp. `whisper-rs-sys` rebuilds only when it changes, so incremental builds are unaffected.

## Phase 6 — Docs

Voice page under `docs/content`, a README line noting transcription works with no configuration, and a CHANGELOG entry.

---

## Open decisions

**1. Opus decoder.** `opus-decoder` 0.1.1 is pure Rust, no unsafe, no FFI, RFC 8251 conformant, ~72k recent downloads — but it's five months old and single-author. The alternative is libopus through `audiopus_sys`, which is C but has been decoding the world's voice traffic for a decade, and we're already accepting a C++ toolchain for whisper.cpp. Recommendation: **libopus**. Opus bugs surface as subtly garbled transcripts, which is a miserable class of bug to chase in production.

**2. Default model size.** `base` is the right quality floor for voice notes, at ~150 MB on disk and ~300 MB resident. Idle unload bounds steady-state memory on the 1 gb fly box, but peak still lands during transcription. If that's too tight, default `base` locally and pin `routing.voice = "local/whisper-tiny"` in the fly config, or add a q5_1 quant as another supported `local/` model.

**3. Default language.** `auto` is the honest default, but Whisper's language detection is unreliable on short clips and misdetection reads as a garbled transcript rather than a wrong-language one. Forcing `"en"` measurably reduces garbage for an English-speaking instance. Recommendation: default `auto`, document the tradeoff, make it a one-line config change.

**4. Channel wait on local transcription.** Transcription runs inline in the channel turn today (Phase 3), which is acceptable for a single cloud request but not obviously for a first-use model download or a near-limit clip on a shared CPU. The alternative is to post a pending marker, transcribe on a background task, and retrigger the channel with the transcript, which lets the channel keep handling messages and worker events but reorders the voice note relative to messages sent after it. Keeping it inline with a tighter duration default is the smaller change; the background path is the one that satisfies the channel-never-blocks rule.

---

## Out of scope

The desktop voice overlay (`interface/src/routes/Overlay.tsx`, `interface/src/hooks/useAudioRecorder.ts`) is still a stub with no server-side transcribe endpoint. Once this lands, that endpoint is a thin wrapper over the same engine — worth doing, but as its own change.
