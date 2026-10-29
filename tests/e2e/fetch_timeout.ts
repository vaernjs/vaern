// Verifies fetch against black-hole server fails at timeout with stage=read.

try {
  const response = await fetch("https://127.0.0.1:8443/blackhole", { timeout: 100 });
  await response.text();
  process.exitCode = 1;
} catch (err: any) {
  const message = String(err);
  const stage = err && err.stage;

  if (stage === "read" || message.includes("stage 'read'") || message.includes("stage=read")) {
    process.exitCode = 0;
  } else {
    console.error("Expected timeout error at stage 'read', received:", err);
    process.exitCode = 1;
  }
}
