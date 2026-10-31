// Sums integers up to 50,000,000. Long enough that a JIT pays off, so this is
// the run where an interpreter is expected to lose.
let sum = 0;
for (let i = 0; i < 50_000_000; i++) {
  sum += i;
}
console.log(`sum: ${sum}`);
