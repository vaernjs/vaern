// Connects to the server script in argv[2] with timeoutMs argv[3] and prints
// how its start failed: McpError stage timeout, naming the step that ran out.
import { connect } from "vaern:mcp";

const [script, timeoutMs] = process.argv.slice(2);
try {
  await connect({ script, timeoutMs: Number(timeoutMs) });
  console.log("connected");
} catch (err) {
  console.log(err.name, err.stage, err.code, err.message.match(/step \w+/)?.[0]);
}
