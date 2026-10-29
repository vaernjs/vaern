// Verifies that redirect chains under 20 are followed, and chains exceeding 20 fail.

// 1. Follow 3-hop redirect to final endpoint
const resSuccess = await fetch("https://127.0.0.1:8443/redirect?count=3");
if (resSuccess.status !== 200) {
  throw new Error(`Expected status 200 after 3 redirects, got ${resSuccess.status}`);
}
if (!resSuccess.url.includes("/test")) {
  throw new Error(`Expected final destination /test, got ${resSuccess.url}`);
}

// 2. Exceed 20-redirect limit with 25 hops
let failedTyped = false;
try {
  await fetch("https://127.0.0.1:8443/redirect?count=25");
} catch (err: any) {
  failedTyped = true;
  const message = String(err).toLowerCase();
  if (!message.includes("redirect")) {
    throw new Error(`Expected redirect error message, got: ${err}`);
  }
}

if (!failedTyped) {
  throw new Error("Expected 25-hop redirect to throw error");
}

console.log("redirect follow and cap enforcement passed");
