import { greet } from "linked-greeter";

// linked-greeter is a directory link to greeter, created by the sandbox
// script, the way pnpm lays packages out.
console.log(greet("link"));
