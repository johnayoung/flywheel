# flywheel-agents

Multi-agent execution layer. Runs real coding-agent harnesses (Claude Code first) behind one adapter contract, streaming normalized events and folding each run into a structured `CompletedRun`.

Design doc: [`docs/agent-harness.md`](../../docs/agent-harness.md). Bottom of the workspace stack: imports nothing from any flywheel package; stdlib-only at runtime (vendor SDKs are optional extras).

```python
from flywheel_agents import AgentConfiguration, AgentRuntime, RunRequest

runtime = AgentRuntime()
result = await runtime.run(
    RunRequest(
        prompt="Fix the failing tests.",
        working_directory=Path("/repo"),
        configuration=AgentConfiguration(agent_id="claude-code"),
    )
)
print(result.final_text, result.stop.reason)
```
