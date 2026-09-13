/* Byte-oriented C port of src/crypto/crypto.cpp (wrkzcoin commit 8d89d7bf)
 * over the vendored ref10 crypto-ops.c, so Rust can call every CryptoNote
 * primitive of spec/03-crypto-primitives.md without C++.
 *
 * Every function is deterministic: randomness (nonces, decoy scalars) is
 * supplied by the caller as already-reduced scalars, which keeps the sign
 * paths testable and keeps the CSPRNG on the Rust side.
 *
 * Function bodies mirror crypto.cpp line for line; the crypto.cpp line is
 * cited on each one so a reviewer can diff them. */

#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>

#include "crypto-ops.h"
#include "hash-ops.h"
#include "keccak.h"

#define KEY 32

/* ---- helpers ------------------------------------------------------------ */

void wrkz_cn_fast_hash(const uint8_t *data, size_t len, uint8_t out[KEY])
{
    cn_fast_hash(data, len, (char *)out);
}

/* crypto.cpp:44 */
static void hash_to_scalar(const void *data, size_t len, uint8_t out[KEY])
{
    cn_fast_hash(data, len, (char *)out);
    sc_reduce32(out);
}

void wrkz_hash_to_scalar(const uint8_t *data, size_t len, uint8_t out[KEY])
{
    hash_to_scalar(data, len, out);
}

void wrkz_sc_reduce32(uint8_t s[KEY])
{
    sc_reduce32(s);
}

/* random_scalar, crypto.cpp:36: 64 random bytes reduced mod l */
void wrkz_scalar_from_64_bytes(const uint8_t rnd[64], uint8_t out[KEY])
{
    uint8_t tmp[64];
    memcpy(tmp, rnd, 64);
    sc_reduce(tmp);
    memcpy(out, tmp, KEY);
}

int wrkz_sc_check(const uint8_t s[KEY])
{
    return sc_check(s);
}

void wrkz_sc_add(uint8_t r[KEY], const uint8_t a[KEY], const uint8_t b[KEY]) { sc_add(r, a, b); }
void wrkz_sc_sub(uint8_t r[KEY], const uint8_t a[KEY], const uint8_t b[KEY]) { sc_sub(r, a, b); }
void wrkz_sc_mul(uint8_t r[KEY], const uint8_t a[KEY], const uint8_t b[KEY]) { sc_mul(r, a, b); }
/* r = c - a*b */
void wrkz_sc_mulsub(uint8_t r[KEY], const uint8_t a[KEY], const uint8_t b[KEY], const uint8_t c[KEY]) { sc_mulsub(r, a, b, c); }

/* Tools::write_varint, common/Varint.h */
static size_t write_varint(uint8_t *dst, uint64_t v)
{
    size_t n = 0;
    while (v >= 0x80)
    {
        dst[n++] = (uint8_t)((v & 0x7f) | 0x80);
        v >>= 7;
    }
    dst[n++] = (uint8_t)v;
    return n;
}

/* ---- keys --------------------------------------------------------------- */

/* crypto.cpp:97 */
int wrkz_check_key(const uint8_t pub[KEY])
{
    ge_p3 point;
    return ge_frombytes_vartime(&point, pub) == 0;
}

/* crypto.cpp:103 */
int wrkz_secret_key_to_public_key(const uint8_t sec[KEY], uint8_t pub[KEY])
{
    ge_p3 point;
    if (sc_check(sec) != 0)
    {
        return 0;
    }
    ge_scalarmult_base(&point, sec);
    ge_p3_tobytes(pub, &point);
    return 1;
}

/* crypto.cpp:68 */
void wrkz_generate_deterministic_keys(const uint8_t seed[KEY], uint8_t sec[KEY], uint8_t pub[KEY])
{
    ge_p3 point;
    memcpy(sec, seed, KEY);
    sc_reduce32(sec);
    ge_scalarmult_base(&point, sec);
    ge_p3_tobytes(pub, &point);
}

/* crypto.cpp:719 */
void wrkz_generate_view_from_spend(const uint8_t spend[KEY], uint8_t view_sec[KEY], uint8_t view_pub[KEY])
{
    uint8_t seed[KEY];
    keccak(spend, KEY, seed, KEY);
    wrkz_generate_deterministic_keys(seed, view_sec, view_pub);
}

