// Connects to mcp_server.ts, a second vaern process, and calls its tools.
import { connect } from "vaern:mcp";

const calc = await connect({ script: "./mcp_server.ts" });
const [add, fail] = await calc.tools();
console.log(calc.name, calc.protocolVersion, [add.name, fail.name].join(","));
console.log(JSON.stringify(await add.call({ a: 2, b: 3 })));
try {
  await fail.call({});
} catch (err) {
  console.log(`${err.name} ${err.stage}: ${err.message}`);
}
await calc.close();
