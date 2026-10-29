interface User {
  name: string;
  role: string;
}
const u: User = { name: "Vaern", role: "Runtime" };
console.log(`Hello from ${u.name} (${u.role})!`);
