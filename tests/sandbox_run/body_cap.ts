// A body over the 1 MiB server cap is answered with 413 before the handler runs.
import { serve } from "vaern:http";

let handled = 0;
const server = serve(() => {
  handled += 1;
  return new Response("handled");
}, { port: 0, hostname: "127.0.0.1" });

const res = await fetch(`http://127.0.0.1:${server.addr.port}/upload`, {
  method: "POST",
  body: "x".repeat(2 * 1024 * 1024),
});
console.log(`status ${res.status}, handler ran ${handled} times`);
await server.close();
