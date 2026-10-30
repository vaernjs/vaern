// Demonstrates HTTP client request execution and JSON parsing.
import { serve } from "vaern:http";

const server = serve((_req: Request) => {
  const payload = JSON.stringify({
    name: "vaern",
    version: "0.1.0",
    engine: "hyper+rustls",
  });
  return new Response(payload, {
    status: 200,
    headers: { "Content-Type": "application/json" },
  });
}, { port: 8010, hostname: "127.0.0.1" });

const response = await fetch("http://127.0.0.1:8010/info.json");
const data = await response.json();
console.log(`fetched package: ${data.name} v${data.version} (${data.engine})`);

await server.close();

