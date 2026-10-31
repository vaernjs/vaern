// An HTTP server that receives no requests, then closes. Used by
// bench/idle-cpu.mjs to measure what an idle runtime costs.
import { serve } from "vaern:http";

const seconds = Number(process.argv[2] ?? "5");
const server = serve(() => new Response("ok"), { port: 0, hostname: "127.0.0.1" });
setTimeout(() => server.close(), seconds * 1000);
