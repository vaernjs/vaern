// An agent whose tool lives in calc-server.ts, which the runtime starts as a
// second vaern process and stops at close(). Its model calls and MCP calls
// are journalled and metered in this run, against a local OpenAI-compatible
// server (Ollama, llama.cpp, vLLM):
//   vaern run examples/mcp/agent.ts [baseUrl] [model]
//   vaern replay <run-id>    (this run's id, under .vaern/runs)
import { agent, budget, model } from "vaern:ai";
import { connect } from "vaern:mcp";

budget({ usd: 0.01, tokens: 4000 });
const baseUrl = process.argv[2] ?? "http://127.0.0.1:11434/v1";
const local = model({ baseUrl, model: process.argv[3] ?? "llama3.2", price: { inputPerMTok: 0.1, outputPerMTok: 0.4 } });
const calc = await connect({ script: "./calc-server.ts" });
const out = await agent({ model: local, tools: await calc.tools(), maxSteps: 4 }).run("What is 17 + 25? Use the add tool.");
console.log(`answer: ${out.text}\nsteps: ${out.steps.map((s) => s.type).join(" > ")}`);
await calc.close();
console.log(`left: ${JSON.stringify(budget.remaining())}`);
