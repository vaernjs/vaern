// An MCP server declared in TypeScript, with no SDK install. Any MCP client
// can start it over stdio:
//   vaern mcp serve examples/mcp/calc-server.ts
import { tool } from "vaern:ai";
import { server } from "vaern:mcp";

const add = tool({
  name: "add",
  description: "Add two integers.",
  parameters: { type: "object", properties: { a: { type: "integer" }, b: { type: "integer" } }, required: ["a", "b"] },
  run: ({ a, b }: { a: number; b: number }) => ({ sum: a + b }),
});

server({ name: "calc", version: "1.0.0", tools: [add] });
