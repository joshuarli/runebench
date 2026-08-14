# pi-agent-core-rs on Runebench

Runebench's local agent path is a concrete world host for
[`pi-agent-core-rs`](../pi-agent-core-rs). It does not install, invoke, or
extend the Pi TypeScript SDK/CLI.

The boundary is deliberate:

```text
OpenRouter or Command Code key ──► Rust provider adapter
                                           │
Pi default profile ───────────────────► pi-agent-core-rs ◄── Luau policy
                                 │                      │
                                 │          prompts + declared coroutine handlers
                                 ▼
                explicit Rust rs-agent MCP capability
                                 │
                                 ▼
                         Runebench game server
```

The pinned Pi default system prompt and the active `read`, `bash`, `edit`, and
`write` tools come from the Rust core. The Runebench policy is
[`agents/runebench-policy.luau`](agents/runebench-policy.luau); it appends the
game guidance and declares the five `rs-agent` tools. The Rust host rejects
any policy capability other than those known bindings.

`OPENROUTER_API_KEY` or `COMMANDCODE_API_KEY` is passed only to the selected
Rust provider adapter. The default shell tools receive an explicit `PATH` but
no provider credential. The Rust-owned MCP client launches its Bun world server
with a cleared environment.

## Local smoke run

```bash
# Show the resolved Harbor configuration.
make agent-core-config

# Build native arm64 images, generate the task image reference, and run the
# five-minute Woodcutting task with the key supplied by the vault.
make agent-core
```

The default model is `openrouter/deepseek/deepseek-v4-flash-0731`. Override it
without changing tracked configuration:

```bash
AGENT_CORE_MODEL=openrouter/nvidia/nemotron-3.5-lightning:free make agent-core
AGENT_CORE_TASK=tasks/mining-xp-5m make agent-core
AGENT_CORE_RUN_DEADLINE_SEC=390 make agent-core
AGENT_CORE_AGENT_TIMEOUT_MULTIPLIER=0.22 make agent-core
```

The final command is always executed under:

```bash
vault OPENROUTER_API_KEY -- ...
```

If OpenRouter returns HTTP 404, the host reports that a key restricted to Zero
Data Retention models is a likely cause. Choose a model that OpenRouter marks
as compatible with that requirement rather than retrying an unavailable model.

## Command Code smoke run

The same host also accepts `commandcode/<model>` names. Its local target takes
the secret from an explicitly selected environment file, rather than invoking
the OpenRouter vault boundary:

```bash
AGENT_CORE_COMMANDCODE_ENV_FILE=../pi-agent-core-rs/.env \
  make agent-core-commandcode
```

The default model is `commandcode/poolside/laguna-s-2.1-free`. The Harbor
adapter creates one UUID per trial and supplies explicit `linux`, date, and
`runebench` project metadata to the Command Code provider; these values are
not discovered by `pi-agent-core-rs` itself.

## Artifacts and diagnosis

The host writes the following agent artifacts during a trial:

- `agent/pi-agent-core.jsonl`: lifecycle events used by the live monitor.
- `agent/pi-agent-core.txt`: process output and aggregate token counts.
- `agent/runebench-pi-agent-core.json`: non-secret policy/MCP startup audit.

`make agent-core` runs the read-only monitor in `scripts/run-pi-live.ts`.
Harbor remains responsible for container lifecycle, task timeout, verifier, and
result artifact collection. Use `make agent-core-direct` only when diagnosing
Harbor itself.

`AGENT_CORE_RUN_DEADLINE_SEC` defaults to 390 seconds for the five-minute
Woodcutting task, leaving 30 seconds before Harbor's 420-second agent limit.
The Rust host requests structured cancellation at that deadline and reaps any
in-flight provider, MCP, or foreground shell child. It intentionally does not
kill detached game workers, which may need to continue while the verifier
collects the world result.

## Image construction

`make agent-core-image` builds a game base image and then an agent-core image.
The latter uses BuildKit's named `pi_agent_core` context so the Docker build
copies only `pi-agent-core-rs` source into a Rust builder stage. That stage
checks the repository's exact `nightly-2026-07-24` compiler before building;
the final game image contains only the release binary and policy. The Bun MCP
server remains part of the Runebench world image, not an agent TypeScript
client bridge.

The default cloud image and ordinary `bun generate-tasks.ts` workflow remain
unchanged. The local agent-core image is selected only through
`RUNEBENCH_DOCKER_IMAGE` in the make targets.

## Extending the policy

Edit `agents/runebench-policy.luau` to change Runebench-specific prompt text,
the declared game-tool schemas, pre-tool allow/block decisions, or a declared
coroutine handler. It cannot acquire a new host capability by naming one.
Adding a new MCP binding requires a Rust `LuauCapability` implementation,
a narrowed `CapabilityManifest` grant, policy-handler wiring, and tests.

For the full extension contract, sandbox behavior, limits, and review checklist
see [`pi-agent-core-rs/docs/luau-extensions.md`](../pi-agent-core-rs/docs/luau-extensions.md).
