/**
 * Run one local pi-agent-core-rs/Harbor job and report useful health signals while it runs.
 *
 * This deliberately wraps `harbor run` instead of reimplementing any Harbor
 * lifecycle.  The monitor only reads job files and Docker state; Harbor still
 * owns setup, timeouts, verification, artifact transfer, and the exit code.
 *
 * Environment is supplied by Makefile (or can be supplied directly):
 *   AGENT_CORE_TASK, AGENT_CORE_MODEL, AGENT_CORE_JOBS_DIR, HARBOR_PROJECT
 *   AGENT_CORE_JOB_NAME, AGENT_CORE_AGENT_TIMEOUT_MULTIPLIER,
 *   AGENT_CORE_VERIFIER_TIMEOUT_MULTIPLIER
 */

import {
  existsSync,
  readdirSync,
  readFileSync,
  statSync,
} from "node:fs";
import { basename, join, resolve } from "node:path";

type JsonRecord = Record<string, any>;

type PiSnapshot = {
  exists: boolean;
  mtimeMs: number | null;
  logAgeSec: number | null;
  lastEvent: string | null;
  lastTool: string | null;
  assistantMessages: number;
  toolStarts: number;
  toolEnds: number;
  providerErrors: number;
  userPrompt: string | null;
  lastStopReason: string | null;
};

type TrackerSnapshot = {
  samples: number;
  elapsedMs: number | null;
  timestamp: string | null;
  xp: number | null;
  level: number | null;
  gold: number | null;
  error?: string;
};

type ContainerSnapshot = {
  id: string;
  name: string;
  status: string;
  processes: string;
  tracker: TrackerSnapshot | null;
  bridge: JsonRecord | null;
};

type TrialSnapshot = {
  name: string;
  path: string;
  phase: string;
  pi: PiSnapshot;
  container: ContainerSnapshot | null;
  agentCoreConfigured: boolean;
  promptAudit: "ok" | "missing" | "pending" | "not-applicable";
  result: JsonRecord | null;
  warnings: string[];
  state: string;
};

const root = process.cwd();
const task = process.env.AGENT_CORE_TASK || "tasks/woodcutting-xp-5m";
const model = process.env.AGENT_CORE_MODEL || "openrouter/deepseek/deepseek-v4-flash-0731";
const jobsDir = resolve(root, process.env.AGENT_CORE_JOBS_DIR || "jobs");
const harborProject = resolve(
  root,
  process.env.HARBOR_PROJECT || `${process.env.HOME || ""}/d/harbor`,
);
const taskPath = resolve(root, task);
const taskSlug = basename(taskPath);
const intervalMs = Number(process.env.AGENT_CORE_LIVE_INTERVAL_MS || 5000);
const stalePiSec = Number(process.env.AGENT_CORE_LIVE_LOG_STALE_SEC || 90);
const staleTrackerSec = Number(process.env.AGENT_CORE_LIVE_TRACKER_STALE_SEC || 35);

