// Test-only ABI probe: save host registers, seed sentinels, call, verify, restore.
.text
.p2align 2
.globl {probe}
{probe}:
    stp x29, x30, [sp, #-160]!
    mov x29, sp
    stp x19, x20, [sp, #16]
    stp x21, x22, [sp, #32]
    stp x23, x24, [sp, #48]
    stp x25, x26, [sp, #64]
    stp x27, x28, [sp, #80]
    stp d8, d9, [sp, #96]
    stp d10, d11, [sp, #112]
    stp d12, d13, [sp, #128]
    stp d14, d15, [sp, #144]
    mov x19, #275
    mov x20, #276
    mov x21, #277
    mov x22, #278
    mov x23, #279
    mov x24, #280
    mov x25, #281
    mov x26, #282
    mov x27, #283
    mov x28, #284
    mov x9, #264
    fmov d8, x9
    mov x9, #265
    fmov d9, x9
    mov x9, #266
    fmov d10, x9
    mov x9, #267
    fmov d11, x9
    mov x9, #268
    fmov d12, x9
    mov x9, #269
    fmov d13, x9
    mov x9, #270
    fmov d14, x9
    mov x9, #271
    fmov d15, x9
    mov x16, x0
    mov x0, x1
    blr x16
    mov x0, #0
    mov x9, sp
    cmp x29, x9
    b.ne 9f
    tst x9, #15
    b.ne 9f
    cmp x19, #275
    b.ne 9f
    cmp x20, #276
    b.ne 9f
    cmp x21, #277
    b.ne 9f
    cmp x22, #278
    b.ne 9f
    cmp x23, #279
    b.ne 9f
    cmp x24, #280
    b.ne 9f
    cmp x25, #281
    b.ne 9f
    cmp x26, #282
    b.ne 9f
    cmp x27, #283
    b.ne 9f
    cmp x28, #284
    b.ne 9f
    fmov x9, d8
    cmp x9, #264
    b.ne 9f
    fmov x9, d9
    cmp x9, #265
    b.ne 9f
    fmov x9, d10
    cmp x9, #266
    b.ne 9f
    fmov x9, d11
    cmp x9, #267
    b.ne 9f
    fmov x9, d12
    cmp x9, #268
    b.ne 9f
    fmov x9, d13
    cmp x9, #269
    b.ne 9f
    fmov x9, d14
    cmp x9, #270
    b.ne 9f
    fmov x9, d15
    cmp x9, #271
    b.ne 9f
    mov x0, #1
9:
    ldp d8, d9, [sp, #96]
    ldp d10, d11, [sp, #112]
    ldp d12, d13, [sp, #128]
    ldp d14, d15, [sp, #144]
    ldp x19, x20, [sp, #16]
    ldp x21, x22, [sp, #32]
    ldp x23, x24, [sp, #48]
    ldp x25, x26, [sp, #64]
    ldp x27, x28, [sp, #80]
    ldp x29, x30, [sp], #160
    ret
