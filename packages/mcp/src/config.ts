export interface BuildMcpConfigOptions {
  sessionId: string;
  // Path or bare name of the burn binary. Defaults to `burn`, which Claude
  // Code's shell resolves against PATH when it spawns the MCP server.
  burnBin?: string;
}

export interface BuildMcpConfigResult {
  // JSON string ready to be passed to `claude --mcp-config <json|path>`.
  // Inline JSON is supported by Claude Code in addition to file paths.
  config: string;
}

// Produce the `--mcp-config` blob that registers `burn mcp-server` as the
// `burn` MCP server for a spawned claude session. The sessionId is baked
// into the server command line so tool handlers default to querying that
// session without the agent having to plumb it through on every call.
//
// Design note: we only register a stdio server. HTTP-mode registration is
// out of scope for the spawner-integration path — a long-lived HTTP daemon
// would require lifecycle management the spawner doesn't own.
export function buildMcpConfig(options: BuildMcpConfigOptions): BuildMcpConfigResult {
  const burnBin = options.burnBin ?? 'burn';
  const config = {
    mcpServers: {
      burn: {
        type: 'stdio',
        command: burnBin,
        args: ['mcp-server', '--session-id', options.sessionId],
      },
    },
  };
  return { config: JSON.stringify(config) };
}
