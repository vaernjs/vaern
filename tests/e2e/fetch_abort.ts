// Verifies AbortSignal cancels in-flight fetch and rejects with AbortError.

const controller = new AbortController();
setTimeout(() => {
  controller.abort();
}, 50);

try {
  await fetch("https://127.0.0.1:8443/delay?ms=1000", { signal: controller.signal });
  // If fetch did not abort, set failure exit code
  process.exitCode = 1;
} catch (err: any) {
  if (err && err.name === "AbortError") {
    process.exitCode = 0;
  } else {
    console.error("Expected AbortError name, received:", err);
    process.exitCode = 1;
  }
}