/* crypto.cpp:731 */
void wrkz_generate_deterministic_subwallet_key(const uint8_t base[KEY], uint64_t index, uint8_t out[KEY])
{
    uint8_t tmp[40];
    uint8_t salt[8];
    uint64_t i;
    for (i = 0; i < 8; i++)
    {
        salt[i] = (uint8_t)(index >> (8 * i));
    }
    memcpy(tmp, base, KEY);
    memset(tmp + KEY, 0, 8);
    for (i = 0; i < index; i++)
    {
        uint8_t h[KEY];
        memcpy(tmp + KEY, salt, 8);
        cn_fast_hash(tmp, 40, (char *)h);
        memcpy(tmp, h, KEY);
    }
    memcpy(out, tmp, KEY);
    sc_reduce32(out);
}

/* ---- output derivation -------------------------------------------------- */

/* crypto.cpp:115 : D = 8 * sec * pub */
int wrkz_generate_key_derivation(const uint8_t pub[KEY], const uint8_t sec[KEY], uint8_t derivation[KEY])
{
    ge_p3 point;
    ge_p2 point2;
    ge_p1p1 point3;
    if (ge_frombytes_vartime(&point, pub) != 0)
    {
        return 0;
    }
    ge_scalarmult(&point2, sec, &point);
    ge_mul8(&point3, &point2);
    ge_p1p1_to_p2(&point2, &point3);
    ge_tobytes(derivation, &point2);
    return 1;
}

/* crypto.cpp:133 : Hs(D || varint(i)) */
void wrkz_derivation_to_scalar(const uint8_t derivation[KEY], uint64_t output_index, uint8_t out[KEY])
{
    uint8_t buf[KEY + 10];
    size_t n;
    memcpy(buf, derivation, KEY);
    n = write_varint(buf + KEY, output_index);
    hash_to_scalar(buf, KEY + n, out);
}

/* crypto.cpp:169 : P = Hs(D||i)*G + B */
int wrkz_derive_public_key(const uint8_t derivation[KEY], uint64_t output_index, const uint8_t base[KEY], uint8_t out[KEY])
{
    uint8_t scalar[KEY];
    ge_p3 point1;
    ge_p3 point2;
    ge_cached point3;
    ge_p1p1 point4;
    ge_p2 point5;
    if (ge_frombytes_vartime(&point1, base) != 0)
    {
        return 0;
    }
    wrkz_derivation_to_scalar(derivation, output_index, scalar);
    ge_scalarmult_base(&point2, scalar);
    ge_p3_to_cached(&point3, &point2);
    ge_add(&point4, &point1, &point3);
    ge_p1p1_to_p2(&point5, &point4);
    ge_tobytes(out, &point5);
    return 1;
}

/* crypto.cpp:254 : x = Hs(D||i) + b */
void wrkz_derive_secret_key(const uint8_t derivation[KEY], uint64_t output_index, const uint8_t base[KEY], uint8_t out[KEY])
{
    uint8_t scalar[KEY];
    wrkz_derivation_to_scalar(derivation, output_index, scalar);
    sc_add(out, base, scalar);
}

/* crypto.cpp:294 : B = P - Hs(D||i)*G */
int wrkz_underive_public_key(const uint8_t derivation[KEY], uint64_t output_index, const uint8_t derived[KEY], uint8_t out[KEY])
{
    uint8_t scalar[KEY];
    ge_p3 point1;
    ge_p3 point2;
    ge_cached point3;
    ge_p1p1 point4;
    ge_p2 point5;
    if (ge_frombytes_vartime(&point1, derived) != 0)
    {
        return 0;
    }
    wrkz_derivation_to_scalar(derivation, output_index, scalar);
    ge_scalarmult_base(&point2, scalar);
    ge_p3_to_cached(&point3, &point2);
    ge_sub(&point4, &point1, &point3);
    ge_p1p1_to_p2(&point5, &point4);
    ge_tobytes(out, &point5);
    return 1;
}

/* ---- hash to point, key images ----------------------------------------- */

/* crypto.cpp:418 */
static void hash_to_ec(const uint8_t key[KEY], ge_p3 *res)
{
    uint8_t h[KEY];
    ge_p2 point;
    ge_p1p1 point2;
    cn_fast_hash(key, KEY, (char *)h);
    ge_fromfe_frombytes_vartime(&point, h);
    ge_mul8(&point2, &point);
    ge_p1p1_to_p3(res, &point2);
}

