// Multi-line types above the throw must not shift the reported line.
interface Config {
  name: string;
  retries: number;
}

type Handler = (
  input: string,
) => void;

function fail(config: Config): never {
  throw new Error(`failed ${config.name}`);
}

try {
  fail({ name: "caught", retries: 1 });
} catch (err) {
  const frame = (err as Error).stack?.split("\n").find((l) => l.includes("fail"));
  console.log(frame?.trim());
}
fail({ name: "uncaught", retries: 2 });
