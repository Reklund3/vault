# Security

Vault sits on the hot path of every Claude Code prompt. That makes it a high-value
target and a place where small design choices have big consequences. This document
captures the design constraints that protect the user; v1 implements all of them.

For broader architecture see `olympus-vault-plan.md`. For the embedding-stack
specifics see `embeddings.md`.

---

## Threat model (v1)

- **In scope**: malicious or accidentally-poisoned content in *indexed files* (proto
  comments, vendored markdown, teammate `CLAUDE.md`, third-party design docs);
  curious or malicious *local processes* on the same machine (binding loopback
  ports, reading world-readable files); accidental *secret leakage* through
  indexing.
- **Out of scope**: a fully compromised local user account (vault cannot defend
  against an attacker who is already root or already the user); network MITM
  against the Anthropic API (TLS handles that); confidentiality of indexed content
  against a stolen unencrypted disk (rely on OS disk encryption).

---

## Trust boundaries

| Source | Trust |
|--------|-------|
| User's prompt typed in Claude Code | **Trusted** — the user is the principal |
| `vault.toml` and `~/.claude/settings.json` | **Trusted** — user-authored config |
| Indexed file content (every chunk in `vault.db`) | **Untrusted** — treat as data, never as instructions |
| Router structured output (Gemma or Haiku) | **Untrusted-shaped** — treat as parameters, never as code/SQL |
| Anything responding on `localhost:8080` / `localhost:8081` | **Trusted by assumption** — see "Localhost" below |

---

## Process boundaries are defense-in-depth, not a hard wall

Vault, Gemma (mlx_lm.server), and TEI all run as the same OS user. That means
filesystem and network permissions are **equivalent across them** — a
compromised TEI process can read `~/.vault/vault.db`, `~/.bashrc`,
`~/.aws/credentials`, anything else the user owns. It can open outbound
connections to anywhere. It can spawn shells.

What process boundary does buy:

- **Memory isolation by default.** Same-user processes can't directly
  `memcpy` from each other's heap. (Caveat: `ptrace` on Linux subject to
  `yama.ptrace_scope`, `task_for_pid` on macOS subject to SIP — often
  blocked by default but not always.)
- **Environment separation if scrubbed on spawn.** TEI doesn't need
  `ANTHROPIC_API_KEY`; vault's TEI launcher calls `env_clear()` before
  exec and re-adds only what TEI needs (see "Secrets and credentials").
  (Caveat: on Linux, `/proc/<vault_pid>/environ` is readable by the same
  user, so this is partial mitigation, not a wall.)
- **Execution context separation.** A library compromise (candle linked
  into vault) *is* vault — malicious code runs in the same address
  space and can intercept any function call. A service compromise
  (TEI) starts elsewhere and has to escalate via `/proc`, syscalls, or
  side channels. Friction, not isolation.
- **Smaller direct attack surface in vault.** Vault doesn't compile
  candle / ort / safetensors / hf-hub, so a CVE in any of those can't be
  triggered through vault directly. Still triggerable through TEI; TEI
  updates independently.

What process boundary does **not** buy on a single-user dev machine:

- Filesystem isolation
- Network egress prevention
- Persistence prevention
- Memory isolation against an attacker willing to use `ptrace` / `/proc`

Real same-user defenses are OS-level: macOS App Sandbox, Linux seccomp /
landlock, dedicated UIDs for services, read-only mounts. None of those
are in v1. The threat model already excludes a fully compromised local
user account, and "compromised Cargo dep with execution rights" is
functionally close to that.

Process boundary is still worth keeping — friction, audit-surface
reduction, and CVE-radius reduction are real benefits. Just don't read
the trust table above as a hard wall between "trusted" and "untrusted
by assumption" rows when both are running as your user.

---

## Indexed content is data, not instructions

