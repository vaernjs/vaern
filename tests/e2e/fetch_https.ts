// Tests basic HTTPS fetch request and prints status code and body length.
const response = await fetch("https://127.0.0.1:8443/test");
const body = await response.text();
console.log(`status: ${response.status}, body length: ${body.length}`);
