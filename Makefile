SHELL := /bin/bash

HARBOR_PROJECT ?= $(HOME)/d/harbor
PI_MODEL ?= openrouter/deepseek/deepseek-v4-flash-0731
PI_THINKING ?= high
PI_TASK ?= tasks/woodcutting-xp-5m
PI_JOBS_DIR ?= jobs
PI_PLATFORM ?= linux/arm64
PI_BASE_IMAGE ?= runebench-base:local-arm64-pi
PI_IMAGE ?= runebench:local-arm64-pi
PI_BASE_DOCKERFILE ?= Dockerfile.base.pi
PI_DOCKERFILE ?= Dockerfile.pi
PI_BASE_REPO := $(word 1,$(subst :, ,$(PI_BASE_IMAGE)))
PI_BASE_TAG := $(word 2,$(subst :, ,$(PI_BASE_IMAGE)))
PI_REPO := $(word 1,$(subst :, ,$(PI_IMAGE)))
PI_TAG := $(word 2,$(subst :, ,$(PI_IMAGE)))

.PHONY: pi pi-image pi-generate pi-config pi-direct

pi-image:
	@echo "Building native $(PI_PLATFORM) base image $(PI_BASE_IMAGE)"
	@cd docker && \
	  PLATFORM="$(PI_PLATFORM)" \
	  BASE_DOCKERFILE="$(PI_BASE_DOCKERFILE)" \
	  IMAGE_NAME="$(PI_BASE_REPO)" \
	  IMAGE_TAG="$(PI_BASE_TAG)" \
	  ./build.sh --base
	@echo "Building native $(PI_PLATFORM) app image $(PI_IMAGE)"
	@cd docker && \
	  PLATFORM="$(PI_PLATFORM)" \
	  BASE_IMAGE="$(PI_BASE_IMAGE)" \
	  DOCKERFILE="$(PI_DOCKERFILE)" \
	  IMAGE_NAME="$(PI_REPO)" \
	  IMAGE_TAG="$(PI_TAG)" \
	  ./build.sh

pi-generate:
	@RUNEBENCH_DOCKER_IMAGE="$(PI_IMAGE)" bun generate-tasks.ts

pi-config:
	@RUNEBENCH_DOCKER_IMAGE="$(PI_IMAGE)" bun generate-tasks.ts >/dev/null
	@PYTHONPATH="$(CURDIR)/agents:$${PYTHONPATH:-}" \
	  uv run --project "$(HARBOR_PROJECT)" harbor run --print-config \
	  -p "$(PI_TASK)" \
	  -e docker \
	  -a 'pi_adapter:RunebenchPi' \
	  -m "$(PI_MODEL)" \
	  --agent-kwarg "thinking=$(PI_THINKING)" \
	  -o "$(PI_JOBS_DIR)" \
	  -n 1 -k 1 | jq .

pi: pi-image pi-generate
	@command -v vault >/dev/null || { echo 'vault is required for the OpenRouter key' >&2; exit 1; }
	@command -v docker >/dev/null || { echo 'docker is required for local Harbor runs' >&2; exit 1; }
	@echo "Running Pi on $(PI_TASK) with $(PI_MODEL) (thinking=$(PI_THINKING), live monitor=on)"
	@PYTHONPATH="$(CURDIR)/agents:$${PYTHONPATH:-}" \
	  PI_TASK="$(PI_TASK)" \
	  PI_MODEL="$(PI_MODEL)" \
	  PI_THINKING="$(PI_THINKING)" \
	  PI_JOBS_DIR="$(PI_JOBS_DIR)" \
	  PI_AGENT_TIMEOUT_MULTIPLIER="$(PI_AGENT_TIMEOUT_MULTIPLIER)" \
	  PI_VERIFIER_TIMEOUT_MULTIPLIER="$(PI_VERIFIER_TIMEOUT_MULTIPLIER)" \
	  PI_JOB_NAME="$(PI_JOB_NAME)" \
	  vault OPENROUTER_API_KEY -- \
	  bun scripts/run-pi-live.ts

# Escape hatch for diagnosing Harbor without the live wrapper. Normal runs
# should use `make pi`, which adds a deterministic job name and health output.
pi-direct: pi-image pi-generate
	@command -v vault >/dev/null || { echo 'vault is required for the OpenRouter key' >&2; exit 1; }
	@PYTHONPATH="$(CURDIR)/agents:$${PYTHONPATH:-}" \
	  vault OPENROUTER_API_KEY -- \
	  uv run --project "$(HARBOR_PROJECT)" harbor run \
	  -p "$(PI_TASK)" \
	  -e docker \
	  -a 'pi_adapter:RunebenchPi' \
	  -m "$(PI_MODEL)" \
	  --agent-kwarg "thinking=$(PI_THINKING)" \
	  -o "$(PI_JOBS_DIR)" \
	  -n 1 -k 1 -y
