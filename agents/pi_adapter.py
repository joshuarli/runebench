"""Runebench Pi adapter with an MCP-to-Pi custom-tool bridge.

Harbor's built-in Pi agent installs and launches the current Pi CLI, but Pi
intentionally does not include MCP. Runebench exposes the game through the
``rs-agent`` stdio MCP server, so this adapter installs a Pi extension that
bridges the MCP tools into Pi's native custom-tool API.

Usage with the Harbor checkout in ``~/d/harbor``::

    PYTHONPATH=agents uv run --project ~/d/harbor harbor run \
        -p tasks/woodcutting-xp-5m \
        -e docker \
        -a 'pi_adapter:RunebenchPi' \
        -m 'openrouter/poolside/laguna-xs-2.1:free'
"""

from __future__ import annotations

import base64
import json
import shlex
from pathlib import Path
from typing import Any

from harbor.agents.installed.pi import Pi
from harbor.environments.base import BaseEnvironment


_EXTENSION_SOURCE = Path(__file__).with_name("pi_rs_agent_extension.ts")
_REMOTE_EXTENSION = "$HOME/.pi/agent/extensions/runebench-rs-agent.ts"
_REMOTE_MODELS = "$HOME/.pi/agent/models.json"

_MODEL_METADATA = {
    "deepseek/deepseek-v4-flash-0731": {
        "name": "DeepSeek: DeepSeek V4 Flash 0731",
        "contextWindow": 1_048_576,
        "maxTokens": 384_000,
        "cost": {
            "input": 0.08,
            "cacheRead": 0.016,
            "cacheWrite": 0,
            "output": 0.18,
        },
    },
    "poolside/laguna-xs-2.1:free": {
        "name": "Poolside: Laguna XS 2.1 (free)",
        "contextWindow": 262_144,
        "maxTokens": 32_768,
        "cost": {
            "input": 0,
            "cacheRead": 0,
            "cacheWrite": 0,
            "output": 0,
        },
    },
    "liquid/lfm-2.5-2.6b:free": {
        "name": "LiquidAI: LFM2.5-2.6B (free)",
        "contextWindow": 128_000,
        "maxTokens": 32_768,
        "cost": {
            "input": 0,
            "cacheRead": 0,
            "cacheWrite": 0,
            "output": 0,
        },
    },
}


class RunebenchPi(Pi):
    """Harbor's Pi agent with the Runebench ``rs-agent`` tool bridge."""

    @staticmethod
    def name() -> str:
        return "pi-runebench"

    def _mcp_server_payload(self) -> list[dict[str, Any]]:
        servers: list[dict[str, Any]] = []
        for server in self.mcp_servers:
            entry: dict[str, Any] = {
                "name": server.name,
                "transport": server.transport,
            }
            if server.transport == "stdio":
                entry["command"] = server.command
                entry["args"] = server.args
            else:
                entry["url"] = server.url
            servers.append(entry)
        return servers

    def _render_extension(self) -> str:
        if not _EXTENSION_SOURCE.is_file():
            raise FileNotFoundError(f"Pi extension source not found: {_EXTENSION_SOURCE}")

        source = _EXTENSION_SOURCE.read_text()
        marker = "const RUNEBENCH_MCP_SERVERS = __RUNEBENCH_MCP_SERVERS__;"
        replacement = (
            "const RUNEBENCH_MCP_SERVERS = "
            + json.dumps(self._mcp_server_payload(), separators=(",", ":"))
            + " as const;"
        )
        if marker not in source:
            raise RuntimeError("Pi extension is missing its MCP server marker")
        return source.replace(marker, replacement, 1)

    def _render_models_config(self) -> str:
        if not self.model_name or "/" not in self.model_name:
            raise ValueError("Pi model must be in provider/model format")

        provider, model_id = self.model_name.split("/", 1)
        # Harbor/OpenRouter names are `openrouter/<vendor>/<model>`, while
        # this table intentionally keys the catalog portion as
        # `<vendor>/<model>`. Looking up `provider/model_id` here would miss
        # every explicit OpenRouter entry and silently apply the fallback.
        metadata = _MODEL_METADATA.get(
            model_id,
            {
                "name": model_id,
                "contextWindow": 128_000,
                "maxTokens": 32_768,
                "cost": {
                    "input": 0,
                    "cacheRead": 0,
                    "cacheWrite": 0,
                    "output": 0,
                },
            },
        )

        # Pi's custom-model catalog prevents its CLI from borrowing an
        # unrelated provider model's context/max-token limits when OpenRouter
        # adds a model before Pi's bundled catalog knows about it.
        config = {
            "providers": {
                provider: {
                    "baseUrl": "https://openrouter.ai/api/v1",
                    "api": "openai-completions",
                    "apiKey": "$OPENROUTER_API_KEY",
                    "models": [
                        {
                            "id": model_id,
                            "name": metadata["name"],
                            "reasoning": False,
                            "input": ["text"],
                            "contextWindow": metadata["contextWindow"],
                            "maxTokens": metadata["maxTokens"],
                            "cost": metadata["cost"],
                            "compat": {
                                "supportsReasoningEffort": False,
                            },
                        }
                    ],
                }
            }
        }
        return json.dumps(config, indent=2)

    async def _write_agent_file(
        self,
        environment: BaseEnvironment,
        remote_path: str,
        content: str,
    ) -> None:
        encoded = base64.b64encode(content.encode()).decode()
        await self.exec_as_agent(
            environment,
            command=(
                "set -eu; "
                "mkdir -p \"$HOME/.pi/agent/extensions\"; "
                f"printf '%s' {shlex.quote(encoded)} | base64 -d > {remote_path}; "
                f"chmod 600 {remote_path}"
            ),
        )

    async def setup(self, environment: BaseEnvironment) -> None:
        # Harbor's native Pi setup installs the latest @earendil-works package
        # and performs its version probe. Keep that behavior intact.
        await super().setup(environment)

        if not self.mcp_servers:
            raise RuntimeError(
                "RunebenchPi requires at least one MCP server; the task did not "
                "provide the rs-agent server"
            )

        await self._write_agent_file(
            environment,
            _REMOTE_MODELS,
            self._render_models_config(),
        )
        await self._write_agent_file(
            environment,
            _REMOTE_EXTENSION,
            self._render_extension(),
        )
