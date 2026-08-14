SHELL := /bin/bash

HARBOR_PROJECT ?= $(HOME)/d/harbor
AGENT_CORE_MODEL ?= openrouter/deepseek/deepseek-v4-flash-0731
AGENT_CORE_TASK ?= tasks/woodcutting-xp-5m
AGENT_CORE_JOBS_DIR ?= jobs
AGENT_CORE_PLATFORM ?= linux/arm64
AGENT_CORE_BASE_IMAGE ?= runebench-base:local-arm64-agent-core
AGENT_CORE_IMAGE ?= runebench:local-arm64-agent-core
AGENT_CORE_BASE_DOCKERFILE ?= Dockerfile.base.agent-core
AGENT_CORE_DOCKERFILE ?= Dockerfile.agent-core
AGENT_CORE_BASE_REPO := $(word 1,$(subst :, ,$(AGENT_CORE_BASE_IMAGE)))
AGENT_CORE_BASE_TAG := $(word 2,$(subst :, ,$(AGENT_CORE_BASE_IMAGE)))
AGENT_CORE_REPO := $(word 1,$(subst :, ,$(AGENT_CORE_IMAGE)))
AGENT_CORE_TAG := $(word 2,$(subst :, ,$(AGENT_CORE_IMAGE)))

.PHONY: agent-core agent-core-image agent-core-generate agent-core-config agent-core-direct

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
	  AGENT_CORE_AGENT_TIMEOUT_MULTIPLIER="$(AGENT_CORE_AGENT_TIMEOUT_MULTIPLIER)" \
	  AGENT_CORE_VERIFIER_TIMEOUT_MULTIPLIER="$(AGENT_CORE_VERIFIER_TIMEOUT_MULTIPLIER)" \
	  AGENT_CORE_JOB_NAME="$(AGENT_CORE_JOB_NAME)" \
	  vault OPENROUTER_API_KEY -- \
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
	  -o "$(AGENT_CORE_JOBS_DIR)" \
	  -n 1 -k 1 -y
