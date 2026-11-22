#ifndef OBS_FIELD_PREVIEW_H
#define OBS_FIELD_PREVIEW_H

#include <stdint.h>
#include <stddef.h>
#include <string.h>

/* One owner per open IIO session. first/end include all real8 markers. */
struct preview_history {
    uint8_t *bytes;
    size_t capacity;
    uint64_t consumed;
    size_t retained;
};
enum preview_result { PREVIEW_READY, PREVIEW_WAIT, PREVIEW_LOST, PREVIEW_INVALID };

static inline int preview_position(const struct preview_history *h, uint32_t raw, uint64_t *absolute)
{
    uint32_t distance=raw-(uint32_t)h->consumed;
    if (distance==UINT32_C(0x80000000)) return -1;
    if (distance<UINT32_C(0x80000000)) {
        if (h->consumed>UINT64_MAX-distance) return -1;
        *absolute=h->consumed+distance;
    } else {
        uint32_t back=0u-distance;
        if (back>h->consumed) return -1;
        *absolute=h->consumed-back;
    }
    return 0;
}

static inline int preview_init(struct preview_history *h, uint8_t *bytes, size_t capacity)
{
    if (!bytes || !capacity || capacity >= UINT32_C(0x80000000)) return -1;
    *h = (struct preview_history){ .bytes=bytes, .capacity=capacity };
    return 0;
}

static inline int preview_append(struct preview_history *h, const uint8_t *data, size_t n)
{
    if (!n) return 0;
    if (!data || n > UINT64_MAX - h->consumed) return -1;
    uint64_t end = h->consumed + n;
    size_t keep = n < h->capacity ? n : h->capacity;
    size_t at = (size_t)((end - keep) % h->capacity);
    size_t first = h->capacity - at;
    if (first > keep) first = keep;
    data += n - keep;
    memcpy(h->bytes + at, data, first);
    memcpy(h->bytes, data + first, keep - first);
    h->consumed = end;
    h->retained = keep >= h->capacity-h->retained ? h->capacity : h->retained+keep;
    return 0;
}

/* Resolve once, while receipt age/lead is known to be less than 2^31 bytes.
 * Persist the resulting first64/end64 if IIO has not returned the suffix yet. */
static inline enum preview_result preview_bounds(const struct preview_history *h,
    uint32_t first, uint32_t end, uint64_t *first64, uint64_t *end64)
{
    uint32_t length = end-first;
    uint64_t e;
    if (!length || length>h->capacity || preview_position(h,end,&e) || e<length)
        return PREVIEW_INVALID;
    *end64=e;
    *first64=e-length;
    return PREVIEW_READY;
}

#endif