/* crypto.cpp:441 */
void wrkz_hash_data_to_ec(const uint8_t *data, size_t len, uint8_t out[KEY])
{
    uint8_t h[KEY];
    ge_p2 point;
    ge_p1p1 point2;
    cn_fast_hash(data, len, (char *)h);
    ge_fromfe_frombytes_vartime(&point, h);
    ge_mul8(&point2, &point);
    ge_p1p1_to_p2(&point, &point2);
    ge_tobytes(out, &point);
}

/* crypto.cpp:453 : I = x * Hp(P) */
void wrkz_generate_key_image(const uint8_t pub[KEY], const uint8_t sec[KEY], uint8_t out[KEY])
{
    ge_p3 point;
    ge_p2 point2;
    hash_to_ec(pub, &point);
    ge_scalarmult(&point2, sec, &point);
    ge_tobytes(out, &point2);
}

/* crypto.cpp:429 : a*P with no canonicality checks (used for the l*I == identity test).
 *
 * The original ignores the ge_frombytes_vartime return, which leaves `A`
 * uninitialised when P does not decompress (crypto-ops.c:1409 returns before
 * touching h, and the later failure paths leave h->T unset): the C++ then
 * multiplies uninitialised stack. This shim reports the failure instead. The
 * caller of the domain check (key_image_in_prime_subgroup) turns that into
 * "not in the prime order subgroup", which is what the C++ produces in
 * practice - a garbage point is not going to encode the identity - only
 * deterministically. Returns 1 on success, 0 if P does not decompress. */
int wrkz_scalarmult_key(const uint8_t P[KEY], const uint8_t a[KEY], uint8_t out[KEY])
{
    ge_p3 A;
    ge_p2 R;
    if (ge_frombytes_vartime(&A, P) != 0)
    {
        memset(out, 0, KEY);
        return 0;
    }
    ge_scalarmult(&R, a, &A);
    ge_tobytes(out, &R);
    return 1;
}

/* ---- plain signatures --------------------------------------------------- */

/* crypto.cpp:354, with k supplied (random_scalar in the original) */
void wrkz_generate_signature(const uint8_t prefix_hash[KEY], const uint8_t pub[KEY], const uint8_t sec[KEY], const uint8_t k[KEY], uint8_t sig[64])
{
    ge_p3 tmp3;
    uint8_t buf[96];
    memcpy(buf, prefix_hash, KEY);
    memcpy(buf + KEY, pub, KEY);
    ge_scalarmult_base(&tmp3, k);
    ge_p3_tobytes(buf + 2 * KEY, &tmp3);
    hash_to_scalar(buf, 96, sig);
    sc_mulsub(sig + KEY, sig, sec, k);
}

/* crypto.cpp:386 */
int wrkz_check_signature(const uint8_t prefix_hash[KEY], const uint8_t pub[KEY], const uint8_t sig[64])
{
    ge_p2 tmp2;
    ge_p3 tmp3;
    uint8_t c[KEY];
    uint8_t buf[96];
    memcpy(buf, prefix_hash, KEY);
    memcpy(buf + KEY, pub, KEY);
    if (ge_frombytes_vartime(&tmp3, pub) != 0)
    {
        return 0;
    }
    if (sc_check(sig) != 0 || sc_check(sig + KEY) != 0)
    {
        return 0;
    }
    ge_double_scalarmult_base_vartime(&tmp2, sig, &tmp3, sig + KEY);
    ge_tobytes(buf + 2 * KEY, &tmp2);
    hash_to_scalar(buf, 96, c);
    sc_sub(c, c, sig);
    return sc_isnonzero(c) == 0;
}

/* ---- ring signatures ---------------------------------------------------- */

/* rs_comm layout (crypto.cpp:466): prefix_hash || (a_i || b_i) * n, 32 + 64n bytes */

/* crypto.cpp:485 prepareRingSignatures + crypto.cpp:578 completeRingSignatures,
 * fused. `sigs` (64*n bytes) must arrive with random reduced scalars in every
 * entry except `real`; those are the (c_i, r_i) decoy values. `k` is the
 * signer's random nonce. Returns 0 if the image or a ring member does not
 * decompress. */
