# runescape-bench

[View results website](https://maxbittker.github.io/runebench/)

Benchmark suite for evaluating AI coding agents on RuneScape gameplay tasks via [rs-sdk](https://github.com/MaxBittker/rs-sdk).

<div align="center">
    <img src="views/hero.png" alt="Average XP per Skill over 30 minutes across models" width="800">
</div>

Agents play the game by writing and executing TypeScript snippets against an emulated game server running at 8x speed. Each agent also gets a folder of markdown files extracted from the game wiki for strategy reference. Agents are scored on their peak XP rate — the best XP/min measured in any 15-second window.

Built for [Harbor](https://harborframework.com/), an open-source framework for running agent benchmarks. Built on [rs-sdk](https://github.com/MaxBittker/rs-sdk) and the [LostCity](https://github.com/LostCityRS/Server) engine/client.

## Tasks

**16 Skill XP tasks (15 min)** — Train a single skill, scored on peak XP rate

**16 Skill XP tasks (30 min)** — Extended versions with time-series tracking

**8 Gold accumulation tasks** (4 starting conditions × 15 min / 30 min) — Maximize total coins using any strategy

All task directories are generated from `generate-tasks.ts` and should not be edited directly.

## Tested Models

Claude Opus 4.6, Claude Opus 4.5, Claude Sonnet 4.6, Claude Sonnet 4.5, Claude Haiku 4.5, Gemini 3 Pro, Gemini 3.1 Pro, Gemini 3 Flash, Codex CLI 5.2, Codex CLI 5.3, GPT-5.4, GLM 5, Kimi K2.5, Qwen3 Coder Next, Qwen3.5 35B

## Quick Start

```bash
bun install
bun generate-tasks.ts
harbor run
```

### Local pi-agent-core-rs smoke run

The local Runebench path uses `pi-agent-core-rs` directly—there is no Pi
TypeScript SDK or Pi CLI in this workflow. The Rust host uses the core's pinned
Pi default profile, then loads a Runebench Luau policy that declares the
`rs-agent` game tools. Rust binds those declarations to the task's MCP server.

```bash
# Verify the resolved Harbor task configuration without running a task.
make agent-core-config

# Build native arm64 images and run the five-minute woodcutting smoke task locally.
make agent-core

# Run the same smoke task in the Smolworld external-NIC VM.
make agent-core-direct-smolworld

# Override the task, model, or Harbor checkout when needed.
AGENT_CORE_TASK=tasks/mining-xp-5m make agent-core
AGENT_CORE_MODEL=openrouter/nvidia/nemotron-3.5-lightning:free make agent-core
HARBOR_PROJECT=/path/to/harbor make agent-core
```

`make agent-core` builds `runebench-base:local-arm64-agent-core` and
`runebench:local-arm64-agent-core`
locally, so Apple Silicon uses native containers rather than emulating the
published amd64 image. The agent-core image omits the optional audio/video
recording stack and other coding-agent CLIs; Chromium remains because it runs
the game client. The default published image remains unchanged for
ordinary `bun generate-tasks.ts` and cloud workflows.

`make agent-core` expects `vault OPENROUTER_API_KEY -- ...` to provide the key
to the local Harbor process. Its default is
`deepseek/deepseek-v4-flash-0731`; the OpenRouter model ID is overrideable with
`AGENT_CORE_MODEL`. The command uses a read-only live wrapper around `harbor
run` to report agent-core log, tracker, process, prompt, MCP-bridge, and
provider health.

`make agent-core-direct-smolworld` exports the native agent-core image as a
host-prepared OCI archive, seals `smolworld/.smolworld`, and runs the agent and
verifier through Smolworld's namespaced exec/copy boundaries. Docker is used
only to prepare that local archive; the guest uses smolvm's explicit NAT egress
path at runtime and never pulls the workload image from a registry.

The live target requires a checked-out, patched smolworld/smolvm pair and
prepared runtime artifacts. For a source checkout, the invocation is:

```bash
vault COMMANDCODE_API_KEY -- env \
  PATH="/opt/homebrew/opt/e2fsprogs/sbin:$PATH" \
  SMOLWORLD_BIN="$HOME/d/smolworld/target/debug/smolworld" \
  SMOLWORLD_SMOLVM="$HOME/d/smolvm/target/debug/smolvm" \
  SMOLVM_AGENT_ROOTFS="$HOME/d/smolvm/target/agent-rootfs" \
  SMOLVM_LIB_DIR="$HOME/d/smolvm/lib" \
  DYLD_LIBRARY_PATH="$HOME/d/smolvm/lib" \
  AGENT_CORE_SMOLWORLD_VAULT_KEY=COMMANDCODE_API_KEY \
  AGENT_CORE_SMOLWORLD_CREDENTIAL_ENV=COMMANDCODE_API_KEY \
  AGENT_CORE_MODEL=commandcode/poolside/laguna-s-2.1-free \
  make agent-core-direct-smolworld
```

The Make target uses Docker only to build and export the local
`smolworld/agent-core.tar`; it then runs `prepare`, `check`, the foreground
world supervisor, guest commands, and exact world cleanup. Teardown retries for
up to two minutes while the smolworld supervisor's lifecycle lock and VM shutdown
settle, and reports the last scoped error if cleanup still fails; cleanup failure
turns an otherwise successful run into a failure. The provider key is
read by `vault` on the host and passed only to the delegated agent command via
`--secret-env`; it is not written to the Smolfile, world state, or material
lock. Override `AGENT_CORE_SMOLWORLD_VAULT_KEY` and
`AGENT_CORE_SMOLWORLD_CREDENTIAL_ENV` together when using another provider.

See [PI_AGENT_CORE.md](PI_AGENT_CORE.md) for the core-host/MCP architecture and
full local workflow. Luau extension authors should start with
[`pi-agent-core-rs/LUA.md`](../pi-agent-core-rs/LUA.md).

## Architecture

Each task runs inside a Docker container based on a pre-built image that bundles the rs-sdk game server at 8x speed. The agent connects via an MCP server that exposes game interaction tools. A verifier script checks the final game state to produce a score.

```
Agent (Claude, Gemini, Codex, etc.)
  │
  ├── MCP Server (TypeScript SDK)
  │     └── Game Server (8x speed, headless)
  │
  └── Verifier (checks peak XP rate / gold)
```

## License

MIT
