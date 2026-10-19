// SPDX-License-Identifier: Apache-2.0
//
// SHA-256 of every fixed-size part of a device buffer, one thread per part. Compiled at runtime
// with NVRTC (see gpu.rs). The composite of the part digests is what S3 stores as a multipart
// object's ChecksumSHA256, so a GPU-resident buffer can be checked against S3 itself.

typedef unsigned int u32;
typedef unsigned long long u64;
typedef unsigned char u8;

__constant__ u32 K[64] = {
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2};

__device__ __forceinline__ u32 rotr(u32 x, u32 n) { return (x >> n) | (x << (32 - n)); }

__device__ void compress(u32 h[8], const u8 *b) {
    u32 w[64];
    for (int i = 0; i < 16; i++)
        w[i] = ((u32)b[4 * i] << 24) | ((u32)b[4 * i + 1] << 16) | ((u32)b[4 * i + 2] << 8) | (u32)b[4 * i + 3];
    for (int i = 16; i < 64; i++) {
        u32 s0 = rotr(w[i - 15], 7) ^ rotr(w[i - 15], 18) ^ (w[i - 15] >> 3);
        u32 s1 = rotr(w[i - 2], 17) ^ rotr(w[i - 2], 19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16] + s0 + w[i - 7] + s1;
    }
    u32 a = h[0], bb = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g = h[6], hh = h[7];
    for (int i = 0; i < 64; i++) {
        u32 S1 = rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25);
        u32 ch = (e & f) ^ (~e & g);
        u32 t1 = hh + S1 + ch + K[i] + w[i];
        u32 S0 = rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22);
        u32 mj = (a & bb) ^ (a & c) ^ (bb & c);
        u32 t2 = S0 + mj;
        hh = g; g = f; f = e; e = d + t1; d = c; c = bb; bb = a; a = t1 + t2;
    }
    h[0] += a; h[1] += bb; h[2] += c; h[3] += d; h[4] += e; h[5] += f; h[6] += g; h[7] += hh;
}

extern "C" __global__ void sha256_parts(const u8 *data, u64 len, u64 part, u64 nparts, u8 *out) {
    u64 i = (u64)blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= nparts) return;
    u64 start = i * part;
    u64 n = len - start < part ? len - start : part;
    const u8 *p = data + start;
    u32 h[8] = {0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19};
    u64 full = n / 64;
    for (u64 k = 0; k < full; k++) compress(h, p + 64 * k);
    u8 tail[128];
    u64 r = n - 64 * full;
    for (u64 k = 0; k < r; k++) tail[k] = p[64 * full + k];
    tail[r] = 0x80;
    u64 tlen = r + 1 + 8 <= 64 ? 64 : 128;
    for (u64 k = r + 1; k < tlen - 8; k++) tail[k] = 0;
    u64 bits = n * 8;
    for (int k = 0; k < 8; k++) tail[tlen - 1 - k] = (u8)(bits >> (8 * k));
    compress(h, tail);
    if (tlen == 128) compress(h, tail + 64);
    for (int k = 0; k < 8; k++) {
        out[32 * i + 4 * k] = (u8)(h[k] >> 24);
        out[32 * i + 4 * k + 1] = (u8)(h[k] >> 16);
        out[32 * i + 4 * k + 2] = (u8)(h[k] >> 8);
        out[32 * i + 4 * k + 3] = (u8)h[k];
    }
}

// Many parts at once, anywhere in device memory: thread i hashes lens[i] bytes at starts[i]. One
// launch verifies a whole wave of objects (thousands of parts) instead of one launch per object.
extern "C" __global__ void sha256_segs(const u64 *starts, const u64 *lens, u64 nparts, u8 *out) {
    u64 i = (u64)blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= nparts) return;
    const u8 *p = (const u8 *)starts[i];
    u64 n = lens[i];
    u32 h[8] = {0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19};
    u64 full = n / 64;
    for (u64 k = 0; k < full; k++) compress(h, p + 64 * k);
    u8 tail[128];
    u64 r = n - 64 * full;
    for (u64 k = 0; k < r; k++) tail[k] = p[64 * full + k];
    tail[r] = 0x80;
    u64 tlen = r + 1 + 8 <= 64 ? 64 : 128;
    for (u64 k = r + 1; k < tlen - 8; k++) tail[k] = 0;
    u64 bits = n * 8;
    for (int k = 0; k < 8; k++) tail[tlen - 1 - k] = (u8)(bits >> (8 * k));
    compress(h, tail);
    if (tlen == 128) compress(h, tail + 64);
    for (int k = 0; k < 8; k++) {
        out[32 * i + 4 * k] = (u8)(h[k] >> 24);
        out[32 * i + 4 * k + 1] = (u8)(h[k] >> 16);
        out[32 * i + 4 * k + 2] = (u8)(h[k] >> 8);
        out[32 * i + 4 * k + 3] = (u8)h[k];
    }
}
