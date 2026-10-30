// HTTP echo server demonstrating route handling and body reading.
import { serve } from "vaern:http";

const server = serve(async (req: Request) => {
  const url = new URL(req.url);

  if (url.pathname === "/echo") {
    const body = await req.text();
    return new Response(body || "echo\n", {
      status: 200,
      headers: { "Content-Type": "text/plain; charset=utf-8" },
    });
  }

  return new Response(`Echo server ready on ${url.pathname}\n`, {
    status: 200,
    headers: { "Content-Type": "text/plain; charset=utf-8" },
  });
}, { port: 8000, hostname: "127.0.0.1" });

console.log(`Echo server running at http://${server.addr.hostname}:${server.addr.port}/`);

if (process.argv.includes("--test")) {
  const res = await fetch("http://127.0.0.1:8000/echo", {
    method: "POST",
    body: "hello vaern",
  });
  const text = await res.text();
  console.log(`echo test response: ${text}`);
  await server.close();
}