Anyone who can write to a file vault indexes can attempt to inject instructions
into every Claude Code session that retrieves that chunk. Realistic vectors:
proto/OpenAPI comments, vendored markdown, a teammate's `CLAUDE.md`, a malicious
README in a dependency.

Defenses, in order of importance:

1. **The global `~/.claude/CLAUDE.md` instruction frames the context block as
   reference data, not commands.** See `olympus-vault-plan.md` → "CLAUDE.md
   Strategy" — the wording explicitly tells Claude not to follow instructions
   inside the block.
2. **Vault never sanitizes chunk content** in v1. Users who index untrusted repos
   accept the risk. This is documented, not silently true.
3. **The index-time secret pre-scan (see `olympus-vault-plan.md` → Indexing)
   drops chunks containing common secret patterns** before they reach the
   store. This is a safety net, not a security boundary.

---

## Indexing is opt-in, scoped, and exclusionary

- `vault index sync <repo>` is always explicit; vault never auto-indexes.
- The walker does **not** follow symlinks (`follow_links = false`). Indexing is
  bounded to the repo root via canonical-path containment.
- A default exclusion list (see `olympus-vault-plan.md` → Indexing → Exclusions)
  keeps `.env`, key material, and similar out of the index regardless of file
  extension.
- The classifier (Gemma local or Haiku fallback) sees **filename + extension +
  the first 1KB of content**, never the full file. Full-file content reaches
  Anthropic only via retrieval-time context injection, which the user can
  inspect with `vault diagnose`.

---

## Secrets and credentials

- `ANTHROPIC_API_KEY` is read from the **environment only**. Never from
  `vault.toml`, never from any file on disk that vault writes. If `mode =
  "haiku"` (or `auto` with Gemma unreachable) and the key is missing,
  `vault hook` honors the fail-open contract: router construction returns
  `MissingApiKey`, the hook treats it as any other failure — empty stdout,
  **exit 0** — and Claude Code sees no error and never blocks. The miss is
  still observable locally: a metadata-only record in `~/.vault/hook.log` plus
  a one-line stderr breadcrumb (which Claude Code shows only in debug mode).
  The missing key surfaces *loudly* where it should — in the interactive
  `vault diagnose` and `vault index sync` commands, which build the same
  Haiku backend off the fail-open hot path and report the error directly.
- Vault never logs, echoes, or includes the key in `vault diagnose` output.
- `vault.toml` may contain repo paths and domain assignments but no secrets.
- **When vault spawns a child process** (the `vault tei start` launcher for
  TEI — implemented in `src/tei/launcher.rs`), the spawn calls `env_clear()`
  and re-adds only a minimal allowlist: `PATH`, `HOME`, the HuggingFace cache
  vars (`HF_HUB_CACHE`, `HF_HOME`, `HUGGINGFACE_HUB_CACHE`), and locale
  (`LANG`/`LC_*`). On Windows it additionally passes the system vars a process
  cannot start without (`SystemRoot`, `windir`, `TEMP`, `APPDATA`, …) — these
  are not secrets. `ANTHROPIC_API_KEY` and any unrelated env are never
  inherited. This is a partial mitigation per "Process boundaries are
  defense-in-depth" above (on Linux, `/proc/<vault_pid>/environ` is still
  readable by the same user), but free to do up front and awkward to retrofit.
  The launcher also writes `tei.pid` / `tei.log` `0600` and the `~/.vault/`
  dir `0700` (best-effort, Unix).

- **The hook writes one telemetry record per invocation** to `~/.vault/hook.log`
  (`0600`, dir `0700`, size-capped rotation to `hook.log.1`). Records are
  metadata only — outcome, failed stage, truncated error detail, backend, and
  per-stage latency; never the prompt and never chunk content, so the log adds
  no plaintext-content surface beyond what `vault.db` already holds. Logging is
  best-effort and swallowed on failure — fail-open applies to the logger too.

---

## Localhost is a trust assumption, not a guarantee

