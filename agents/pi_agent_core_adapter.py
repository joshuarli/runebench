"""Harbor adapter for the Runebench `pi-agent-core-rs` world host.

The adapter deliberately does not install or invoke the Pi TypeScript SDK/CLI.
The container image supplies the Rust host, while this class only passes the
task instruction and Harbor-resolved provider credential into that host.
"""

from __future__ import annotations

import shlex
from datetime import date
from uuid import uuid4

from harbor.agents.base import BaseAgent
from harbor.agents.model_connection import ModelConnectionSpec, resolve_model_connection
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext


class RunebenchPiAgentCore(BaseAgent):
    """Runebench's core host with an explicit rs-agent MCP capability binding."""

    MODEL_CONNECTION = ModelConnectionSpec(passthrough=True)
    _COMMANDCODE_CONNECTION = ModelConnectionSpec(
        default_provider="commandcode",
        api_key_envs=("COMMANDCODE_API_KEY",),
        passthrough=True,
    )
    _DEFAULT_RUN_DEADLINE_SEC = 390

    def __init__(
        self,
        run_deadline_sec: int | str = _DEFAULT_RUN_DEADLINE_SEC,
        *args,
        **kwargs,
    ):
        super().__init__(*args, **kwargs)
        try:
            self._run_deadline_sec = int(run_deadline_sec)
        except (TypeError, ValueError) as error:
            raise ValueError(
                "run_deadline_sec must be a positive whole number of seconds"
            ) from error
        if self._run_deadline_sec <= 0:
            raise ValueError("run_deadline_sec must be a positive whole number of seconds")

    @staticmethod
    def name() -> str:
        return "pi-agent-core-rs-runebench"

    def version(self) -> str | None:
        return "0.1.0"

    @property
    def model_connection(self):
        """Resolve Command Code's explicit key without changing Harbor globally."""
        if self.model_name and self.model_name.startswith("commandcode/"):
            return resolve_model_connection(
                self.model_name,
                self._COMMANDCODE_CONNECTION,
                self._resolve_env,
            )
        return super().model_connection

    async def setup(self, environment: BaseEnvironment) -> None:
        if not any(server.name == "rs-agent" for server in self.mcp_servers):
            raise RuntimeError(
                "RunebenchPiAgentCore requires the task's rs-agent MCP server"
            )
        result = await environment.exec(
            "test -x /usr/local/bin/runebench-pi-agent "
            "&& test -f /app/benchmark/runebench-policy.luau"
        )
        if result.return_code != 0:
            raise RuntimeError(
                "Runebench image does not contain the pi-agent-core-rs host and policy"
            )

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        if not self.model_name or not (
            self.model_name.startswith("openrouter/")
            or self.model_name.startswith("commandcode/")
        ):
            raise ValueError(
                "RunebenchPiAgentCore requires an openrouter/<model> or "
                "commandcode/<model> name"
            )
        access = self.model_connection
        is_commandcode = self.model_name.startswith("commandcode/")
        key_name = "COMMANDCODE_API_KEY" if is_commandcode else "OPENROUTER_API_KEY"
        expected_provider = "commandcode" if is_commandcode else "openrouter"
        if access.provider != expected_provider or key_name not in access.env:
            raise RuntimeError(
                f"{expected_provider} credentials were not resolved; supply {key_name} "
                "through the caller's secret boundary"
            )

        command_parts = [
            "/usr/local/bin/runebench-pi-agent",
            "--model",
            shlex.quote(self.model_name),
            "--instruction",
            shlex.quote(instruction),
            "--workspace",
            "/app",
            "--policy",
            "/app/benchmark/runebench-policy.luau",
            "--log-jsonl",
            "/logs/agent/pi-agent-core.jsonl",
            "--deadline-seconds",
            str(self._run_deadline_sec),
        ]
        if is_commandcode:
            # The Rust provider requires these caller-owned values. The benchmark
            # container is Linux; the per-trial UUID prevents unrelated transcripts
            # from being grouped as a single Command Code session.
            command_parts.extend(
                [
                    "--commandcode-date",
                    date.today().isoformat(),
                    "--commandcode-environment",
                    "linux",
                    "--commandcode-thread-id",
                    str(uuid4()),
                    "--commandcode-project-slug",
                    "runebench",
                ]
            )
        command = "set -o pipefail; " + " ".join(
            [*command_parts, "2>&1", "|", "tee", "/logs/agent/pi-agent-core.txt"]
        )
        result = await environment.exec(command=command, env=dict(access.env))
        if result.return_code != 0:
            detail = (result.stderr or result.stdout or "no host output").strip()
            raise RuntimeError(f"pi-agent-core-rs Runebench host failed: {detail}")
