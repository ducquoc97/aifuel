# Separate AI Fuel's MCP server and external MCP connections

AI Fuel's architecture covers two separate MCP roles: exposing its own read-only monitoring interface to hosts, and acting as a client of external MCP servers. Keep their interfaces and configuration separate because publishing AI Fuel status and consuming external capabilities have different responsibilities and lifecycles. External connections serve the MCP Gateway described in ADR-0002; they do not expand the existing monitoring server's read-only contract.
