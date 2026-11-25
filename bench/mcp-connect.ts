// Starts examples/mcp/calc-server.ts as a second vaern process, calls its
// tool once and closes it. Compared with examples/hello.ts, the difference
// is the cost of one server's start, negotiation, call and exit.
import { connect } from "vaern:mcp";

const calc = await connect({ script: "../examples/mcp/calc-server.ts" });
const [add] = await calc.tools();
console.log(JSON.stringify(await add.call({ a: 17, b: 25 })));
await calc.close();
