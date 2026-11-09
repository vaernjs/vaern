// vaern:ai without a model server: the budget refuses the call before any
// connection is made, and tools run and check their arguments locally.
import { budget, BudgetError, model, tool, ToolError } from "vaern:ai";

console.log(JSON.stringify(budget({ usd: 0.001, tokens: 50 })));
const m = model({ baseUrl: "http://127.0.0.1:9/v1", model: "m", price: { inputPerMTok: 1, outputPerMTok: 1 } });
try {
  await m.chat({ messages: [{ role: "user", content: "hi" }], maxTokens: 100 });
} catch (err) {
  if (!(err instanceof BudgetError)) throw err;
  console.log(err.code, err.unit, err.reserve, err.remaining);
}
const double = tool({
  name: "double",
  parameters: { type: "object", properties: { n: { type: "integer" } }, required: ["n"] },
  run: ({ n }: { n: number }) => n * 2,
});
console.log(await double.call({ n: 21 }));
try {
  await double.call({ n: "x" });
} catch (err) {
  if (!(err instanceof ToolError)) throw err;
  console.log(err.stage, err.message);
}
