// An agent with one tool, under a budget the engine checks before every
// model call, against a local OpenAI-compatible server (Ollama, llama.cpp,
// vLLM). Run it, then reproduce its transcript from the journal alone:
//   vaern run examples/agent-budget-replay.ts [baseUrl] [model]
//   vaern replay latest
import { agent, budget, BudgetError, model, tool } from "vaern:ai";

budget({ usd: 0.01, tokens: 4000 });
const baseUrl = process.argv[2] ?? "http://127.0.0.1:11434/v1";
const local = model({ baseUrl, model: process.argv[3] ?? "llama3.2", price: { inputPerMTok: 0.1, outputPerMTok: 0.4 } });
const add = tool({
  name: "add",
  description: "Add two integers.",
  parameters: { type: "object", properties: { a: { type: "integer" }, b: { type: "integer" } }, required: ["a", "b"] },
  run: ({ a, b }: { a: number; b: number }) => a + b,
});
const out = await agent({ model: local, tools: [add], maxSteps: 4 }).run("What is 17 + 25? Use the add tool.");
console.log(`answer: ${out.text}\nsteps: ${out.steps.map((s) => s.type).join(" > ")}`);
try {
  await local.chat({ messages: [{ role: "user", content: "Write a long novel." }], maxTokens: 8000 });
} catch (err) {
  if (!(err instanceof BudgetError)) throw err;
  console.log(`refused before sending: ${err.message}`);
}
