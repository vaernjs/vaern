let step = 0;

const intervalId = setInterval(() => {
  step += 1;
  console.log(`interval: ${step}`);
  if (step >= 2) {
    clearInterval(intervalId);
  }
}, 20);

setTimeout(() => {
  console.log("timeout 1");
}, 10);

setTimeout(() => {
  console.log("timeout 2");
}, 30);