function slug(value: string): string {
  return value
    .replace(/^openrouter\//, "")
    .replace(/[^a-zA-Z0-9]+/g, "-")
    .replace(/^-|-$/g, "")
    .toLowerCase()
    .slice(0, 100);
}

function defaultJobName(): string {
  const stamp = new Date()
    .toISOString()
    .replace(/[-:]/g, "")
    .replace(/\.\d{3}Z$/, "")
    .replace("T", "-");
  return `agent-core-${slug(taskSlug)}-${slug(model)}-${stamp}`;
}

const jobName = process.env.AGENT_CORE_JOB_NAME || defaultJobName();
const jobDir = resolve(jobsDir, jobName);

function decode(value: Uint8Array | string | null | undefined): string {
  if (typeof value === "string") return value;
  if (!value) return "";
  return new TextDecoder().decode(value);
}

function capture(command: string[]): { code: number; stdout: string; stderr: string } {
  try {
    const result = Bun.spawnSync(command, { stdout: "pipe", stderr: "pipe" });
    return {
      code: result.exitCode,
      stdout: decode(result.stdout),
      stderr: decode(result.stderr),
    };
  } catch (error) {
    return { code: 127, stdout: "", stderr: String(error) };
  }
}

function readJson(path: string): JsonRecord | null {
  try {
    return JSON.parse(readFileSync(path, "utf8")) as JsonRecord;
  } catch {
    return null;
  }
}

function readText(path: string): string | null {
  try {
    return readFileSync(path, "utf8");
  } catch {
    return null;
  }
}

function fileAgeSec(path: string): number | null {
  try {
    return Math.max(0, (Date.now() - statSync(path).mtimeMs) / 1000);
  } catch {
    return null;
  }
}

function eventText(event: JsonRecord): string {
  const content = event.message?.content;
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .map((part: any) => (part?.type === "text" ? part.text || "" : ""))
    .join("\n");
}

const providerErrorPattern =
  /(?:429|rate.?limit|5\d\d|api error|provider error|connection (?:closed|reset|refused)|timeout|timed out|authentication|unauthorized|context window|token limit)/i;

/**
 * Pi records tool arguments and tool results in the same JSONL stream as model
 * messages. Those payloads commonly contain words such as `timeout` and
 * `error`; neither is evidence that OpenRouter failed. Only count an explicit
 * provider error field or an assistant turn whose stop reason is `error`.
 */
export function isProviderErrorEvent(event: JsonRecord): boolean {
  const message = event.message;
  if (event.type === "message_end" && message?.role === "assistant") {
    const stopReason = String(message.stopReason || message.rawStopReason || "").toLowerCase();
    if (stopReason === "error") return true;

    return [message.error, message.errorMessage, event.error]
      .filter((value) => value !== undefined && value !== null)
      .some((value) => providerErrorPattern.test(JSON.stringify(value)));
  }

  if (event.type === "error" || event.type === "provider_error") {
    return providerErrorPattern.test(JSON.stringify(event.error || event.message || event));
  }

  return false;
}

function readPiSnapshot(trialPath: string): PiSnapshot {
  const path = join(trialPath, "agent", "pi-agent-core.jsonl");
  const text = readText(path);
  if (text === null) {
    return {
      exists: false,
      mtimeMs: null,
      logAgeSec: null,
      lastEvent: null,
      lastTool: null,
      assistantMessages: 0,
      toolStarts: 0,
      toolEnds: 0,
      providerErrors: 0,
      userPrompt: null,
      lastStopReason: null,
    };
  }

  let assistantMessages = 0;
  let toolStarts = 0;
  let toolEnds = 0;
  let providerErrors = 0;
  let lastEvent: string | null = null;
  let lastTool: string | null = null;
  let lastStopReason: string | null = null;
  let userPrompt: string | null = null;

  for (const line of text.split("\n")) {
    if (!line.trim()) continue;
    let event: JsonRecord;
    try {
      event = JSON.parse(line) as JsonRecord;
    } catch {
      // A partially-written final JSONL line is normal after interruption and
      // cannot be classified reliably as a provider failure.
      continue;
    }

    const type = String(event.type || "unknown");
    lastEvent = type;

    if (type === "message_end") {
      const role = event.message?.role;
      if (role === "assistant") {
        assistantMessages++;
        lastStopReason = event.message?.stopReason || null;
      }
      if (role === "user" && userPrompt === null) userPrompt = eventText(event);
    } else if (type === "message_start" && event.message?.role === "user") {
      if (userPrompt === null) userPrompt = eventText(event);
    } else if (type === "tool_execution_start") {
      toolStarts++;
      lastTool = event.toolName || null;
    } else if (type === "tool_execution_end") {
      toolEnds++;
      lastTool = event.toolName || lastTool;
    }

    if (isProviderErrorEvent(event)) providerErrors++;
  }

  let mtimeMs: number | null = null;
  try {
    mtimeMs = statSync(path).mtimeMs;
  } catch {}

  return {
    exists: true,
    mtimeMs,
    logAgeSec: fileAgeSec(path),
    lastEvent,
    lastTool,
    assistantMessages,
    toolStarts,
    toolEnds,
    providerErrors,
    userPrompt,
    lastStopReason,
  };
}

function taskPrompt(): string | null {
  if (!existsSync(taskPath) || !statSync(taskPath).isDirectory()) return null;
  return readText(join(taskPath, "instruction.md"));
}

function auditPrompt(pi: PiSnapshot, expectedPrompt: string | null): TrialSnapshot["promptAudit"] {
  if (!pi.exists || pi.userPrompt === null) return "pending";
  if (expectedPrompt === null) {
    const markers = [
      "rs-agent",
      "execute_code",
      "Start simple",
      "RULES:",
    ];
    return markers.every((marker) => pi.userPrompt!.includes(marker))
      ? "ok"
      : "missing";
  }
  return pi.userPrompt.includes(expectedPrompt.trim()) ? "ok" : "missing";
}

function parsePsLines(output: string): Array<{ id: string; name: string; status: string; project: string }> {
  return output
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const [id = "", name = "", status = "", project = ""] = line.split("\t");
      return { id, name, status, project };
    })
    .filter((row) => row.id && row.name);
}

