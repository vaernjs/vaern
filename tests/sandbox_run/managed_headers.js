import { serve } from "vaern:http";

try {
  await fetch("http://127.0.0.1:9/", { headers: { "Transfer-Encoding": "chunked" } });
  console.log("sent");
} catch (err) {
  console.log(`${err.name}: ${err.message}`);
}

const server = serve(
  () => new Response("hi", { headers: { "Content-Length": "999" } }),
  { port: 0, hostname: "127.0.0.1" },
);
const res = await fetch(`http://127.0.0.1:${server.addr.port}/`);
console.log(res.status);
await server.close();
