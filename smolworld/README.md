# Runebench agent-core Smolworld

This fixture is the Smolworld runtime analogue of the local Harbor
`agent-core-direct` smoke run. It owns one machine, `agent`, whose Smolfile
starts the Runebench game stack and whose in-guest `runebench-pi-agent` host
starts the fixed `rs-agent` MCP server from `/app/mcp/server.ts`.

The authored world owns topology, the explicit `network.egress` opt-in, and the task save-file seed. The
Smolfile owns the workload entrypoint, environment, and VM resources. The
`agent-core.tar` archive is ignored host-prepared material exported from the
native `AGENT_CORE_IMAGE`; guests never build or pull it.

The Make target prepares the archive, copies the selected task's `agent.sav`
into this fixture, seals the local inputs, starts the world, delegates the
agent and verifier commands into the machine, and lets the foreground
Smolworld supervisor perform exact cleanup on exit.