Vault treats anything responding on `localhost:8080` (mlx_lm.server / Gemma) and
`localhost:8081` (TEI) as authoritative. This is fine on a single-user
workstation. It is *not* fine on:

- Multi-user systems where another user can bind those ports first
- Hosts running untrusted Docker containers with `--network=host`
- Any environment where you cannot enumerate what's listening

Operational requirements:
- Both servers MUST bind `127.0.0.1` (loopback) — never `0.0.0.0`
- The user is responsible for verifying nothing else is listening on those ports

---

## File and directory permissions

Vault creates and maintains:

```
~/.vault/                  mode 0700  (user-only)
~/.vault/vault.db          mode 0600  (user-only read/write)
~/.vault/vault.toml        mode 0600
```

`vault.db` contains plaintext indexed content (proprietary code, design docs,
conventions). Permissions stop other local users from reading it; disk-level
encryption (FileVault, BitLocker, dm-crypt) is the user's responsibility for
stolen-laptop scenarios.

---

## SQL parameter binding (non-negotiable)

Router output (`projects`, `type_names`, `topics`, `doc_types`, `languages`)
flows into SQL `WHERE` clauses. **All values are bound via rusqlite's named or
positional parameters.** No `format!` or string concatenation into SQL,
anywhere, ever. The router is treated as an untrusted-shaped output source —
a successful prompt-injection attack on indexed content could otherwise pivot
through the router into the database.

---

## Hook command resolution

`~/.claude/settings.json` should reference vault by **absolute path**:

```json
{ "hooks": { "UserPromptSubmit": [{ "command": "/usr/local/bin/vault hook" }] } }
```

A bare `"vault hook"` is PATH-resolved; anything earlier in the user's PATH
that names itself `vault` will silently intercept every Claude Code prompt.

---

## Fail-open hook behavior

Any error in `vault hook` results in stdin → stdout passthrough with exit code
0. Concretely: malformed JSON, panics, SQLite errors, router timeouts, TEI
unreachable, classifier failures — none of these block the user. The hook is
designed to fail invisibly. This is a reliability requirement and a security
posture: a vault failure must not coerce the user into running without it
"just to get unblocked."

---

## Off-localhost deployment is a v1+ shift

V1's trust posture assumes the DB stays on a single user's machine:
`vault.db` is `0600`, the directory is `0700`, and OS disk encryption is
the user's responsibility for stolen-laptop cases. Multiple rules in this
document are calibrated to that assumption.

When the DB eventually moves off the machine — multi-machine sync, team
sharing, hosted store, anything that takes plaintext indexed content
beyond the user's filesystem — at minimum these need to be revisited:

- **Indexed-content sensitivity.** "Don't index sensitive repos" (see
  `olympus-vault-plan.md` → "What not to sync") goes from a soft rule
  of thumb to a hard prerequisite. Anything indexed under laptop-local
  trust must be re-evaluated under the new posture, including content
  already in the store.
- **The secret pre-scan stops being a safety net.** Once content is on
  shared infrastructure, a secret that slipped through the pre-scan is
  exposed to whoever has access to the store, not just the local user.
- **Trust boundaries shift.** The current trust table treats `vault.db`
  as user-only. Off-localhost introduces new principals — other users,
  the hosting layer, network paths — that all need trust-boundary
  entries.
- **Filesystem permissions stop applying.** `0700` / `0600` are local
  controls. Off-localhost storage uses different access primitives
  (ACLs, IAM, signed tokens) and needs an equivalent default-deny
  posture defined explicitly.
- **The localhost trust assumption splits.** Routing and embedding stay
  on the user's machine; the store does not. The two halves of the
  pipeline now sit on different trust boundaries and the security model
  has to acknowledge both.

This is a deliberate v1+ direction, not a v1 commitment. Recorded here
so the eventual migration starts from "what does this section need to
change" rather than retrofitting a model after the fact.
