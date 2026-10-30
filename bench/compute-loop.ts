// Computes sum of integers up to 1,000,000 to benchmark raw arithmetic loop execution.
let sum = 0;
for (let i = 0; i < 1_000_000; i++) {
  sum += i;
}
console.log(`sum: ${sum}`);

