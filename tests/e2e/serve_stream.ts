// Verifies chunked streaming response bodies arrive in order.

import { serve } from "vaern:http";

const expectedChunks = ["chunk1-", "chunk2-", "chunk3"];

const server = serve((_req: Request) => {
  const stream = new ReadableStream({
    async start(controller) {
      for (const chunk of expectedChunks) {
        controller.enqueue(new TextEncoder().encode(chunk));
        await new Promise((resolve) => setTimeout(resolve, 20));
      }
      controller.close();
    },
  });

  return new Response(stream, {
    status: 200,
    headers: { "Content-Type": "text/plain; charset=utf-8" },
  });
}, { port: 8001, hostname: "127.0.0.1" });

const res = await fetch("http://127.0.0.1:8001/stream");
if (res.status !== 200) {
  console.error(`Expected status 200, received ${res.status}`);
  process.exitCode = 1;
}

const reader = res.body ? res.body.getReader() : null;
if (!reader) {
  const text = await res.text();
  if (text !== "chunk1-chunk2-chunk3") {
    console.error(`Stream content mismatch: ${text}`);
    process.exitCode = 1;
  }
} else {
  const received: string[] = [];
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    received.push(new TextDecoder().decode(value));
  }
  const fullText = received.join("");
  if (fullText !== "chunk1-chunk2-chunk3") {
    console.error(`Received chunks did not match: ${fullText}`);
    process.exitCode = 1;
  }
}

await server.close();
console.log("serve_stream passed");
