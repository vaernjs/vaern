// audit-demo.ts
//
// M2 milestone preview: full deterministic journal recording lands in M2.
// This example displays the schema emitted by `vaern audit export`.

const auditExport = {
  version: "1.0.0",
  runId: "7f3a9e21",
  timestamp: "2026-10-29T18:00:00Z",
  engine: "vaern-0.1.0",
  boundaryEffects: [
    { type: "network", target: "127.0.0.1:8000", action: "listen" },
    { type: "crypto", algorithm: "ring-ed25519", action: "sign" },
  ],
  status: "verified",
};

console.log(JSON.stringify(auditExport, null, 2));

