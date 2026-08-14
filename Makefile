SHELL := /bin/bash

HARBOR_PROJECT ?= $(HOME)/d/harbor
AGENT_CORE_MODEL ?= openrouter/deepseek/deepseek-v4-flash-0731
AGENT_CORE_COMMANDCODE_MODEL ?= commandcode/poolside/laguna-s-2.1-free
AGENT_CORE_COMMANDCODE_ENV_FILE ?=
AGENT_CORE_TASK ?= tasks/woodcutting-xp-5m
AGENT_CORE_JOBS_DIR ?= jobs
AGENT_CORE_RUN_DEADLINE_SEC ?= 390
AGENT_CORE_PLATFORM ?= linux/arm64
AGENT_CORE_BASE_IMAGE ?= runebench-base:local-arm64-agent-core
AGENT_CORE_IMAGE ?= runebench:local-arm64-agent-core
AGENT_CORE_BASE_DOCKERFILE ?= Dockerfile.base.agent-core
AGENT_CORE_DOCKERFILE ?= Dockerfile.agent-core
AGENT_CORE_BASE_REPO := $(word 1,$(subst :, ,$(AGENT_CORE_BASE_IMAGE)))
AGENT_CORE_BASE_TAG := $(word 2,$(subst :, ,$(AGENT_CORE_BASE_IMAGE)))
AGENT_CORE_REPO := $(word 1,$(subst :, ,$(AGENT_CORE_IMAGE)))
AGENT_CORE_TAG := $(word 2,$(subst :, ,$(AGENT_CORE_IMAGE)))
AGENT_CORE_SMOLWORLD_ARCHIVE ?= $(CURDIR)/smolworld/agent-core.tar
AGENT_CORE_SMOLWORLD_VAULT_KEY ?= OPENROUTER_API_KEY
AGENT_CORE_SMOLWORLD_CREDENTIAL_ENV ?= $(AGENT_CORE_SMOLWORLD_VAULT_KEY)

.PHONY: agent-core agent-core-commandcode agent-core-image agent-core-smolworld-image agent-core-generate agent-core-config agent-core-direct agent-core-direct-smolworld smolworld-fixture-check

smolworld-fixture-check:
	@bash tests/check-smolworld-agent-core-fixture.sh

agent-core-image:
	@echo "Building native $(AGENT_CORE_PLATFORM) base image $(AGENT_CORE_BASE_IMAGE)"
	@cd docker && \
	  PLATFORM="$(AGENT_CORE_PLATFORM)" \
	  BASE_DOCKERFILE="$(AGENT_CORE_BASE_DOCKERFILE)" \
	  IMAGE_NAME="$(AGENT_CORE_BASE_REPO)" \
	  IMAGE_TAG="$(AGENT_CORE_BASE_TAG)" \
	  ./build.sh --base
	@echo "Building native $(AGENT_CORE_PLATFORM) agent-core image $(AGENT_CORE_IMAGE)"
	@cd docker && \
	  PLATFORM="$(AGENT_CORE_PLATFORM)" \
	  BASE_IMAGE="$(AGENT_CORE_BASE_IMAGE)" \
	  DOCKERFILE="$(AGENT_CORE_DOCKERFILE)" \
	  BUILD_CONTEXT=".." \
	  PI_AGENT_CORE_CONTEXT="../../pi-agent-core-rs" \
	  IMAGE_NAME="$(AGENT_CORE_REPO)" \
	  IMAGE_TAG="$(AGENT_CORE_TAG)" \
	  ./build.sh

# Smolworld consumes host-prepared local OCI archives. Keep Docker confined to
# this preparation step; the benchmark workload, agent host, and MCP server
# run inside the recorded Smolworld machine after the export.
agent-core-smolworld-image: agent-core-image
	@command -v docker >/dev/null || { echo 'docker is required to export the host-prepared Smolworld archive' >&2; exit 1; }
	@mkdir -p "$(dir $(AGENT_CORE_SMOLWORLD_ARCHIVE))"
	@echo "Exporting $(AGENT_CORE_IMAGE) to $(AGENT_CORE_SMOLWORLD_ARCHIVE)"
	@docker save --output "$(AGENT_CORE_SMOLWORLD_ARCHIVE)" "$(AGENT_CORE_IMAGE)"

agent-core-generate:
	@RUNEBENCH_DOCKER_IMAGE="$(AGENT_CORE_IMAGE)" bun generate-tasks.ts

agent-core-config:
	@RUNEBENCH_DOCKER_IMAGE="$(AGENT_CORE_IMAGE)" bun generate-tasks.ts >/dev/null
	@PYTHONPATH="$(CURDIR)/agents:$${PYTHONPATH:-}" \
	  uv run --project "$(HARBOR_PROJECT)" harbor run --print-config \
	  -p "$(AGENT_CORE_TASK)" \
	  -e docker \
	  -a 'pi_agent_core_adapter:RunebenchPiAgentCore' \
	  -m "$(AGENT_CORE_MODEL)" \
	  --agent-kwarg "run_deadline_sec=$(AGENT_CORE_RUN_DEADLINE_SEC)" \
	  -o "$(AGENT_CORE_JOBS_DIR)" \
	  -n 1 -k 1 | jq .

