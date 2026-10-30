// Demonstrates asynchronous event loop scheduling using timers.
let ticks = 0;

const intervalId = setInterval(() => {
  ticks += 1;
  console.log(`interval tick: ${ticks}`);
  if (ticks >= 2) {
    clearInterval(intervalId);
  }
}, 20);

setTimeout(() => {
  console.log("timeout fired");
}, 10);

