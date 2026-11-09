// Math.random from a seed the runtime records in the journal's run.start,
// so a replay draws the same numbers. xoshiro128** over four 32-bit words;
// each result takes 53 bits from two outputs. Not for cryptography, like
// the generator it replaces. Until the runtime seeds it (only when the run
// is journalled), the engine's own generator stays in place.
(() => {
    const rotl = (x, k) => (x << k) | (x >>> (32 - k));
    globalThis.__vaern_seed_random = (s0, s1, s2, s3) => {
        let a = s0 | 0;
        let b = s1 | 0;
        let c = s2 | 0;
        let d = s3 | 0;
        if ((a | b | c | d) === 0) a = 1;
        const next = () => {
            const result = Math.imul(rotl(Math.imul(b, 5), 7), 9) >>> 0;
            const t = b << 9;
            c ^= a;
            d ^= b;
            b ^= c;
            a ^= d;
            c ^= t;
            d = rotl(d, 11);
            return result;
        };
        Math.random = function random() {
            const high = next() >>> 5;
            const low = next() >>> 6;
            return (high * 67108864 + low) / 9007199254740992;
        };
        delete globalThis.__vaern_seed_random;
    };
})();
