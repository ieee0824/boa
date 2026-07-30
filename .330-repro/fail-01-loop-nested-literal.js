let s = 0;
for (let i = 0; i < 50; i++) { const o = { a: i, b: { c: i } }; s += o.b.c; }
console.log(s);
