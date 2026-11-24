import { tool } from "vaern:ai";
import { server } from "vaern:mcp";

const add = tool({
  name: "add",
  description: "Adds two numbers",
  parameters: {
    type: "object",
    properties: { a: { type: "number" }, b: { type: "number" } },
    required: ["a", "b"],
  },
  run: ({ a, b }: { a: number; b: number }) => {
    console.log(`adding ${a} and ${b}`);
    return { sum: a + b };
  },
});

const fail = tool({
  name: "fail",
  run: () => {
    throw new Error("no luck");
  },
});

server({ name: "calc", version: "1.0.0", tools: [add, fail] });
