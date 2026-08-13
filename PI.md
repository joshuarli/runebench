# Pi on Runebench

This document describes the current Pi integration, the local smoke workflow,
and the path from a successful Pi run to a published leaderboard row.

The integration is intentionally implemented in Runebench only. It uses
Harbor's built-in `pi` agent as the launcher and adds a Runebench-specific
MCP-to-Pi tool bridge in [`agents/pi_adapter.py`](agents/pi_adapter.py).
Harbor itself does not need to be forked or modified.

## Current status

The local path has been validated on Apple Silicon with native `linux/arm64`
containers:

- Harbor checkout: `~/d/harbor`
- Pi package: current `@earendil-works/pi-coding-agent`
- Game image: `runebench:local-arm64-pi`
- Auth: `vault OPENROUTER_API_KEY -- ...`
- MCP bridge: `rs-agent` → Pi custom tools
- DeepSeek setting: Pi `thinking=high`

A five-minute DeepSeek smoke run completed successfully and produced:

- model: `deepseek/deepseek-v4-flash-0731`
- final Woodcutting level: 35
- final XP: 11,250
- peak rate: 138 XP/min
- no Harbor or verifier exception
- estimated model cost: $0.0151

This latest run used the lean native Pi image. The earlier full-image baseline
reached level 67, 109,250 XP, and 371 XP/min, so the smoke results are not
behaviorally equivalent yet; see [the comparison](#smoke-run-comparison).
Job directories are intentionally gitignored.

## Quick local smoke run

`make pi` builds the native arm64 images, regenerates the task directories, and
runs the five-minute Woodcutting smoke task through the read-only live wrapper.
The wrapper prints periodic Pi-log, tracker, process, MCP-bridge, prompt, and
provider-health signals while Harbor remains the process in charge of the run.

```bash
make pi
```

The default model is the paid DeepSeek endpoint. Override it without changing
the repository default, including with a free OpenRouter model:

```bash
PI_MODEL=openrouter/deepseek/deepseek-v4-flash-0731 make pi
PI_MODEL=openrouter/poolside/laguna-xs-2.1:free make pi

# The default is DeepSeek V4 Flash with Pi's high thinking setting.
PI_THINKING=high make pi

# Short integration check: keep the five-minute task semantics but stop the
# agent after 90 seconds. This is useful for validating auth, MCP, prompts,
# and the tracker before paying for a full horizon.
PI_AGENT_TIMEOUT_MULTIPLIER=0.22 make pi
```

Other useful options:

```bash
# Show the resolved Harbor configuration without starting a task.
make pi-config

# Run a different generated task.
PI_TASK=tasks/mining-xp-5m make pi

# Use a different Harbor checkout.
HARBOR_PROJECT=/path/to/harbor make pi

# Bypass the live wrapper only when diagnosing Harbor itself.
make pi-direct

# Build only the local native images.
make pi-image
```

The local images are separate from the published cloud image:

```text
runebench-base:local-arm64-pi
runebench:local-arm64-pi
```

No amd64 emulation is used. The published `ghcr.io/...:v53` image remains the
default for ordinary/cloud task generation; `make pi` sets
`RUNEBENCH_DOCKER_IMAGE` to the local arm64 image.

## Full leaderboard run

The smoke target runs one task only. A complete Runebench model submission is
normally 40 tasks:

- 16 skills × 15 minutes
- 16 skills × 30 minutes
- 4 gold starting conditions × 2 horizons = 8 gold tasks

The generated five-minute Woodcutting task is a diagnostic smoke task and is
not part of the 40-task leaderboard set.

### Build and generate once

```bash
make pi-image
RUNEBENCH_DOCKER_IMAGE=runebench:local-arm64-pi bun generate-tasks.ts
```

Use one Harbor process per horizon and one concurrent trial locally to keep
memory use and provider pressure predictable. After the workflow is stable,
`-n 2` or higher can be tried, but free/provider-shared endpoints may
rate-limit concurrent requests.

### Run all 16 skill tasks for one horizon

```bash
export HARBOR_PROJECT="${HARBOR_PROJECT:-$HOME/d/harbor}"
export PI_MODEL="${PI_MODEL:-openrouter/deepseek/deepseek-v4-flash-0731}"
export PI_LABEL="deepseekflash0731"

SKILLS=(
  attack defence strength hitpoints ranged prayer magic woodcutting
  fishing mining cooking fletching crafting smithing firemaking thieving
)

run_skill_horizon() {
  horizon="$1"
  task_flags=()
  for skill in "${SKILLS[@]}"; do
    task_flags+=(-i "${skill}-xp-${horizon}")
  done

  PYTHONPATH="$PWD/agents:${PYTHONPATH:-}" \
  vault OPENROUTER_API_KEY -- \
  uv run --project "$HARBOR_PROJECT" harbor run \
    -p tasks \
    "${task_flags[@]}" \
    -e docker \
    -a 'pi_adapter:RunebenchPi' \
    -m "$PI_MODEL" \
    --job-name "skills-${horizon}-${PI_LABEL}-$(date +%Y%m%d-%H%M%S)" \
    -o jobs \
    -n 1 \
    -k 1 \
    -y
}

run_skill_horizon 15m
run_skill_horizon 30m
```

Because these tasks use real-time horizons, a sequential local run takes
roughly 12 hours for the 32 skill tasks before setup and verifier overhead.
Cloud execution or carefully increased concurrency is substantially faster.

### Run all gold tasks

```bash
CONDITIONS=(vanilla smith-alch fish fletch-alch)

run_gold_horizon() {
  horizon="$1"
  task_flags=()
  for condition in "${CONDITIONS[@]}"; do
    task_flags+=(-i "gold-${condition}-${horizon}")
  done

  PYTHONPATH="$PWD/agents:${PYTHONPATH:-}" \
  vault OPENROUTER_API_KEY -- \
  uv run --project "$HARBOR_PROJECT" harbor run \
    -p tasks \
    "${task_flags[@]}" \
    -e docker \
    -a 'pi_adapter:RunebenchPi' \
    -m "$PI_MODEL" \
    --job-name "gold-${horizon}-${PI_LABEL}-$(date +%Y%m%d-%H%M%S)" \
    -o jobs \
    -n 1 \
    -k 1 \
    -y
}

run_gold_horizon 15m
run_gold_horizon 30m
```

The full sequential wall-clock time is approximately 15 hours. That is the
task horizon total: 240 minutes for 15m skills, 480 minutes for 30m skills,
60 minutes for 15m gold, and 120 minutes for 30m gold.

### Extract the leaderboard artifacts

After the jobs finish:

```bash
bun scripts/postprocess-costs.ts
bun extractors/extract-skill-results.ts --horizon 15m
bun extractors/extract-skill-results.ts --horizon 30m
bun extractors/extract-gold-results.ts
```

The important 30-minute website artifacts are:

```text
results/skills-30m/_data.js              # committed summary data
results/skills-30m/<model>.json          # committed samples/trajectories
results/gold/_data.js                    # gold summary data
```

`_combined.json`, `jobs/`, and generated task directories are local working
artifacts. Follow the repository's `.gitignore` rules when deciding what to
publish.

Before committing, verify that the model was detected under the intended
leaderboard key:

```bash
jq '.model, (.skills | keys)' results/skills-30m/deepseekflash0731.json
```

For a new model, the extractor key must be present in
`extractors/extract-skill-results.ts`, and the display/color/icon metadata must
be present in [`views/shared-constants.js`](views/shared-constants.js). The
DeepSeek `deepseekflash0731` key is already registered.

### Graph eligibility versus statistical confidence

There are three different thresholds that are easy to conflate:

| Goal | Required coverage |
|---|---|
| Make a model appear in generated graph data | At least one valid extracted skill result plus model metadata |
| Produce a complete skills row | All 16 skills for the relevant horizon, normally the 30-minute graph |
| Produce a full leaderboard submission | 16 skills × 15m, 16 skills × 30m, and 8 gold tasks: 40 tasks total |

The extractor does not require a complete set before writing a model entry. A
partial run can therefore appear in local graph data, but it is not a complete
leaderboard row. Missing-skill behavior is also not perfectly uniform across
the current views: the heatmap treats absent skills as zero for its fixed skill
set, while some release/cost aggregates average only the skills that exist.
Partial models should not be compared to complete models.

Coverage is not variance control. The published graph pipeline currently keeps
one newest valid result per model and skill; it does not average repeated trials,
compute confidence intervals, or show error bars. The site describes the skill
values as “Best of 1” and warns that they have a wide error margin. Repeating a
run does not automatically produce a mean or variance estimate—the extractor's
single-result merge behavior still selects one result per skill.

For a credible Pi comparison, distinguish the following:

- Run the complete 16-skill 30-minute set to obtain a full skills graph row.
- Run the gold tasks as well when preparing the complete 40-task leaderboard
  submission.
- Treat that row as a best-of-one exploratory result, not a stable estimate of
  model ability.
- For image, adapter, or model A/B testing, repeat each task 3–5 times and
  report median, spread, exception rate, setup time, and trajectory data
  separately from the public best-of-one graph output.

That means roughly 48–80 skill trials for one horizon, or 120–200 trials for
the full 40-task suite, when the goal is to estimate run-to-run variance rather
than merely populate the graph.

The website is a static GitHub Pages site. Once the generated result files are
committed and the normal Pages publication runs, the graphs update from those
files; there is no separate graph database or server-side aggregation step.

### Smoke-run comparison

These are one-trial diagnostics, not statistically stable model rankings. The
token costs use DeepSeek V4 Flash 0731's OpenRouter rates: $0.08/M input,
$0.016/M cache-read, and $0.18/M output. `input` below excludes cache-read
tokens; the Harbor `n_input_tokens` field is cache-inclusive.

| Run | Image | XP | Level | Peak XP/min | Tracker samples | API turns | Estimated cost | Exceptions |
|---|---|---:|---:|---:|---:|---:|---:|---|
| [full baseline](jobs/2026-08-12__16-04-04/) | full | 109,250 | 67 | 371 | 22 | 28 | $0.0244 | none |
| [lean baseline](jobs/2026-08-12__16-24-11/) | Pi | 58,750 | 58 | 225 | 26 | 23 | $0.0268 | none |
| [short validation](jobs/pi-woodcutting-xp-5m-deepseek-deepseek-v4-flash-0731-20260812-234035/) | Pi | 625 | 7 | 13 | 8 | 3 | $0.0028 | intentional timeout |
| [latest full smoke](jobs/pi-woodcutting-xp-5m-deepseek-deepseek-v4-flash-0731-20260812-234331/) | Pi | 11,250 | 35 | 138 | 24 | 18 | $0.0151 | none |
| [full-image replication](jobs/pi-woodcutting-xp-5m-deepseek-full-arm64-20260812-001/) | full | 625 | 7 | 12 | 28 | 14 | $0.0098 | agent timeout |

The latest run passed the runtime audits: the exact task instruction was in
Pi's user prompt, `thinking=high` was present in both Harbor config and the Pi
command, the bridge system-prompt hook ran, 49,335 API-documentation
characters were loaded, and all five bridge tools were registered. It also
completed 22 tool calls and reached the verifier with no trial exception.
The first run's pre-fix heartbeat briefly labeled the trial
`provider-failing`; replaying its 194 persisted Pi events with the corrected
classifier finds zero provider-error events. The warning was caused by normal
MCP timeout text and tool arguments, not an OpenRouter failure.

Prompt delivery and prompt compliance are separate checks. The task prompt was
delivered exactly, but this run's first model tool was a harmless `bash`
inspection followed by state inspection rather than the instruction's exact
first `execute_code` action (`await bot.skipTutorial()`). That is model
behavior after prompt delivery, not evidence that Harbor or the adapter dropped
the task prompt. The transcript also contains 3,916 reasoning tokens, which is
consistent with `thinking=high` being active at the Pi/OpenRouter layer.

The score gap is expected to be noisy for a single five-minute run: the agent
chose a different strategy and spent time recovering from an early MCP script
attempt. Run several trials per image before attributing the gap to the lean
image itself.

### Interpreting the strategy variance

The three completed runs show a wide spread even though they used the same
DeepSeek model, task, five-minute horizon, and native arm64 host:

- The full-image baseline reached 109,250 XP and a 371 XP/min peak.
- The earlier lean-image run reached 58,750 XP and 225 XP/min.
- The latest lean-image run reached 11,250 XP and 138 XP/min.

That is a 2.7× spread between the highest and latest peak rates. The transcript
shows why the latest run should not be read as a clean image regression. It
spent roughly the first half of the horizon exploring, started a background
training script, encountered a blocking dialog/control-connection conflict,
then switched to a direct `execute_code` chopping loop. Its peak arrived late,
at 4m15s–4m30s, after the recovery. Because Runebench scores the best aligned
15-second window, late recovery can leave the final XP and overall trajectory
far behind even when the recovered loop is productive.

Several sources of variance are therefore mixed together:

- strategy selection: immediate chopping versus researching, moving, upgrading,
  or switching skills;
- setup overhead: tutorial/dialog handling, inventory preparation, and travel;
- control topology: background scripts and `execute_code` must not compete for
  the same bot connection;
- recovery behavior: syntax errors, MCP timeouts, dead scripts, or a model's
  decision to abandon a failing approach;
- score-window alignment: a productive burst that lands late may produce a
  respectable peak but little final XP;
- environment/image effects: browser or service startup differences may exist,
  but these runs do not isolate them from the strategy differences above.

The result is best understood as end-to-end agent behavior, not a deterministic
benchmark of the Docker image or Pi adapter. Prompt delivery is also not the
same as prompt obedience: the latest run received the exact task instruction,
but did not perform the requested first `skipTutorial()` action. That belongs in
the trajectory audit rather than being misdiagnosed as missing prompt context.

For a meaningful image or adapter A/B comparison, keep the model, prompt,
horizon, task checksum, concurrency, and host fixed; run at least 3–5 trials per
variant; and report median plus spread for peak XP/min, final XP, setup time,
tool count, and exception rate. Preserve each trajectory and classify
infrastructure failures separately from strategy failures. A single successful
trial is useful for integration validation, but not for claiming that one image
or harness is intrinsically better.

The full-image replication strengthens that caution: it produced the same
625-XP floor as the short validation run (12–13 XP/min) and timed out while the
bridge, tracker, and model prompt audits remained healthy. In other words, the
full image did not automatically recover the 109,250-XP baseline. The current
evidence is consistent with large strategy/recovery variance, while still being
too few trials to rule out a smaller image effect.

## Live-run introspection

Harbor's live terminal display reports the coarse trial phase:

```text
starting environment → running agent → running verifier
```

That is not a complete health signal. A trial can remain in `running agent`
while Pi is actively working, while a background script is still training, or
while a process is alive but making no progress.

The useful live signals are:

| Signal | Healthy interpretation | Warning sign |
|---|---|---|
| Harbor phase | Trial is in the expected phase | `running agent` with no other activity for a long interval |
| `agent/pi.txt` size/mtime | Pi is producing model/tool events | No new event for 60–120 seconds |
| `skill_tracking.json` | Game services and tracker are alive | No new sample for 30+ seconds |
| `execute_code` results | MCP/game control is responding | Repeated tool errors or no tool completions |
| Engine/gateway/browser processes | Game runtime is alive | Missing or dead process |
| Child `bun` scripts | Deliberate background strategy | Orphaned script after the agent has stopped |
| Provider errors | Requests are completing | Explicit provider error events such as 429/5xx, or no new Pi output |

### Manual monitoring

The normal `make pi` command now runs [`scripts/run-pi-live.ts`](scripts/run-pi-live.ts).
It wraps the same `uv run ... harbor run` invocation used by `pi-direct`, adds
an explicit `--job-name`, and returns Harbor's exit code. Its monitor is
diagnostic and read-only: it never restarts a process or cancels a trial.
Set `PI_LIVE_INTERVAL_MS` to change the default five-second heartbeat.

The Harbor job name and trial name appear in the `make pi` output. Set them in a
second terminal:

```bash
JOB=jobs/<job-timestamp>/woodcutting-xp-5m__<trial-id>
CONTAINER=<docker-container-name>
```

Follow Pi's JSONL transcript on the host:

```bash
tail -f "$JOB/agent/pi.txt"
```

Summarize the most recent agent/tool events:

```bash
tail -200 "$JOB/agent/pi.txt" \
  | jq -r '
      if .type == "message_end" then
        "message " + (.message.role // "?") + " stop=" + (.message.stopReason // "")
      elif .type == "tool_execution_start" then
        "tool start " + (.toolName // "?")
      elif .type == "tool_execution_end" then
        "tool end " + (.toolName // "?")
      else empty end'
```

The minimal Pi image does not include `jq`, so inspect the live game tracker
with Bun:

```bash
docker exec "$CONTAINER" bun -e '
const x = await Bun.file("/logs/tracking/skill_tracking.json").json();
const s = x.samples.at(-1);
console.log(JSON.stringify({
  samples: x.samples.length,
  elapsedMs: s?.elapsedMs,
  woodcutting: s?.skills?.Woodcutting,
  totalLevel: s?.totalLevel,
}));'
```

Inspect the game processes and any agent-spawned scripts:

```bash
docker top "$CONTAINER"
```

A practical five-second polling loop can combine the tracker and container
check. It is intentionally read-only:

```bash
while true; do
  date
  docker ps --filter "name=$CONTAINER" --format '{{.Status}} {{.Names}}'
  docker exec "$CONTAINER" bun -e '
    const x = await Bun.file("/logs/tracking/skill_tracking.json").json();
    const s = x.samples.at(-1);
    console.log(`samples=${x.samples.length} elapsed=${s?.elapsedMs ?? 0}ms woodcutting=${s?.skills?.Woodcutting?.xp ?? 0}xp`);'
  tail -1 "$JOB/agent/pi.txt" | cut -c1-240
  sleep 5
done
```

### Current limitations

- Harbor does not itself aggregate Pi events, tracker freshness, process
  health, and provider errors into one dashboard; the local wrapper does this
  in the terminal for Docker runs.
- The host does not receive a durable structured heartbeat artifact during the
  run. The useful tracker and Pi logs live inside the task/container and are
  collected into job artifacts.
- A live agent process can be waiting on a model response, retrying a provider,
  or spinning in a recovery loop while Harbor still reports `running agent`.
- The provider warning is deliberately conservative: MCP/tool-result text such
  as `MCP error ... timed out`, and a normal tool argument named `timeout`, are
  not counted as OpenRouter failures. The monitor counts explicit Pi/provider
  error events and assistant turns with `stopReason=error`.
- Background scripts can continue training after the model stops reasoning;
  this can be legitimate progress or an orphaned process and needs inspection.
- The Pi-local image sets `RECORD_VIDEO=0` and omits FFmpeg/PulseAudio, so new
  Pi runs do not produce the normal MP4 video artifacts. Scores and tracker
  data still work.
- Harbor's task timeout is the final safety boundary. A five-minute task can
  remain alive for its configured buffer while Pi or child scripts wind down.

### Future monitoring work

The local Docker monitor covers the first useful version of this functionality.
Remaining work is mostly scope expansion:

- Add a durable heartbeat JSON artifact if Harbor exposes a supported live
  artifact/event stream.
- Add remote-environment support. For Modal or another remote backend, this
  wrapper can monitor Harbor output and downloaded job files, but it cannot
  inspect the remote tracker or process table in real time.
- Consider an opt-in dashboard or notification sink. Automatic restart or
  cancellation should remain a separate explicit policy because terminating a
  background training script can change the benchmark result.

## Swarming and concurrency boundaries

The current integration has a clean boundary between Harbor-level swarming and
within-trial agent parallelism. Harbor expects one agent process per trial, but
it can run many isolated trials concurrently.

### Safe boundary: independent Harbor trials

Harbor's top-level `n_concurrent_trials` limit controls how many trials run at
once. Each trial receives its own environment, browser/game process,
`rs-agent` MCP server, Pi process/session, log directory, and benchmark bot.
Running two or more DeepSeek trials is therefore the natural safe swarm for
benchmarking: the agents do not share an `agent` bot or MCP control connection.

The current local wrapper intentionally passes `-n 1` in
`scripts/run-pi-live.ts`, so `make pi` launches one trial today. Harbor also
has an optional per-agent `n_concurrent` cap and `concurrency_group` pool, but
those limits apply to separate trial agent phases; they do not turn one Pi
session into a multi-agent runtime. Swarming independent trials would increase
OpenRouter usage and may expose rate limits, but it does not require an
Harbor or rs-sdk modification.

### Unsafe boundary: multiple agents controlling one trial bot

Inside a trial, Harbor's native Pi runner launches one Pi process and one
session. The Runebench extension registers `execute_code` with sequential
execution, so tool calls from that session are serialized. Pi can spawn child
processes through shell commands, but those processes are outside Harbor's
agent lifecycle, token accounting, trajectory capture, and live health model.

More importantly, the game control path is currently single-controller. The
bot launcher creates the `agent` bot's SDK connection with
`connectionMode: "control"` in [`docker/launch-bot.ts`](docker/launch-bot.ts).
An independently started SDK/script using another control connection can take
over the bot or disconnect the MCP server's SDK. This is why a background
training script and interactive `execute_code` calls must not control the same
bot concurrently.

Consequently, multiple Pi processes sharing the same trial container and
`bot_name: "agent"` are not a supported swarm. Parallel reads may appear to
work temporarily, but concurrent writes, reconnects, or shutdowns can race and
make the trajectory nondeterministic or strand the MCP connection.

### What a future swarm design would mean

There are two different future designs, with different costs:

1. **Trial fan-out:** expose an opt-in Harbor concurrency setting for the local
   wrapper and let Harbor create two or more isolated Pi trials. This is the
   preferred design for independent strategy sampling and image/model A/B
   testing.
2. **Cooperative same-game agents:** add a controller/lease layer to rs-sdk or
   `rs-agent`, define ownership and cancellation semantics, and give workers
   distinct capabilities or bots. The parent agent would also need to collect
   child trajectories, token usage, errors, and costs. This is a substantially
   different runtime, not just starting extra `pi` commands from Bash.

For now, treat “swarm” as Harbor trial fan-out. Treat same-bot multi-agent
control as unsupported until the SDK exposes an explicit multi-controller or
brokered command model.

## Lean-image validation

The Pi-specific image was compared against the earlier full Runebench image
using the same model, task, adapter, and native arm64 host.

| Metric | Full image | Pi image |
|---|---:|---:|
| App image size | 949 MB | 735 MB |
| Base image size | 608 MB | 490 MB |
| Final Woodcutting level | 67 | 58 |
| Final XP | 109,250 | 58,750 |
| Peak XP/min | 371 | 225 |
| Agent exceptions | 0 | 0 |
| Skill samples | 22 | 26 |
| `execute_code` calls | 9 | 8 |
| Bash calls | 15 | 13 |
| Write calls | 3 | 2 |

The lean image is approximately 22% smaller at the app layer and 20% smaller
at the base layer. It still runs the browser game, MCP server, tracker, and
verifier successfully.

The score difference is large enough that it should not be attributed to image
slimming from one run alone. The two runs are stochastic and the agent chose
different strategies. Before treating the lean image as behaviorally
equivalent, run at least 2–4 smoke trials per image or compare the full
leaderboard sweep. The removed audio/video stack should not affect game state,
but future A/B checks should continue to verify browser startup, MCP calls,
tracker freshness, and score—not just image size.

## How the adapter works

The adapter has four layers:

```text
Harbor task config
  └─ self.mcp_servers: rs-agent stdio command
       ↓
RunebenchPi (Python Harbor adapter)
  ├─ calls Harbor's native Pi setup/install
  ├─ writes ~/.pi/agent/models.json
  └─ writes ~/.pi/agent/extensions/runebench-rs-agent.ts
       ↓
Pi extension (TypeScript)
  ├─ starts an MCP StdioClientTransport
  ├─ registers Pi custom tools
  └─ forwards tools/call to /app/mcp/server.ts
       ↓
rs-agent MCP server → game SDK → RuneScape container
```

### Harbor's native Pi agent remains responsible for the lifecycle

[`RunebenchPi`](agents/pi_adapter.py) subclasses
`harbor.agents.installed.pi.Pi`. Its `setup()` first calls Harbor's native
implementation, which:

- installs the current `@earendil-works/pi-coding-agent` package in the task
  container
- performs the Pi version probe
- preserves Harbor's auth/environment handling

The adapter does not reimplement Pi process execution, Harbor timeouts, log
collection, session output, or token accounting.

### The model config prevents unsafe fallback metadata

Pi can accept an arbitrary `provider/model` string, but an uncataloged model
would otherwise inherit metadata from an unrelated provider model. The adapter
writes a task-local `~/.pi/agent/models.json` containing:

- provider and API endpoint
- exact model ID
- context window
- maximum output tokens
- reasoning compatibility
- per-million-token cost fields

DeepSeek V4 Flash 0731 is explicitly declared with its current context,
output, and cost limits. Unknown override models still receive conservative
fallback metadata, so `PI_MODEL=...` remains usable for experiments.

### The extension supplies what Pi intentionally does not

Pi does not include built-in MCP, so the uploaded extension registers:

- `execute_code`
- `list_bots`
- `disconnect_bot`
- `rs_agent_list_resources`
- `rs_agent_read_resource`

The extension creates one persistent MCP client using the SDK already installed
in the Runebench image. It forwards tool calls to the task's `rs-agent` stdio
server and closes the client during session shutdown.

It also loads the SDK API resource into Pi's system prompt, bounded to keep the
initial context manageable. The explicit resource tools remain available if
the startup documentation load fails.

### Why Harbor did not need modification

This works cleanly because Harbor already exposes the necessary extension
points:

1. `--agent` accepts a local `module.path:Class` import.
2. `BaseInstalledAgent` passes task MCP definitions in `self.mcp_servers`.
3. Harbor's native Pi class already owns installation and execution.
4. Pi's current SDK supports extensions and custom tools.
5. The benchmark image already contains `@modelcontextprotocol/sdk`.

Runebench only translates Harbor's generic MCP configuration into Pi's native
extension API. No Harbor fork, upstream patch, or separate Harbor Pi SDK is
required.

## How the graphs are generated

The published graphs are a second, separate pipeline:

```text
jobs/<job>/<trial>/
  ↓
extract-skill-results.ts / extract-gold-results.ts
  ↓
results/skills-30m/_data.js
results/skills-30m/<model>.json
results/gold/_data.js
  ↓
index.html loads static data
  ↓
React + Chart.js render the graphs in the browser
```

The skill extractor:

1. Groups raw Harbor jobs by model and skill.
2. Chooses the newest valid result, avoiding replacement by failed runs with
   no tracking samples.
3. Reads verifier rewards, tracker samples, token usage, trajectories, and
   video metadata.
4. Computes peak XP/min using the benchmark's `/200` normalization.
5. Writes a slim `_data.js` summary and full per-model JSON payloads.

The main site loads `results/skills-30m/_data.js` up front. When a user opens a
trajectory, [`app/model-data.js`](app/model-data.js) lazy-loads the matching
per-model JSON file.

The heatmap displays each model's `peakXpRate` per skill. Overall comparison
uses a logarithmic mean, `mean(log(1 + rate))`, so very high-rate skills do not
completely dominate the model ranking. The cost scatter compares average API
cost per skill run against that logarithmic performance score. Display names,
colors, ordering, release dates, and icons are defined in
[`views/shared-constants.js`](views/shared-constants.js).

## Cost and provider notes

Run artifacts carry input, cache, output, and estimated cost metadata. The
shared pricing table is [`shared/pricing.ts`](shared/pricing.ts).

For the latest completed DeepSeek V4 Flash 0731 smoke run, the Pi transcript
reported:

- 669k cache-inclusive input tokens
- 621k cache-read tokens
- 6.9k output tokens
- approximately $0.0151 at current OpenRouter catalog rates

The earlier jobs were created before the adapter's explicit OpenRouter model
metadata fix, so Harbor recorded null cost fields for them. Their estimates in
the comparison table are reconstructed from the per-turn token counts and the
same current rates.

The cost is small, but the model is paid and not rate-limit-free. Free
endpoints may cost `$0` while still stopping early because of shared upstream
rate limits. A provider 429 is an external serving limitation, not a Harbor or
Pi adapter failure.

## Troubleshooting

### Pi says the model is unknown

That warning is normally harmless if the request reaches OpenRouter. The
adapter writes a custom `models.json` entry at setup time. Check the agent log
for the actual provider error before changing the adapter.

### The container cannot resolve the published image

The published Runebench image is amd64-only. Use `make pi`, which builds the
native arm64 base and app images. Do not set `DOCKER_DEFAULT_PLATFORM=linux/amd64`
for the local workflow.

### The model rate-limits

Use `-n 1`, wait, retry, or configure a provider key/routing policy through
OpenRouter. Increasing local Harbor concurrency increases API pressure and can
make free/shared endpoints fail sooner.

### Harbor rejects rich reward JSON

Harbor 0.21 expects numeric values in `verifier/reward.json`. Runebench writes
the numeric summary there and preserves tracking-rich metadata in
`verifier/runebench-result.json`; the extractors understand both the new and
historical layouts.
