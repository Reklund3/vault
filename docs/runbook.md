# Runbook

Operational steps for the runtime services vault depends on. None of these are
managed by vault itself yet — Step 8b (`vault tei start|stop|status|logs`) and
the equivalent gemma launcher are planned but not implemented. Until they land,
start each service manually using the sections below.

Service ports are fixed by `~/.vault/vault.toml`:

| Service | Port | Endpoint key in vault.toml |
|---------|------|---------------------------|
| TEI     | 8081 | `[embeddings].endpoint`   |
| Gemma   | 8080 | `[mlx].endpoint`          |

---

## TEI — text-embeddings-inference

Provides 768-dim embeddings for `nomic-ai/nomic-embed-text-v1.5`. Required by
`vault index sync` (hard error if unreachable). The hook silently passes through
when TEI is down, so the only operational consequence of TEI being offline at
hook time is "no new context injection until it's back."

The dimension is **locked at 768** because `chunks_vec FLOAT[768]` is fixed at
schema creation. Verifying the dim before any real indexing is the Step 0
prerequisite.

### One-time install (macOS, Apple Silicon)

There's a bottled Homebrew formula with Metal acceleration baked in — use this
unless you need to build a development version:

```bash
brew install text-embeddings-inference
```

Confirm the binary is on PATH:

```bash
which text-embeddings-router
```

First run downloads the model weights (~500 MB) into `~/.cache/huggingface/`.
Subsequent runs are offline.

#### Fallback: build from source

Only needed if you want to track an unreleased version or contribute upstream.
Requires a working Rust toolchain.

```bash
git clone https://github.com/huggingface/text-embeddings-inference.git ~/code/tei
cd ~/code/tei
cargo install --path router -F metal
```

The binary lands in `~/.cargo/bin/text-embeddings-router`.

### Start

```bash
text-embeddings-router \
    --model-id nomic-ai/nomic-embed-text-v1.5 \
    --port 8081
```

Leave this running in its own terminal. Startup takes ~5–10 seconds on first
launch (longer if the model is still downloading), <1 second on warm cache.

Look for the line:

```
Ready
```

If you want it backgrounded:

```bash
nohup text-embeddings-router \
    --model-id nomic-ai/nomic-embed-text-v1.5 \
    --port 8081 \
    > ~/.vault/tei.log 2>&1 &
echo $! > ~/.vault/tei.pid
```

(Once Step 8b lands, `vault tei start` will do all of this with proper PID and
log management. Until then, manage by hand.)

### Verify

Two checks. First, health:

```bash
curl -fs http://localhost:8081/health && echo "ok"
```

Then confirm the dim is actually 768 — this is the Step 0 gate:

```bash
curl -s http://localhost:8081/v1/embeddings \
    -H "Content-Type: application/json" \
    -d '{"input": "search_document: hello world"}' \
  | jq '.data[0].embedding | length'
```

Expected output: `768`. Anything else means the schema's `FLOAT[768]` is wrong
for whatever model is loaded — either swap the model or migrate the schema.
Don't proceed with `vault index sync` until this prints 768.

### Stop

If running in foreground: Ctrl-C.

If backgrounded via the `nohup` recipe above:

```bash
kill "$(cat ~/.vault/tei.pid)"
rm ~/.vault/tei.pid
```

### Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `Address already in use` on start | Old TEI still running, or another service on 8081 | `lsof -i :8081` — kill the holder or pick a different port (and update `vault.toml`) |
| Stalls at `Downloading` | First-run model fetch, slow link | Wait. `~/.cache/huggingface/` will hold it after the first time |
| `jq` returns `null` for embedding length | TEI started but model failed to load | Check the server log — likely a Metal/macOS-version mismatch or insufficient memory |
| Embedding dim != 768 | Wrong model loaded | Confirm `--model-id` is exactly `nomic-ai/nomic-embed-text-v1.5` (not `-v1` or another variant) |
| Cold-start latency > 30s | Model still downloading | Check `~/.cache/huggingface/hub/` size; let it finish |

### Notes on the model

`nomic-embed-text-v1.5` is task-prefixed — vault applies the prefix at the
client layer (`src/embed/tei.rs`, Step 8a):

- `search_document:` at index time (long-form text)
- `search_query:` at query time (the prompt)

Forgetting the prefix produces semantically wrong embeddings (cosine scores
look reasonable but rankings are subtly worse). If `vault diagnose` shows
counterintuitive ranks once Steps 8a+ are wired, this is the first thing to
check.

---

## Gemma — mlx_lm.server

_To be added._
