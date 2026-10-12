# codex-layer

The stability layer for codex relays.

`codex-layer` sits between a [codex](https://github.com/openai/codex) client and **any
OpenAI-compatible backend** (mlx-serve, llama.cpp, vLLM, or a cloud API). Its job is to keep
the backend's prefix cache warm across turns.

## The problem

Every turn, the codex client replays the whole conversation. Replayed prompts carry
surface-level byte jitter — regenerated timestamps, rewritten tail messages, unstable
`tools` serialization — that changes the prompt bytes without changing their meaning.
Prefix caches key on bytes, so a single jittered byte invalidates everything after it,
and the backend re-prefills tens of thousands of tokens. On long sessions that is
tens of seconds per turn.

## What it does

- **Prefix pinning with a semantic line.** Prompts whose prefix differs only by
  surface jitter are pinned to the first stable byte sequence. Real semantic changes
  pass through untouched — pinning never hides a genuine edit.
- **`tools` jitter rules.** Tool definitions are pinned only for surface-level
  serialization noise (key order, whitespace); a real schema change is never pinned.
- **Protocol translation.** `/v1/responses` ⇄ `/v1/chat/completions`, including SSE
  streaming, so the same layer works with backends that speak either dialect.
- **Media normalization.** `input_image` parts are normalized to `image_url`, so
  image input works against any OpenAI-compatible vision backend.

No hard dependency on any specific server, and none on CodexPlusPlus itself.

## Quick start

> Porting in progress — see `docs/` for the extracted design. Usage docs land with
> the first release.

```bash
cargo build --release
codex-layer --config codex-layer.toml
```

## Origin

Extracted from the prompt-prefix stabilizer relay in
[CodexPlusPlus](https://github.com/BigPizzaV3/CodexPlusPlus)
([PR #2449](https://github.com/BigPizzaV3/CodexPlusPlus/pull/2449)).
Licensed under AGPLv3, same as the upstream it derives from.

## License

AGPLv3. See [LICENSE](LICENSE).