function findContainer(trialName: string): { id: string; name: string; status: string } | null {
  const result = capture([
    "docker",
    "ps",
    "--format",
    '{{.ID}}\t{{.Names}}\t{{.Status}}\t{{.Label "com.docker.compose.project"}}',
  ]);
  const trialId = trialName.split("__").at(-1)?.toLowerCase() || "";
  const trialNeedles = [trialName.toLowerCase(), trialId, taskSlug.toLowerCase()].filter(Boolean);
  const candidates = parsePsLines(result.stdout).filter((row) => {
    const haystack = `${row.name}\t${row.project}`.toLowerCase();
    return trialNeedles.some((needle) => haystack.includes(needle));
  });
  const candidate = candidates.find((row) => row.status.toLowerCase().includes("up")) || candidates[0];
  return candidate ? { id: candidate.id, name: candidate.name, status: candidate.status } : null;
}

function readTracker(containerId: string): TrackerSnapshot | null {
  const script = [
    'const p="/logs/tracking/skill_tracking.json";',
    'try {',
    'const x=JSON.parse(await Bun.file(p).text());',
    'const s=x.samples?.at(-1);',
    'const w=s?.skills?.Woodcutting;',
    'console.log(JSON.stringify({samples:x.samples?.length??0,elapsedMs:s?.elapsedMs??null,timestamp:s?.timestamp??null,xp:w?.xp??null,level:w?.level??null,gold:s?.gold??null}));',
    '} catch (e) { console.log(JSON.stringify({error:String(e)})); }',
  ].join("");
  const result = capture(["docker", "exec", containerId, "bun", "-e", script]);
  const line = result.stdout.trim().split("\n").at(-1);
  if (!line) return null;
  try {
    const parsed = JSON.parse(line) as TrackerSnapshot;
    if (parsed.error) {
      return {
        samples: 0,
        elapsedMs: null,
        timestamp: null,
        xp: null,
        level: null,
        gold: null,
        error: parsed.error,
      };
    }
    return parsed;
  } catch {
    return {
      samples: 0,
      elapsedMs: null,
      timestamp: null,
      xp: null,
      level: null,
      gold: null,
      error: line.slice(0, 160),
    };
  }
}

function readBridgeMarker(containerId: string): JsonRecord | null {
  const result = capture(["docker", "exec", containerId, "cat", "/logs/agent/runebench-pi-agent-core.json"]);
  if (result.code !== 0) return null;
  try {
    return JSON.parse(result.stdout) as JsonRecord;
  } catch {
    return null;
  }
}

function readContainer(trialName: string): ContainerSnapshot | null {
  const found = findContainer(trialName);
  if (!found) return null;

  const top = capture(["docker", "top", found.id]).stdout;
  return {
    ...found,
    processes: top,
    tracker: readTracker(found.id),
    bridge: readBridgeMarker(found.id),
  };
}

function processHealth(processes: string): Record<string, boolean> {
  const text = processes.toLowerCase();
  return {
    engine: /src\/app\.ts|engine/.test(text),
    gateway: /gateway\.ts|gateway/.test(text),
    browser: /chromium|chrome/.test(text),
    mcp: /mcp[\/]server\.ts|mcp\/server|server\.ts/.test(text),
    tracker: /skill_tracker/.test(text),
  };
}

