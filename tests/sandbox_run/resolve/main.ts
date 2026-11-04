import { greet } from "greeter";
import { version } from "greeter/version";
import { add } from "@fixture/math";

console.log(greet("vaern"), version, add(2, 3));

try {
  await import("greeter/internal");
  console.log("imported");
} catch (err: any) {
  console.log(String(err.message).includes("subpath './internal' is not exported"));
}
