// JSON-schema-ish shape accepted by MCP's `inputSchema`. We keep this loose
// (Record<string, unknown>) so call sites can pass whatever shape they need
// without us re-declaring the entire JSON Schema type.
export type ToolInputSchema = {
  type: 'object';
  properties?: Record<string, unknown>;
  required?: string[];
  additionalProperties?: boolean;
};

export type ToolHandler = (args: Record<string, unknown>) => Promise<unknown> | unknown;

export interface ToolDefinition {
  name: string;
  description: string;
  inputSchema: ToolInputSchema;
  handler: ToolHandler;
}
