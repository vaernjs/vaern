// Verifies handler exceptions return 500 plain text and server stays up.

import { serve } from "vaern:http";

const server = serve((req: Request) => {
  const url = new URL(req.url);
  if (url.pathname === "/throw") {
    throw new Error("intentional unhandled failure");
  }
  if (url.pathname === "/double-read") {
    return (async () => {
      await req.text();
      await req.text();
      return new Response("read twice succeeded", { status: 200 });
    })();
  }
  return new Response("healthy\n", { status: 200 });
}, { port: 8003, hostname: "127.0.0.1" });

// 1. Request that throws uncaught exception
const res1 = await fetch("http://127.0.0.1:8003/throw");
const body1 = await res1.text();
if (res1.status !== 500 || !body1.includes("Internal Server Error")) {
  console.error(`Expected 500 Internal Server Error, got ${res1.status}: ${body1}`);
  process.exitCode = 1;
}

// 2. Request that attempts double read of request body
const res2 = await fetch("http://127.0.0.1:8003/double-read", {
  method: "POST",
  body: "test body payload",
});
const body2 = await res2.text();
if (res2.status !== 500 || !body2.includes("Internal Server Error")) {
  console.error(`Expected 500 for double body read, got ${res2.status}: ${body2}`);
  process.exitCode = 1;
}

// 3. Subsequent request to verify server stayed up
const res3 = await fetch("http://127.0.0.1:8003/ok");
const body3 = await res3.text();
if (res3.status !== 200 || body3 !== "healthy\n") {
  console.error(`Expected 200 healthy, got ${res3.status}: ${body3}`);
  process.exitCode = 1;
}

await server.close();
console.log("serve_throw passed");
