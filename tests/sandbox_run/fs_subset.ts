import { appendFile, readFile, writeFile } from "vaern:fs";

const file: string = "out/notes.txt";
await writeFile(file, "line one\n");
await appendFile(file, new TextEncoder().encode("line two\n"));
const text: string = await readFile(file, "utf8");
const bytes: Uint8Array = await readFile(file);
console.log(JSON.stringify(text), bytes.length);

const attempts: Array<[string, () => Promise<unknown>]> = [
  ["out/notes.txt", () => writeFile(file, "again", { flag: "wx" })],
  ["out/missing.txt", () => readFile("out/missing.txt")],
  ["out", () => readFile("out")],
];
for (const [target, attempt] of attempts) {
  try {
    await attempt();
    console.log("no error");
  } catch (err: any) {
    const absolute: boolean = err.path.replaceAll("\\", "/").endsWith(`/tests/sandbox_run/${target}`);
    console.log(err.name, err.stage, err.code, absolute);
  }
}