function configuredAgentCore(trialPath: string): boolean {
  const config = readJson(join(trialPath, "config.json"));
  const log = readText(join(trialPath, "trial.log")) || "";
  return (
    JSON.stringify(config || {}).includes("pi_agent_core_adapter:RunebenchPiAgentCore") ||
    log.includes("pi_agent_core_adapter:RunebenchPiAgentCore")
  );
}

function resultFor(trialPath: string): JsonRecord | null {
  return readJson(join(trialPath, "result.json"));
}

function phaseFor(trialPath: string, pi: PiSnapshot): string {
  if (existsSync(join(trialPath, "verifier", "test-stdout.txt"))) return "verifier";
  if (pi.exists) return "agent";
  if (existsSync(join(trialPath, "config.json"))) return "setup";
  return "pending";
}

function horizonSeconds(): number | null {
  const explicit = Number(process.env.AGENT_CORE_HORIZON_SECONDS || "");
  if (Number.isFinite(explicit) && explicit > 0) return explicit;
  const match = taskSlug.match(/-(\d+)m(?:$|[-_])/i);
  return match ? Number(match[1]) * 60 : null;
}

function classify(
  pi: PiSnapshot,
  container: ContainerSnapshot | null,
  phase: string,
  ageSec: number,
): { state: string; warnings: string[] } {
  const warnings: string[] = [];
  const tracker = container?.tracker;
  const health = container ? processHealth(container.processes) : null;

  if (pi.providerErrors >= 3) warnings.push(`provider errors=${pi.providerErrors}`);
  if (pi.exists && pi.logAgeSec !== null && pi.logAgeSec > stalePiSec && phase === "agent") {
    warnings.push(`agent-core log stale ${Math.round(pi.logAgeSec)}s`);
  }
  if (tracker?.timestamp) {
    const trackerAge = Math.max(0, (Date.now() - Date.parse(tracker.timestamp)) / 1000);
    if (trackerAge > staleTrackerSec && phase === "agent") {
      warnings.push(`tracker stale ${Math.round(trackerAge)}s`);
    }
  }
  if (health && phase === "agent" && ageSec > 60) {
    for (const name of ["engine", "gateway", "browser", "mcp", "tracker"] as const) {
      if (!health[name]) warnings.push(`missing ${name}`);
    }
  }
  if (container?.bridge && container.bridge.docsLoaded === false && phase === "agent") {
    warnings.push("MCP API docs were not loaded into the agent-core system prompt");
  }

  const horizon = horizonSeconds();
  if (horizon && tracker?.elapsedMs && tracker.elapsedMs / 1000 > horizon + 60) {
    const childScripts = container?.processes
      .split("\n")
      .filter((line) => /bun|node/i.test(line) && !/engine|gateway|launch-bot|skill_tracker|mcp\/server|ensure-services/i.test(line));
    if (childScripts && childScripts.length > 0) {
      warnings.push(`child scripts beyond ${Math.round(horizon / 60)}m horizon`);
    }
  }

  if (pi.toolStarts > pi.toolEnds && pi.logAgeSec !== null && pi.logAgeSec > 120 && phase === "agent") {
    warnings.push(`tool ${pi.lastTool || "call"} has no completion`);
  }

  if (pi.providerErrors >= 3) return { state: "provider-failing", warnings };
  if (warnings.some((warning) => warning.startsWith("tracker stale") || warning.startsWith("missing "))) {
    return { state: "game-stalled", warnings };
  }
  if (warnings.some((warning) => warning.startsWith("agent-core log stale"))) {
    return { state: "model-waiting", warnings };
  }
  if (warnings.some((warning) => warning.startsWith("tool "))) {
    return { state: "likely-spinning", warnings };
  }
  if (phase === "verifier") return { state: "verifying", warnings };
  if (phase === "agent" && (tracker || pi.toolEnds > 0)) return { state: "healthy", warnings };
  if (ageSec < 60) return { state: "starting", warnings };
  return { state: "waiting", warnings };
}

function listTrials(): string[] {
  if (!existsSync(jobDir)) return [];
  return readdirSync(jobDir, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && !entry.name.startsWith("."))
    .map((entry) => entry.name)
    .sort();
}

