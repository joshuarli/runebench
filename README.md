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

### Local Pi smoke run

The repository includes a local Pi path using the latest Harbor checkout and
Pi's current `@earendil-works/pi-coding-agent` package. The Pi adapter bridges
Runebench's `rs-agent` MCP server into Pi custom tools, because Pi intentionally
does not include built-in MCP.

```bash
# Verify the resolved Harbor/Pi task configuration without running a task.
make pi-config

# Build native arm64 images and run the five-minute woodcutting smoke task locally.
make pi

# Override the task, model, or Harbor checkout when needed.
PI_TASK=tasks/mining-xp-5m make pi
PI_MODEL=openrouter/poolside/laguna-xs-2.1:free make pi
HARBOR_PROJECT=/path/to/harbor make pi
```

`make pi` builds `runebench-base:local-arm64-pi` and `runebench:local-arm64-pi`
locally, so Apple Silicon uses native containers rather than emulating the
published amd64 image. The Pi image omits the optional audio/video recording
stack and other coding-agent CLIs; Chromium remains because it runs the game
client. The default published image remains unchanged for
ordinary `bun generate-tasks.ts` and cloud workflows.

`make pi` expects `vault OPENROUTER_API_KEY -- ...` to provide the key to the
local Harbor process. Its default is the paid
`deepseek/deepseek-v4-flash-0731` endpoint with Pi `thinking=high`; OpenRouter
model IDs and thinking level are overrideable through `PI_MODEL` and
`PI_THINKING`. The command uses a read-only live wrapper around `harbor run` to
report Pi-log, tracker, process, prompt, MCP-bridge, and provider health.

See [PI.md](PI.md) for the full leaderboard workflow, graph-generation path,
cost guidance, and a breakdown of the Pi/MCP adapter.

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
