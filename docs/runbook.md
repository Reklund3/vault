# Runbook

Operational steps for the runtime services vault depends on. The TEI launcher
(`vault tei start|stop|status|logs`) **is implemented** (`src/tei/launcher.rs`)
and handles spawn + PID/log management for you. The equivalent Gemma/mlx launcher
is **not** built yet, so the Gemma service is still started by hand. The manual
recipes below remain the fallback for TEI (and the only option for Gemma); they
also document exactly what `vault tei start` automates.

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

The dimension defaults to **768** (nomic-embed-text-v1.5). `chunks_vec` is built
at whatever `[embeddings].dims` declares, then locked per-DB — the first sync
records `(model, dim)` in the `meta` table and changing it means deleting
`vault.db` and re-syncing. Verifying the server's dim matches your configured
`dims` before any real indexing is the Step 0 prerequisite.

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

(`vault tei start` automates all of this — spawn, detach, and PID + log
management in `~/.vault/`. The manual recipe above is the fallback or for
debugging.)

### Verify

Two checks. First, health:

```bash
curl -fs http://localhost:8081/health && echo "ok"
```

Then confirm the dim matches your configured `dims` (768 for the default model) —
this is the Step 0 gate:

```bash
curl -s http://localhost:8081/v1/embeddings \
    -H "Content-Type: application/json" \
    -d '{"input": "search_document: hello world"}' \
  | jq '.data[0].embedding | length'
```

Expected output: `768` for the default model. It must match `[embeddings].dims`
in `vault.toml` — anything else means the loaded model and your configured `dims`
disagree (fix whichever is wrong). Don't proceed with `vault index sync` until
the printed length matches your `dims`.

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
| Embedding dim != configured `dims` | Wrong model loaded, or `dims` misconfigured | Confirm `--model-id` matches `[embeddings].model` (default `nomic-ai/nomic-embed-text-v1.5`, 768-dim — not `-v1` or another variant) |
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
