// Allocates a lot of short-lived garbage. If the collector reclaims nothing, peak RSS
// grows without bound.
let sink = 0;
for (let i = 0; i < 200000; i++) {
  const o = { a: i, b: { c: i + 1 }, d: [i, i + 1, i + 2] };
  sink += o.b.c + o.d[2];
}
console.log("ok", sink);
