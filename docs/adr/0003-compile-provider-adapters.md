# Compile provider adapters into AI Fuel

New provider integrations are Rust adapters compiled into the single AI Fuel binary. Group provider-specific implementation in a provider folder and register it centrally so extending integrations does not require changing common CLI, dashboard, or MCP behavior. This trades installation without rebuilding for simpler distribution and compile-time verification; runtime provider plugins are outside this design.
