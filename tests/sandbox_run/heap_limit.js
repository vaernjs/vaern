// Allocating past the heap limit fails with a reportable error, not a host OOM.
const held = [];
for (let i = 0; i < 384; i++) held.push(new Uint8Array(1024 * 1024));
console.log("allocated", held.length);
