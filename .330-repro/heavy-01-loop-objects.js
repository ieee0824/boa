function make(n) { return { a: n, b: [n, n + 1], c: "x".repeat(8) }; }
let acc = 0;
for (let i = 0; i < 2000; i++) {
  const o = make(i);
  acc += o.a + o.b[1] + o.c.length;
}
console.log("ok", acc);
