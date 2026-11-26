// A server that answers server/discover and then never runs again. The busy
// wait holds the top level until the probe is queued; the loop turn that
// answers it runs this timer next, before tools/list can arrive.
import { tool } from "vaern:ai";
import { server } from "vaern:mcp";

server({ name: "stuck", version: "1.0.0", tools: [tool({ name: "ping", run: () => ({}) })] });
const until = Date.now() + 200;
while (Date.now() < until) {}
setTimeout(() => {
  for (;;) {}
}, 0);