int wrkz_generate_ring_signature(
    const uint8_t prefix_hash[KEY],
    const uint8_t image[KEY],
    const uint8_t *pubs,
    size_t n,
    const uint8_t sec[KEY],
    uint64_t real,
    const uint8_t k[KEY],
    uint8_t *sigs)
{
    ge_p3 image_unp;
    ge_dsmp image_pre;
    uint8_t sum[KEY], h[KEY];
    uint8_t *buf;
    size_t i;
    if (n == 0 || real >= n)
    {
        return 0;
    }
    buf = (uint8_t *)malloc(KEY + 64 * n);
    if (!buf)
    {
        return 0;
    }
    if (ge_frombytes_vartime(&image_unp, image) != 0)
    {
        free(buf);
        return 0;
    }
    ge_dsm_precomp(image_pre, &image_unp);
    sc_0(sum);
    memcpy(buf, prefix_hash, KEY);
    for (i = 0; i < n; i++)
    {
        ge_p2 tmp2;
        ge_p3 tmp3;
        uint8_t *a = buf + KEY + 64 * i;
        uint8_t *b = a + KEY;
        uint8_t *sig = sigs + 64 * i;
        if (i == real)
        {
            ge_scalarmult_base(&tmp3, k);
            ge_p3_tobytes(a, &tmp3);
            hash_to_ec(pubs + KEY * i, &tmp3);
            ge_scalarmult(&tmp2, k, &tmp3);
            ge_tobytes(b, &tmp2);
        }
        else
        {
            if (ge_frombytes_vartime(&tmp3, pubs + KEY * i) != 0)
            {
                free(buf);
                return 0;
            }
            ge_double_scalarmult_base_vartime(&tmp2, sig, &tmp3, sig + KEY);
            ge_tobytes(a, &tmp2);
            hash_to_ec(pubs + KEY * i, &tmp3);
            ge_double_scalarmult_precomp_vartime(&tmp2, sig + KEY, &tmp3, sig, image_pre);
            ge_tobytes(b, &tmp2);
            sc_add(sum, sum, sig);
        }
    }
    hash_to_scalar(buf, KEY + 64 * n, h);
    sc_sub(sigs + 64 * real, h, sum);
    /* completeRingSignatures: r_s = k - c_s * x */
    sc_mulsub(sigs + 64 * real + KEY, sigs + 64 * real, sec, k);
    free(buf);
    return 1;
}

/* crypto.cpp:631 */
int wrkz_check_ring_signature(
    const uint8_t prefix_hash[KEY],
    const uint8_t image[KEY],
    const uint8_t *pubs,
    size_t n,
    const uint8_t *sigs)
{
    ge_p3 image_unp;
    ge_dsmp image_pre;
    uint8_t sum[KEY], h[KEY];
    uint8_t *buf;
    size_t i;
    int ok;
    if (ge_frombytes_vartime(&image_unp, image) != 0)
    {
        return 0;
    }
    ge_dsm_precomp(image_pre, &image_unp);
    if (ge_check_subgroup_precomp_vartime(image_pre) != 0)
    {
        return 0;
    }
    buf = (uint8_t *)malloc(KEY + 64 * n);
    if (!buf)
    {
        return 0;
    }
    sc_0(sum);
    memcpy(buf, prefix_hash, KEY);
    for (i = 0; i < n; i++)
    {
        ge_p2 tmp2;
        ge_p3 tmp3;
        uint8_t *a = buf + KEY + 64 * i;
        uint8_t *b = a + KEY;
        const uint8_t *sig = sigs + 64 * i;
        if (sc_check(sig) != 0 || sc_check(sig + KEY) != 0)
        {
            free(buf);
            return 0;
        }
        if (ge_frombytes_vartime(&tmp3, pubs + KEY * i) != 0)
        {
            free(buf);
            return 0;
        }
        ge_double_scalarmult_base_vartime(&tmp2, sig, &tmp3, sig + KEY);
        ge_tobytes(a, &tmp2);
        hash_to_ec(pubs + KEY * i, &tmp3);
        ge_double_scalarmult_precomp_vartime(&tmp2, sig + KEY, &tmp3, sig, image_pre);
        ge_tobytes(b, &tmp2);
        sc_add(sum, sum, sig);
    }
    hash_to_scalar(buf, KEY + 64 * n, h);
    sc_sub(h, h, sum);
    ok = sc_isnonzero(h) == 0;
    free(buf);
    return ok;
}

