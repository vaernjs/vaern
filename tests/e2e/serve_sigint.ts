// Verifies server drains in-flight requests cleanly during shutdown.

import { serve } from "vaern:http";

let completed = false;

const server = serve(async (req: Request) => {
  const url = new URL(req.url);
  if (url.pathname === "/slow") {
    await new Promise((resolve) => setTimeout(resolve, 2000));
    completed = true;
    return new Response("drained successfully\n", { status: 200 });
  }
  return new Response("ok\n", { status: 200 });
}, { port: 8002, hostname: "127.0.0.1" });

const slowPromise = fetch("http://127.0.0.1:8002/slow").then(async (res) => {
  const text = await res.text();
  return { status: res.status, text };
});

setTimeout(async () => {
  await server.close();
}, 500);

const result = await slowPromise;
if (result.status === 200 && result.text === "drained successfully\n" && completed) {
  console.log("serve_sigint drained, exit 0");
  process.exitCode = 0;
} else {
  console.error("Drain failed:", result);
  process.exitCode = 1;
}
