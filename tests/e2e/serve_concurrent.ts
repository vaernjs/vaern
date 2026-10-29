// Verifies concurrency under 100 parallel client requests with p99 < 500ms.

import { serve } from "vaern:http";

const server = serve(async (_req: Request) => {
  await new Promise((resolve) => setTimeout(resolve, 5));
  return new Response("concurrent ok\n", {
    status: 200,
    headers: { "Content-Type": "text/plain" },
  });
}, { port: 8004, hostname: "127.0.0.1" });

const clientCount = 100;
const startAll = Date.now();

const tasks = Array.from({ length: clientCount }, async (_, i) => {
  const reqStart = Date.now();
  const res = await fetch(`http://127.0.0.1:8004/test?client=${i}`);
  const body = await res.text();
  const duration = Date.now() - reqStart;
  if (res.status !== 200 || body !== "concurrent ok\n") {
    throw new Error(`Client ${i} failed: status=${res.status}`);
  }
  return duration;
});

const durations = await Promise.all(tasks);
durations.sort((a, b) => a - b);

const p99Index = Math.floor(durations.length * 0.99);
const p99 = durations[p99Index];
const totalTime = Date.now() - startAll;

console.log(`100 parallel clients completed: p99 = ${p99}ms (total: ${totalTime}ms)`);

if (p99 >= 500) {
  console.error(`p99 exceeded threshold: ${p99}ms >= 500ms`);
  process.exitCode = 1;
}

await server.close();
