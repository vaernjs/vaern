// Verifies case-insensitive header access in both directions.

// 1. Client-side Headers class normalisation
const headers = new Headers({
  "Content-Type": "application/json",
  "X-Custom-Foo": "initial",
});

if (headers.get("content-type") !== "application/json") {
  throw new Error("Headers get lowercase failed");
}
if (headers.get("CONTENT-TYPE") !== "application/json") {
  throw new Error("Headers get uppercase failed");
}

headers.set("x-custom-FOO", "updated");
if (headers.get("X-CUSTOM-FOO") !== "updated") {
  throw new Error("Headers set case overwrite failed");
}

let visitedKey = false;
headers.forEach((value, key) => {
  if (key.toLowerCase() === "x-custom-foo" && value === "updated") {
    visitedKey = true;
  }
});
if (!visitedKey) {
  throw new Error("Headers forEach iteration failed");
}

// 2. Server wire response header normalisation
const response = await fetch("https://127.0.0.1:8443/echo-headers", {
  headers: {
    "X-Request-Case": "Preserved",
  },
});

const lower = response.headers.get("x-response-server");
const upper = response.headers.get("X-RESPONSE-SERVER");

if (!lower || lower !== upper) {
  throw new Error("Response headers case-normalisation failed");
}

console.log("headers case-normalisation passed");
