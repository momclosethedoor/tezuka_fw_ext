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
    uint32_t distance = end-(uint32_t)h->consumed;
    if (!length || length > h->capacity || distance == UINT32_C(0x80000000))
        return PREVIEW_INVALID;
    uint64_t e;
    if (distance < UINT32_C(0x80000000)) {
        if (h->consumed > UINT64_MAX-distance) return PREVIEW_INVALID;
        e = h->consumed+distance;
    } else {
        uint32_t back = 0u-distance;
        if (back > h->consumed) return PREVIEW_INVALID;
        e = h->consumed-back;
    }
    if (e < length) return PREVIEW_INVALID;
    *end64=e;
    *first64=e-length;
    return PREVIEW_READY;
}

static inline enum preview_result preview_copy(const struct preview_history *h,
    uint64_t first, uint64_t end, uint8_t *out, size_t capacity)
{
    if (first >= end || end-first > capacity || end-first > h->capacity || !out)
        return PREVIEW_INVALID;
    if (first < h->consumed-h->retained) return PREVIEW_LOST;
    if (end > h->consumed) return PREVIEW_WAIT;
    size_t n = (size_t)(end-first);
    size_t at = (size_t)(first % h->capacity);
    size_t head = h->capacity-at;
    if (head > n) head=n;
    memcpy(out,h->bytes+at,head);
    memcpy(out+head,h->bytes,n-head);
    return PREVIEW_READY;
}
#endif