function snapshotTrial(name: string, expectedPrompt: string | null, startedAt: number): TrialSnapshot {
  const path = join(jobDir, name);
  const pi = readPiSnapshot(path);
  const container = readContainer(name);
  const phase = phaseFor(path, pi);
  const result = resultFor(path);
  const classification = classify(pi, container, phase, (Date.now() - startedAt) / 1000);
  return {
    name,
    path,
    phase,
    pi,
    container,
    agentCoreConfigured: configuredAgentCore(path),
    promptAudit: auditPrompt(pi, expectedPrompt),
    result,
    warnings: classification.warnings,
    state: classification.state,
  };
}

function formatAge(seconds: number | null): string {
  if (seconds === null) return "-";
  if (seconds < 60) return `${Math.round(seconds)}s`;
  return `${Math.floor(seconds / 60)}m${Math.round(seconds % 60)}s`;
}

function trialLine(trial: TrialSnapshot, startedAt: number): string {
  const tracker = trial.container?.tracker;
  const elapsed = tracker?.elapsedMs !== null && tracker?.elapsedMs !== undefined
    ? `${Math.floor(tracker.elapsedMs / 60000)}m${Math.floor((tracker.elapsedMs / 1000) % 60)}s`
    : "-";
  const progress = tracker
    ? `xp=${tracker.xp ?? "-"} lvl=${tracker.level ?? "-"} samples=${tracker.samples} game=${elapsed}`
    : "tracker=-";
  const tools = `${trial.pi.toolEnds}/${trial.pi.toolStarts}`;
  const container = trial.container ? `docker=${trial.container.name}` : "docker=-";
  const bridge = trial.container?.bridge
    ? `bridge=ok${trial.container.bridge.docsLoaded ? "/docs" : "/no-docs"}`
    : "bridge=-";
  return [
    `${trial.name} phase=${trial.phase} state=${trial.state}`,
    progress,
    `pi=${formatAge(trial.pi.logAgeSec)} tools=${tools} last=${trial.pi.lastTool || trial.pi.lastEvent || "-"}`,
    `prompt=${trial.promptAudit} agent-core=${trial.agentCoreConfigured ? "ok" : "pending"}`,
    container,
    bridge,
    `run=${formatAge((Date.now() - startedAt) / 1000)}`,
  ].join(" | ");
}

function emitWarnings(trials: TrialSnapshot[], previous: Set<string>): Set<string> {
  const current = new Set<string>();
  for (const trial of trials) {
    for (const warning of trial.warnings) {
      const key = `${trial.name}:${warning}`;
      current.add(key);
      if (!previous.has(key)) console.error(`[pi-live][warn] ${trial.name}: ${warning}`);
    }
    if (trial.promptAudit === "missing") {
      const key = `${trial.name}:prompt`;
      current.add(key);
      if (!previous.has(key)) console.error(`[pi-live][warn] ${trial.name}: task prompt audit failed`);
    }
  }
  return current;
}

function printHeartbeat(trials: TrialSnapshot[], startedAt: number): void {
  if (trials.length === 0) {
    console.error(`[pi-live] ${formatAge((Date.now() - startedAt) / 1000)} phase=waiting for Harbor trial directory ${jobDir}`);
    return;
  }
  console.error(`[pi-live] ${formatAge((Date.now() - startedAt) / 1000)} job=${jobName} trials=${trials.length}`);
  for (const trial of trials) console.error(`[pi-live]   ${trialLine(trial, startedAt)}`);
}

function resultSummary(): string {
  const result = readJson(join(jobDir, "result.json"));
  if (!result) return "result=not-written";
  const stats = result.stats || {};
  return `completed=${stats.n_completed_trials ?? "-"} errored=${stats.n_errored_trials ?? "-"} cost=$${Number(stats.cost_usd || 0).toFixed(4)}`;
}

function rewardSummary(trialPath: string): string {
  const rich = readJson(join(trialPath, "verifier", "runebench-result.json"));
  if (rich) {
    return `peak=${rich.peakXpRate ?? "-"} xp/min xp=${rich.xp ?? "-"} level=${rich.level ?? "-"}`;
  }
  const reward = readJson(join(trialPath, "verifier", "reward.json"));
  return reward ? `reward=${JSON.stringify(reward)}` : "reward=not-written";
}

