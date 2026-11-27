#include <stddef.h>
#include <stdint.h>
#if defined(__wasm__)
void *memcpy(void *, const void *, size_t);
void *memset(void *, int, size_t);
#else
#include <string.h>
#endif

#if defined(__ARM_NEON)
#include <arm_neon.h>
#endif

enum { group = 32 };
#if defined(_WIN32)
#define EXPORT __declspec(dllexport)
#else
#define EXPORT
#endif

static void range(const uint8_t *input, int *lo, int *hi)
{
#if defined(__ARM_NEON)
    const int8x16_t a = vld1q_s8((const int8_t *)input);
    const int8x16_t b = vld1q_s8((const int8_t *)input + 16);
    const int8x16_t min16 = vminq_s8(a, b);
    const int8x16_t max16 = vmaxq_s8(a, b);
    int8x8_t mn = vpmin_s8(vget_low_s8(min16), vget_high_s8(min16));
    int8x8_t mx = vpmax_s8(vget_low_s8(max16), vget_high_s8(max16));
    for (int i = 0; i < 3; ++i) {
        mn = vpmin_s8(mn, mn);
        mx = vpmax_s8(mx, mx);
    }
    *lo = vget_lane_s8(mn, 0);
    *hi = vget_lane_s8(mx, 0);
#else
    int mn = (int8_t)*input, mx = mn;
    for (int i = 1; i < group; ++i) {
        const int x = (int8_t)input[i];
        if (x < mn) mn = x;
        if (x > mx) mx = x;
    }
    *lo = mn;
    *hi = mx;
#endif
}

#define PACK_CASE(BITS) \
    case BITS: \
        for (int i = 0; i < group; i += 8) { \
            uint32_t a = 0, b = 0; \
            for (int j = 0; j < 4; ++j) { \
                a |= (uint32_t)(uint8_t)(input[i + j] - base) << (j * BITS); \
                b |= (uint32_t)(uint8_t)(input[i + 4 + j] - base) << (j * BITS); \
            } \
            const uint64_t packed = a | (uint64_t)b << (4 * BITS); \
            for (int j = 0; j < BITS; ++j) *out++ = packed >> (8 * j); \
        } \
        break

/* Return zero when the input is better sent raw. A short tail stays raw. */
EXPORT size_t real8_pack(const uint8_t *input, size_t size, uint8_t *dst)
{
    if (size < group) return 0;
    uint8_t *out = dst;
    const uint8_t *end = input + size - size % group;
    const uint8_t *limit = dst + size - size % group;
    for (; input != end; input += group) {
        int lo, hi;
        range(input, &lo, &hi);
        const unsigned spread = (unsigned)(hi - lo);
        const unsigned bits = spread ? 32u - (unsigned)__builtin_clz(spread) : 0;
        const size_t bytes = bits == 8 ? 1 + group : 2 + 4 * bits;
        if ((size_t)(limit - out) <= bytes) return 0;
        *out++ = bits;
        if (bits == 8) {
            memcpy(out, input, group);
            out += group;
            continue;
        }
        const uint8_t base = (uint8_t)lo;
        *out++ = base;
        switch (bits) {
            case 0: break;
            PACK_CASE(1);
            PACK_CASE(2);
            PACK_CASE(3);
            PACK_CASE(4);
            PACK_CASE(5);
            PACK_CASE(6);
            PACK_CASE(7);
        }
    }
    memcpy(out, input, size % group);
    return (size_t)(out - dst) + size % group;
}

EXPORT int real8_unpack(const uint8_t *input, size_t used, uint8_t *out, size_t size)
{
    const uint8_t *limit = input + used;
    const uint8_t *end = out + size - size % group;
    for (; out != end; out += group) {
        if (input == limit) return 0;
        const unsigned bits = *input++;
        if (bits > 8 || (size_t)(limit - input) < (bits == 8 ? group : 1 + 4 * bits)) return 0;
        if (bits == 8) {
            memcpy(out, input, group);
            input += group;
            continue;
        }
        const unsigned base = *input++;
        const unsigned mask = (1u << bits) - 1;
        if (!bits) {
            memset(out, (int)base, group);
            continue;
        }
        uint32_t acc = 0;
        unsigned avail = 0;
        for (int i = 0; i < group; ++i) {
            if (avail < bits) {
                acc |= (uint32_t)*input++ << avail;
                avail += 8;
            }
            out[i] = (uint8_t)(base + (acc & mask));
            acc >>= bits;
            avail -= bits;
        }
    }
    if ((size_t)(limit - input) != size % group) return 0;
    memcpy(out, input, size % group);
    return 1;
}
