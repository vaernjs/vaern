// A server that declares itself and never returns from its top level, so it
// answers nothing.
import { tool } from "vaern:ai";
import { server } from "vaern:mcp";

server({ name: "stuck", version: "1.0.0", tools: [tool({ name: "ping", run: () => ({}) })] });
for (;;) {}
