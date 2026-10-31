// Release builds carry no loopback test server and trust no test certificate,
// so this request must fail instead of reaching an in-process server.
try {
  const res = await fetch("https://127.0.0.1:8443/test", { timeout: 2000 });
  console.log(`reached a server: ${res.status}`);
} catch {
  console.log("rejected");
}
