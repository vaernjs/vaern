import { readFile } from "vaern:fs";

// Both files are created by the sandbox script: one at the read cap, one a
// byte over it.
const exact: Uint8Array = await readFile("out/exact.bin");
console.log(exact.length);

try {
  await readFile("out/over.bin");
  console.log("no error");
} catch (err: any) {
  console.log(err.name, err.code);
}
