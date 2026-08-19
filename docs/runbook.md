# Runbook

Operational steps for the runtime services vault depends on. The TEI launcher
(`vault tei start|stop|status|logs`) **is implemented** (`src/tei/launcher.rs`)
and handles spawn + PID/log management for you. The equivalent Gemma/mlx launcher
is **not** built yet, so the Gemma service is still started by hand. The manual
recipes below remain the fallback for TEI (and the only option for Gemma); they
also document exactly what `vault tei start` automates.

Two install paths are documented, and they are not interchangeable — pick by
platform:

| Platform | Path | Notes |
|---|---|---|
| Arch / Linux + NVIDIA | Docker image | CUDA images need compute capability ≥ 7.5; Pascal (GTX 10xx) must use `cpu-latest` |
| macOS, Apple Silicon | Homebrew native binary | Metal acceleration is baked into the bottle |

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

### One-time install (Arch Linux / NVIDIA)

**Use the Docker CPU image.** Verified working on Arch with a GTX 1080 Ti.

#### Why not the GPU

TEI publishes CUDA images only for these architectures — confirmed against the
`ghcr.io/huggingface/text-embeddings-inference` tag list:

| Tag family | Architecture | Compute capability |
|---|---|---|
| `cpu` | — | — |
| `turing` | Turing (T4, RTX 20xx) | 7.5 |
| `86` | Ampere (A10, A40, RTX 30xx) | 8.6 |
| `89` | Ada (RTX 40xx) | 8.9 |
| `hopper` | Hopper (H100) | 9.0 |

**Pascal (GTX 10xx, compute 6.1) is below every one of them.** Building from
source does not help: TEI's CUDA features (`candle-cuda`, `candle-cuda-turing`)
depend on flash-attention kernels that have no Pascal implementation. The card
itself is more than capable of a 137M-parameter model — the gap is TEI's build
targets, not the hardware.

Check your card before assuming the GPU path is available:

```bash
nvidia-smi --query-gpu=name,compute_cap --format=csv,noheader
```

`7.5` or higher → use the matching CUDA tag above and add `--gpus all`.
Below `7.5` → CPU image.

#### Run

```bash
mkdir -p ~/.cache/tei-data
docker run -d --name vault-tei \
    -u "$(id -u):$(id -g)" \
    -p 127.0.0.1:8081:80 \
    -v "$HOME/.cache/tei-data:/data" \
    ghcr.io/huggingface/text-embeddings-inference:cpu-latest \
    --model-id nomic-ai/nomic-embed-text-v1.5
```

`-p 127.0.0.1:8081:80` is deliberate — TEI listens on port 80 inside the
container, and binding the host side to loopback keeps the
"[loopback only](security.md)" rule intact. Publishing it as `-p 8081:80` would
expose the embeddings server to the local network.

Add `--restart unless-stopped` if you want it to survive a reboot.

First start takes ~90s (model download plus a backend fallback, below); warm
starts are ~30s. `docker logs -f vault-tei` and wait for `Ready`.

#### Expected: the ONNX backend fails and TEI falls back to Candle

On first start the log shows:

```
ERROR Could not start ORT backend: Failed to parse `config.json`:
      duplicate field `hidden_size` at line 38 column 10
INFO  Downloading `model.safetensors`
INFO  Starting NomicBert model on Cpu
```

This is **expected and not a misconfiguration.** `nomic-embed-text-v1.5`'s
`config.json` carries both `n_embd` and `hidden_size`; TEI's ORT config parser
aliases them onto one field and rejects the pair as a duplicate. TEI then falls
back to the Candle backend, which works correctly.

Do **not** try to fix this by deleting `n_embd` from the cached `config.json` —
Candle's NomicBert loader requires that key, so the edit breaks *both* backends
and the container will not start.

The consequence is speed, not correctness. Measured on a 32-core box:

| Workload | Latency |
|---|---|
| Short query (the hook's hot path) | ~30 ms |
| ~200-token chunk | ~220 ms |
| ~1400-token chunk | ~1.9 s |
| Sustained sync throughput | ~2–5 chunks/s |

The hook is unaffected — 30 ms against a 3 s budget. `vault index sync` is the
slow path: expect minutes on a large repo. If that becomes a problem, the options
are to switch to a model with a working ORT path (`BAAI/bge-base-en-v1.5` is also
768-dim, so `[embeddings].dims` and the locked schema are unchanged — but
`src/embed/tei.rs` hardcodes nomic's `search_document:`/`search_query:` prefixes,
so it needs a code change plus a full re-index), or to run a non-TEI embeddings
server that supports Pascal via ONNX Runtime's CUDA execution provider.

#### Wire it into `vault tei start`

```toml
[embeddings]
launcher_cmd = "/usr/bin/docker run --rm --name vault-tei -u 1000:1000 -p 127.0.0.1:8081:80 -v /home/YOU/.cache/tei-data:/data ghcr.io/huggingface/text-embeddings-inference:cpu-latest --model-id nomic-ai/nomic-embed-text-v1.5"
```

Two constraints, both load-bearing:

- **Foreground, not `-d`.** `vault tei start` spawns this command and records the
  child PID (`src/tei/launcher.rs`). `docker run -d` returns immediately, so vault
  would store a PID that is already dead and `vault tei stop` would do nothing.
  `--rm` cleans the container up when vault kills it.
- **Absolute paths, no shell.** `launcher_cmd` is split on whitespace and executed
  directly, so `$HOME`, `~`, and `$(id -u)` do **not** expand. Write them out.

Note that `vault tei status` reports `reachable: yes` as soon as the port is bound,
which happens *before* the model finishes loading. For true readiness poll
`/health`:

```bash
until curl -fs http://localhost:8081/health >/dev/null; do sleep 1; done; echo ready
```

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

### Start (native binary)

For the Docker path see the Arch section above — the container is already running
after `docker run`, so this section does not apply there.

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

Docker (the Arch path):

```bash
docker stop vault-tei          # started with `docker run -d`
docker rm   vault-tei          # only if you did not pass --rm
vault tei stop                 # if started via launcher_cmd
```

Native binary — if running in foreground: Ctrl-C.

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
| Cold-start latency > 30s | Model still downloading | Check the cache dir size; let it finish |
| `Could not start ORT backend: duplicate field hidden_size` | Expected on nomic-embed-text-v1.5 — TEI falls back to Candle | Ignore. Do **not** delete `n_embd` from `config.json`; that breaks Candle too and neither backend will start |
| Sync is slow (minutes), hook is fine | Candle CPU backend; ORT unavailable for this model | Expected. See the throughput table in the Arch section for options |
| CUDA image exits immediately on a GTX 10xx | Pascal (compute 6.1) is below TEI's minimum of 7.5 | Use `cpu-latest`. Check with `nvidia-smi --query-gpu=compute_cap --format=csv,noheader` |
| `vault tei status` says reachable but embeds fail | Port is bound before the model finishes loading — status uses a TCP probe | Poll `/health` instead: `until curl -fs localhost:8081/health; do sleep 1; done` |
| `vault tei start` leaves nothing running | `launcher_cmd` used `docker run -d`, so the recorded PID exited instantly | Drop `-d`; the launcher needs a foreground process to track |

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
