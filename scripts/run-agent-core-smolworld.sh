#!/usr/bin/env bash
# Run one generated Runebench task inside the checked-in Smolworld fixture.
#
# The world supervisor owns the VM and private NIC. This script only delegates
# workload commands into the recorded `agent` machine and hands the provider
# credential across the one-command secret boundary.

set -Eeuo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
project_dir=$(cd "$script_dir/.." && pwd)
world_dir="$project_dir/smolworld"
world_file="$world_dir/.smolworld"
smolworld_bin="${SMOLWORLD_BIN:-smolworld}"
task_ref="${AGENT_CORE_TASK:-tasks/woodcutting-xp-5m}"
model="${AGENT_CORE_MODEL:-openrouter/deepseek/deepseek-v4-flash-0731}"
deadline="${AGENT_CORE_RUN_DEADLINE_SEC:-390}"
credential_env="${AGENT_CORE_CREDENTIAL_ENV:-OPENROUTER_API_KEY}"
output_dir="${AGENT_CORE_SMOLWORLD_OUTPUT_DIR:-$project_dir/jobs/agent-core-smolworld-$(date +%Y%m%d-%H%M%S)}"
archive_ref="${AGENT_CORE_SMOLWORLD_ARCHIVE:-$world_dir/agent-core.tar}"