agent-core: agent-core-image agent-core-generate
	@command -v vault >/dev/null || { echo 'vault is required for the OpenRouter key' >&2; exit 1; }
	@command -v docker >/dev/null || { echo 'docker is required for local Harbor runs' >&2; exit 1; }
	@echo "Running pi-agent-core-rs on $(AGENT_CORE_TASK) with $(AGENT_CORE_MODEL) (live monitor=on)"
	@PYTHONPATH="$(CURDIR)/agents:$${PYTHONPATH:-}" \
	  AGENT_CORE_TASK="$(AGENT_CORE_TASK)" \
	  AGENT_CORE_MODEL="$(AGENT_CORE_MODEL)" \
	  AGENT_CORE_JOBS_DIR="$(AGENT_CORE_JOBS_DIR)" \
	  AGENT_CORE_RUN_DEADLINE_SEC="$(AGENT_CORE_RUN_DEADLINE_SEC)" \
	  AGENT_CORE_AGENT_TIMEOUT_MULTIPLIER="$(AGENT_CORE_AGENT_TIMEOUT_MULTIPLIER)" \
	  AGENT_CORE_VERIFIER_TIMEOUT_MULTIPLIER="$(AGENT_CORE_VERIFIER_TIMEOUT_MULTIPLIER)" \
	  AGENT_CORE_JOB_NAME="$(AGENT_CORE_JOB_NAME)" \
	  vault OPENROUTER_API_KEY -- \
	  bun scripts/run-pi-live.ts

# Command Code keys are supplied from a caller-selected env file rather than
# discovered by the Rust core or checked into this repository. The local
# adapter converts the key into a provider-scoped container environment only.
agent-core-commandcode: agent-core-image agent-core-generate
	@test -n "$(AGENT_CORE_COMMANDCODE_ENV_FILE)" || { echo 'AGENT_CORE_COMMANDCODE_ENV_FILE is required' >&2; exit 1; }
	@test -f "$(AGENT_CORE_COMMANDCODE_ENV_FILE)" || { echo 'AGENT_CORE_COMMANDCODE_ENV_FILE does not exist' >&2; exit 1; }
	@set -euo pipefail; \
	  set -a; . "$(AGENT_CORE_COMMANDCODE_ENV_FILE)"; set +a; \
	  test -n "$${COMMANDCODE_API_KEY:-}" || { echo 'COMMANDCODE_API_KEY is absent from AGENT_CORE_COMMANDCODE_ENV_FILE' >&2; exit 1; }; \
	  echo "Running pi-agent-core-rs on $(AGENT_CORE_TASK) with $(AGENT_CORE_COMMANDCODE_MODEL) (live monitor=on)"; \
	  PYTHONPATH="$(CURDIR)/agents:$${PYTHONPATH:-}" \
	  AGENT_CORE_TASK="$(AGENT_CORE_TASK)" \
	  AGENT_CORE_MODEL="$(AGENT_CORE_COMMANDCODE_MODEL)" \
	  AGENT_CORE_JOBS_DIR="$(AGENT_CORE_JOBS_DIR)" \
	  AGENT_CORE_RUN_DEADLINE_SEC="$(AGENT_CORE_RUN_DEADLINE_SEC)" \
	  AGENT_CORE_AGENT_TIMEOUT_MULTIPLIER="$(AGENT_CORE_AGENT_TIMEOUT_MULTIPLIER)" \
	  AGENT_CORE_VERIFIER_TIMEOUT_MULTIPLIER="$(AGENT_CORE_VERIFIER_TIMEOUT_MULTIPLIER)" \
	  AGENT_CORE_JOB_NAME="$(AGENT_CORE_JOB_NAME)" \
	  bun scripts/run-pi-live.ts

# Escape hatch for diagnosing Harbor without the live wrapper. Normal runs
# should use `make agent-core`, which adds a deterministic job name and health output.
agent-core-direct: agent-core-image agent-core-generate
	@command -v vault >/dev/null || { echo 'vault is required for the OpenRouter key' >&2; exit 1; }
	@PYTHONPATH="$(CURDIR)/agents:$${PYTHONPATH:-}" \
	  vault OPENROUTER_API_KEY -- \
	  uv run --project "$(HARBOR_PROJECT)" harbor run \
	  -p "$(AGENT_CORE_TASK)" \
	  -e docker \
	  -a 'pi_agent_core_adapter:RunebenchPiAgentCore' \
	  -m "$(AGENT_CORE_MODEL)" \
	  --agent-kwarg "run_deadline_sec=$(AGENT_CORE_RUN_DEADLINE_SEC)" \
	  -o "$(AGENT_CORE_JOBS_DIR)" \
	  -n 1 -k 1 -y

# Direct analogue of agent-core-direct using the Smolworld world supervisor.
# The task image is exported once as local material; no Docker environment or
# Harbor adapter is involved after that archive has been prepared. Select the
# provider key with AGENT_CORE_SMOLWORLD_VAULT_KEY and keep the guest variable
# explicit via AGENT_CORE_SMOLWORLD_CREDENTIAL_ENV.
agent-core-direct-smolworld: smolworld-fixture-check agent-core-smolworld-image agent-core-generate
	@command -v vault >/dev/null || { echo 'vault is required for the provider key' >&2; exit 1; }
	@echo "Running pi-agent-core-rs in Smolworld on $(AGENT_CORE_TASK) with $(AGENT_CORE_MODEL)"
	@vault "$(AGENT_CORE_SMOLWORLD_VAULT_KEY)" -- \
	  env \
	  AGENT_CORE_TASK="$(AGENT_CORE_TASK)" \
	  AGENT_CORE_MODEL="$(AGENT_CORE_MODEL)" \
	  AGENT_CORE_RUN_DEADLINE_SEC="$(AGENT_CORE_RUN_DEADLINE_SEC)" \
	  AGENT_CORE_CREDENTIAL_ENV="$(AGENT_CORE_SMOLWORLD_CREDENTIAL_ENV)" \
	  AGENT_CORE_SMOLWORLD_ARCHIVE="$(AGENT_CORE_SMOLWORLD_ARCHIVE)" \
	  bash scripts/run-agent-core-smolworld.sh
