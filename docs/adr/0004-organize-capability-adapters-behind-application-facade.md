# Organize capability adapters behind an application facade

Use an application facade shared by the CLI, dashboard, and MCP interfaces, with a central registry and factory functions to construct compiled provider adapters. Separate monitoring, agent execution, and Agent MCP Registration interfaces so integrations implement only the capabilities they support. Factories perform construction rather than forming an extra layer on every request; strategy implementations are warranted only for policies that actually vary.

MCP Hosts have identities independent of Agent Integrations. A registration adapter may exist for a host that cannot perform Agent Runs; neither the shared registry nor gateway configuration may require an execution adapter to register or select that host.

Group provider-specific discovery, monitoring, execution, and agent configuration knowledge within each provider folder. The MCP Gateway uses protocol transport adapters so adding a compatible external server normally requires configuration rather than provider-specific Rust code. This keeps integration changes local while allowing shared workflows to be tested through the same interfaces used by callers.

## Accepted workspace structure

```text
crates/
├── aifuel-core/          # Domain types and capability interfaces
├── aifuel-app/           # Facade, collection, runs, setup workflows
├── aifuel-providers/
│   └── src/
│       ├── registry.rs  # Registration and factory functions
│       ├── claude/       # Discovery, monitoring, execution, MCP setup
│       ├── codex/
│       └── gemini/
├── aifuel-mcp/
│   └── src/
│       ├── monitoring/  # AI Fuel's read-only status server
│       ├── gateway/     # Tool routing and isolated sessions
│       └── transports/ # External MCP connections
└── aifuel/              # CLI, dashboard, dependency construction
```

The provider folders shown are examples, not a reduction of provider coverage. This decision approves the architecture; implementation and verification remain pending.
