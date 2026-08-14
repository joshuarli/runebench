"""Harbor adapter for the Runebench `pi-agent-core-rs` world host.

The adapter deliberately does not install or invoke the Pi TypeScript SDK/CLI.
The container image supplies the Rust host, while this class only passes the
task instruction and Harbor-resolved OpenRouter credential into that host.
"""

from __future__ import annotations

import shlex

from harbor.agents.base import BaseAgent
from harbor.agents.model_connection import ModelConnectionSpec
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext


class RunebenchPiAgentCore(BaseAgent):
    """Runebench's core host with an explicit rs-agent MCP capability binding."""

    MODEL_CONNECTION = ModelConnectionSpec(passthrough=True)
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
        if not self.model_name or not self.model_name.startswith("openrouter/"):
            raise ValueError(
                "RunebenchPiAgentCore currently requires an openrouter/<model> name"
            )
        access = self.model_connection
        if access.provider != "openrouter" or "OPENROUTER_API_KEY" not in access.env:
            raise RuntimeError(
                "OpenRouter credentials were not resolved; run through "
                "vault OPENROUTER_API_KEY -- …"
            )

        command = "set -o pipefail; " + " ".join(
            [
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
                "2>&1",
                "|",
                "tee",
                "/logs/agent/pi-agent-core.txt",
            ]
        )
        result = await environment.exec(command=command, env=dict(access.env))
        if result.return_code != 0:
            detail = (result.stderr or result.stdout or "no host output").strip()
            raise RuntimeError(f"pi-agent-core-rs Runebench host failed: {detail}")
