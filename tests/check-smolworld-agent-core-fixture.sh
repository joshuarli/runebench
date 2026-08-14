#!/usr/bin/env bash
# Static contract check for the Runebench Smolworld smoke fixture.

set -euo pipefail

project_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
fixture_dir="$project_dir/smolworld"
world_file="$fixture_dir/.smolworld"
smolfile="$fixture_dir/smol/agent-core.Smolfile"

fail() {
    echo "agent-core Smolworld fixture: $*" >&2
    exit 1
}

[[ -f "$world_file" ]] || fail "missing $world_file"
[[ -f "$smolfile" ]] || fail "missing $smolfile"

grep -Fqx 'format: 2' "$world_file" || fail "world must declare format: 2"
grep -Fqx '  name: runebench-agent-core' "$world_file" || fail "unexpected world name"
grep -Fqx '  subnet: 10.91.0.0/24' "$world_file" || fail "unexpected subnet"
grep -Fqx '  domain: runebench-agent-core.test' "$world_file" || fail "unexpected DNS domain"
grep -Fqx '  egress: true' "$world_file" || fail "world must explicitly enable smolvm egress"
grep -Fqx '    smolfile: ./smol/agent-core.Smolfile' "$world_file" || fail "agent must use its Smolfile"
grep -Fqx '        destination: /app/server/engine/data/players/main/agent.sav' "$world_file" || fail "agent save must be a guest seed"

if grep -Eq '^[[:space:]]*(image|command|cpus|memory_mib|storage_gib|overlay_gib):' "$world_file"; then
    fail "workload fields leaked into .smolworld"
fi

grep -Fqx 'image = "../agent-core.tar"' "$smolfile" || fail "Smolfile must use the local agent archive"
grep -Fqx 'entrypoint = ["/entrypoint.sh"]' "$smolfile" || fail "Smolfile must start the Runebench entrypoint"
grep -Fqx 'workdir = "/app"' "$smolfile" || fail "Smolfile must use /app"
grep -Fqx '  "AGENT_CORE_MINIMAL=1",' "$smolfile" || fail "Smolfile must select the minimal agent-core stack"
grep -Fqx '  "RECORD_VIDEO=0",' "$smolfile" || fail "Smolfile must disable recording"
grep -Fqx 'cpus = 2' "$smolfile" || fail "Smolfile must declare two CPUs"
grep -Fqx 'memory = 8192' "$smolfile" || fail "Smolfile must declare 8192 MiB"
grep -Fqx 'storage = 10' "$smolfile" || fail "Smolfile must declare storage"
grep -Fqx 'overlay = 10' "$smolfile" || fail "Smolfile must declare overlay"

if grep -Eq '(^|[[:space:]])(net|ports|volumes|docker_socket|ssh_agent|health|restart)[[:space:]]*=' "$smolfile"; then
    fail "Smolfile declares a forbidden host-capability or lifecycle setting"
fi

grep -Fq 'for _ in {1..120}; do' "$project_dir/scripts/run-agent-core-smolworld.sh" || fail "runner must retry world teardown"
grep -Fq 'teardown_world' "$project_dir/scripts/run-agent-core-smolworld.sh" || fail "runner must use scoped teardown helper"

echo "PASS: agent-core fixture uses v2 topology, explicit egress, and restricted local Smolfile"