function harborArgs(): string[] {
  const args = [
    "run",
    "-p",
    task,
    "-e",
    "docker",
    "-a",
    "pi_agent_core_adapter:RunebenchPiAgentCore",
    "-m",
    model,
    "-o",
    process.env.AGENT_CORE_JOBS_DIR || "jobs",
    "-n",
    "1",
    "-k",
    "1",
    "--job-name",
    jobName,
    "-y",
  ];
  const agentTimeout = process.env.AGENT_CORE_AGENT_TIMEOUT_MULTIPLIER;
  const verifierTimeout = process.env.AGENT_CORE_VERIFIER_TIMEOUT_MULTIPLIER;
  if (agentTimeout) args.push("--agent-timeout-multiplier", agentTimeout);
  if (verifierTimeout) args.push("--verifier-timeout-multiplier", verifierTimeout);
  return args;
}

async function main(): Promise<number> {
  if (process.argv.includes("--help") || process.argv.includes("-h")) {
    console.log("Usage: bun scripts/run-pi-live.ts");
    console.log("Runs Harbor locally with pi-agent-core-rs and prints read-only health heartbeats.");
    return 0;
  }
  if (!existsSync(harborProject)) {
    console.error(`[pi-live] Harbor project does not exist: ${harborProject}`);
    return 2;
  }
  if (existsSync(jobDir)) {
    console.error(`[pi-live] Job directory already exists: ${jobDir}`);
    console.error("Set AGENT_CORE_JOB_NAME to a new name or remove only that completed job directory.");
    return 2;
  }

  const expectedPrompt = taskPrompt();
  if (!process.env.OPENROUTER_API_KEY) {
    console.error("[pi-live] OPENROUTER_API_KEY is not present; use vault OPENROUTER_API_KEY -- ...");
    return 2;
  }

  const args = harborArgs();
  console.error(`[pi-live] starting: uv run --project ${harborProject} harbor ${args.join(" ")}`);
  console.error(`[pi-live] job directory: ${jobDir}`);
  console.error("[pi-live] audits: task prompt, agent-core log freshness, MCP bridge, tracker, processes");

  const startedAt = Date.now();
  const child = Bun.spawn(["uv", "run", "--project", harborProject, "harbor", ...args], {
    cwd: root,
    env: process.env,
    stdout: "inherit",
    stderr: "inherit",
  });

  let warnings = new Set<string>();
  let lastHeartbeat = 0;
  let trials: TrialSnapshot[] = [];
  const prompt = expectedPrompt;

  const exitPromise = child.exited;
  let finished = false;
  let exitCode = 1;
  exitPromise.then((code) => {
    finished = true;
    exitCode = code;
  });

  const onSignal = (signal: NodeJS.Signals) => {
    console.error(`[pi-live] forwarding ${signal} to Harbor (read-only monitor stopping)`);
    child.kill(signal);
  };
  process.on("SIGINT", onSignal);
  process.on("SIGTERM", onSignal);

  while (!finished) {
    trials = listTrials().map((name) => snapshotTrial(name, prompt, startedAt));
    const now = Date.now();
    if (now - lastHeartbeat >= intervalMs || trials.some((trial) => trial.promptAudit === "missing")) {
      printHeartbeat(trials, startedAt);
      warnings = emitWarnings(trials, warnings);
      lastHeartbeat = now;
    }
    await Promise.race([exitPromise, Bun.sleep(intervalMs)]);
  }

  await exitPromise;
  trials = listTrials().map((name) => snapshotTrial(name, prompt, startedAt));
  printHeartbeat(trials, startedAt);
  warnings = emitWarnings(trials, warnings);
  console.error(`[pi-live] Harbor exited ${exitCode}; ${resultSummary()}`);
  for (const trial of trials) {
    const result = trial.result;
    if (result) {
      console.error(`[pi-live] ${trial.name}: ${rewardSummary(trial.path)} exception=${result.exception_info?.exception_type ?? "none"}`);
    }
  }
  return exitCode;
}

if (import.meta.main) {
  process.exitCode = await main();
}