if [[ "$task_ref" == /* ]]; then
    task_dir="$task_ref"
else
    task_dir="$project_dir/$task_ref"
fi
if [[ "$archive_ref" == /* ]]; then
    archive_path="$archive_ref"
else
    archive_path="$project_dir/$archive_ref"
fi

die() {
    echo "agent-core-smolworld: $*" >&2
    exit 1
}

command -v "$smolworld_bin" >/dev/null 2>&1 || die "smolworld is not on PATH; set SMOLWORLD_BIN"
[[ -f "$world_file" ]] || die "missing world file: $world_file"
[[ -f "$archive_path" ]] || die "missing $archive_path; run the image target first"
[[ -f "$task_dir/instruction.md" ]] || die "missing task instruction: $task_dir/instruction.md"
[[ -f "$task_dir/environment/agent.sav" ]] || die "missing task save: $task_dir/environment/agent.sav"
[[ "$credential_env" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || die "invalid credential environment name: $credential_env"
[[ -n "${!credential_env:-}" ]] || die "$credential_env is not set"

cp "$task_dir/environment/agent.sav" "$world_dir/agent.sav"
if [[ "$archive_path" != "$world_dir/agent-core.tar" ]]; then
    cp "$archive_path" "$world_dir/agent-core.tar"
fi

run_smolworld() {
    "$smolworld_bin" -f "$world_file" "$@"
}

teardown_world() {
    # SIGTERM makes the foreground supervisor begin its own scoped cleanup,
    # but its lifecycle lock can outlive the supervisor process while the VM
    # shutdown completes. Retry the same world-scoped `down` operation instead
    # of treating that transient lock as successful cleanup.
    local last_error=""
    for _ in {1..120}; do
        if last_error=$(run_smolworld down 2>&1); then
            return 0
        fi
        sleep 1
    done
    echo "agent-core-smolworld: world cleanup did not settle: ${last_error:-unknown error}" >&2
    return 1
}

echo "agent-core-smolworld: sealing $task_ref"
run_smolworld prepare
run_smolworld check

runtime_dir=$(mktemp -d "${TMPDIR:-/tmp}/runebench-smolworld.XXXXXX")
up_log="$runtime_dir/up.log"
up_pid=""

cleanup() {
    local status=$?
    trap - EXIT INT TERM
    if [[ -n "$up_pid" ]] && kill -0 "$up_pid" 2>/dev/null; then
        echo "agent-core-smolworld: stopping world" >&2
        kill -TERM "$up_pid" 2>/dev/null || true
        wait "$up_pid" 2>/dev/null || true
    fi
    # The supervisor normally performs this cleanup on signal. Repeat the
    # scoped world teardown here because a delegated guest command can make
    # the supervisor exit before its lifecycle record is finalized.
    if ! teardown_world && [[ "$status" -eq 0 ]]; then
        status=1
    fi
    rm -rf "$runtime_dir"
    exit "$status"
}
trap cleanup EXIT INT TERM

run_smolworld up >"$up_log" 2>&1 &
up_pid=$!
world_up=false
for _ in {1..180}; do
    if grep -Fq 'smolworld: world is up' "$up_log"; then
        world_up=true
        break
    fi
    if ! kill -0 "$up_pid" 2>/dev/null; then
        cat "$up_log" >&2
        die "world supervisor exited before attachment"
    fi
    sleep 1
done
if [[ "$world_up" != true ]]; then
    cat "$up_log" >&2
    die "world did not attach within 180 seconds"
fi

echo "agent-core-smolworld: world is up"
run_smolworld exec agent -- /ensure-services.sh
run_smolworld exec agent -- /bin/mkdir -p /logs/agent

# Harbor normally copies this task-local verifier into the environment image.
# Smolworld keeps the app archive reusable and transfers only this generated
# task input into the already-started machine.
"$smolworld_bin" cp -f "$world_file" \
    "$task_dir/tests/check_skill_xp.ts" agent:/tmp/check_skill_xp.ts

instruction=$(<"$task_dir/instruction.md")
commandcode_args=()
if [[ "$model" == commandcode/* ]]; then
    command -v uuidgen >/dev/null 2>&1 || die "uuidgen is required for Command Code runs"
    commandcode_args=(
        --commandcode-date "$(date -u +%Y-%m-%d)"
        --commandcode-environment linux
        --commandcode-thread-id "$(uuidgen | tr '[:upper:]' '[:lower:]')"
        --commandcode-project-slug runebench
    )
fi
agent_status=0
set +e
run_smolworld exec agent \
    --secret-env "$credential_env=$credential_env" \
    -- /usr/local/bin/runebench-pi-agent \
    --model "$model" \
    --instruction "$instruction" \
    --workspace /app \
    --policy /app/benchmark/runebench-policy.luau \
    --log-jsonl /logs/agent/pi-agent-core.jsonl \
    --deadline-seconds "$deadline" \
    "${commandcode_args[@]}"
agent_status=$?
set -e

echo "agent-core-smolworld: agent exited with status $agent_status"

verifier_status=0
set +e
run_smolworld exec agent -- /usr/bin/env SKILL_NAME=Woodcutting bun run /tmp/check_skill_xp.ts
verifier_status=$?
set -e

mkdir -p "$output_dir/agent" "$output_dir/tracking" "$output_dir/verifier"
copy_guest() {
    local guest_path="$1"
    local host_path="$2"
    set +e
    "$smolworld_bin" cp -f "$world_file" "agent:$guest_path" "$host_path" >/dev/null 2>&1
    local status=$?
    set -e
    if [[ "$status" -ne 0 ]]; then
        echo "agent-core-smolworld: optional artifact unavailable: $guest_path" >&2
    fi
}

copy_guest /logs/agent/pi-agent-core.jsonl "$output_dir/agent/pi-agent-core.jsonl"
copy_guest /logs/agent/pi-agent-core.txt "$output_dir/agent/pi-agent-core.txt"
copy_guest /logs/agent/runebench-pi-agent-core.json "$output_dir/agent/runebench-pi-agent-core.json"
copy_guest /logs/tracking/skill_tracking.json "$output_dir/tracking/skill_tracking.json"
copy_guest /logs/verifier/runebench-result.json "$output_dir/verifier/runebench-result.json"
copy_guest /logs/verifier/reward.json "$output_dir/verifier/reward.json"
copy_guest /logs/verifier/reward.txt "$output_dir/verifier/reward.txt"

echo "agent-core-smolworld: artifacts in $output_dir"
if [[ "$verifier_status" -ne 0 ]]; then
    exit "$verifier_status"
fi
exit "$agent_status"
