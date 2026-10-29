// Verifies HTTP server responds with status 200 and exact plain text body.

import { serve } from "vaern:http";

const server = serve((_req: Request) => {
  return new Response("Hello from Vaern HTTP!\n", {
    status: 200,
    headers: { "Content-Type": "text/plain" },
  });
}, { port: 8000, hostname: "127.0.0.1" });

console.log(`Server listening on http://${server.addr.hostname}:${server.addr.port}`);

if (process.argv.includes("--test")) {
  const res = await fetch("http://127.0.0.1:8000/");
  const body = await res.text();
  if (res.status !== 200 || body !== "Hello from Vaern HTTP!\n") {
    console.error(`Unexpected response: status=${res.status}, body=${body}`);
    process.exitCode = 1;
  } else {
    console.log("serve_basic passed");
  }
  await server.close();
}
