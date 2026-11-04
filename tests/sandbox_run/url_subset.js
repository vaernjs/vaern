// URL follows the WHATWG parser for the cases the old regex rejected or mangled.
const auth = new URL("https://user:p%40ss@example.com:8443/a/../b?x=1#top");
console.log(auth.username, auth.password, auth.host, auth.pathname, auth.search, auth.hash);
const v6 = new URL("http://[::1]:3000/");
console.log(v6.hostname, v6.port, v6.origin);
console.log(new URL("../up?q=a b", "https://example.com/dir/sub/page").href);
console.log(new URL("HTTPS://EXAMPLE.com:443/%7Efoo").href);

const u = new URL("https://example.com/?tag=a&tag=b");
u.searchParams.append("q", "x y&z");
u.searchParams.delete("tag", "a");
console.log(u.search, u.searchParams.getAll("tag").join(","), u.searchParams.size);
u.search = "?k=v";
console.log(u.searchParams.get("k"), u.href);
u.pathname = "/new path";
u.port = "99999";
console.log(u.href);
console.log(URL.canParse("not a url"), URL.canParse("/rel", "https://example.com"));
console.log(String(new URLSearchParams({ a: "1", b: "two words" })));
