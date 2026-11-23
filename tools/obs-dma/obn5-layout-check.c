/* Read the owned OBN5 DMA slots directly before enabling network delivery. */
#define _POSIX_C_SOURCE 200809L
#include <fcntl.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

enum { SLOTS = 1024, FULL = 32768, VIEW = 4096, OBN5 = 0x354e424f, SHIFT = 544 * 8 };
static volatile uint32_t *regs;
static const uint8_t *full, *views;

static void check(bool ok, const char *message)
{
    if (!ok) {
        fprintf(stderr, "%s\n", message);
        exit(1);
    }
}

static uint64_t reg64(unsigned offset)
{
    return regs[offset / 4] | ((uint64_t)regs[offset / 4 + 1] << 32);
}

static void delay_ms(unsigned ms)
{
    struct timespec delay = { .tv_nsec = ms * 1000000L };
    nanosleep(&delay, NULL);
}

static uint64_t le(const uint8_t *p, unsigned n)
{
    uint64_t out = 0;
    for (unsigned i = 0; i < n; ++i)
        out |= (uint64_t)p[i] << (8 * i);
    return out;
}

static void print_locations(unsigned first, unsigned count)
{
    for (unsigned index = 0; index <= count; ++index) {
        unsigned slot = (first + index - 1) % SLOTS;
        const uint8_t *p = full + slot * FULL;
        for (unsigned i = 0; i + 64 <= FULL; i += 8)
            if (le(p+i, 4) == OBN5 && le(p+i+4, 4) == 0x400001)
                printf("header physical_slot=%u offset=%u ordinal=%" PRIu64 "\n",
                       slot, i, le(p+i+56, 8));
    }
}

static bool has_header(const uint8_t *p, uint64_t ordinal)
{
    return le(p, 4) == OBN5 && le(p+4, 4) == 0x400001 && le(p+56, 8) == ordinal;
}

static bool bind_split(uint64_t ordinal, unsigned *last_length, unsigned *last_view)
{
    const uint8_t *previous = full + ((ordinal - 1) % SLOTS) * FULL;
    for (unsigned bins = 4096; bins >= 1024; bins /= 2) {
        unsigned length = 128 + 6 * bins;
        unsigned widths[] = {0, bins / 8, bins / 4, bins / 2, bins};
        for (unsigned i = 0; i < sizeof widths / sizeof *widths; ++i) {
            if (has_header(previous + length - SHIFT + widths[i], ordinal)) {
                *last_length = length;
                *last_view = widths[i];
                return true;
            }
        }
    }
    return false;
}

static void validate(uint64_t first, uint64_t end, unsigned bins, unsigned mode)
{
    unsigned divisor = mode == 4 ? 1 : 4;
    unsigned length = 128 + 6 * bins;
    unsigned last_length = 0, last_view = 0;
    bool split = !has_header(full + (first % SLOTS) * FULL, first);
    if (split && !bind_split(first, &last_length, &last_view)) {
        check(first == 0 && bind_split(1, &last_length, &last_view), "split544 binding");
        puts("Cold ordinal 0 lacks a complete split544 prefix");
        ++first;
    }
    printf("layout=%s first_checked=%" PRIu64 " previous_length=%u previous_view=%u\n",
           split ? "split544" : "native", first, last_length, last_view);
    uint8_t reconstruction[128 + 6 * 4096];
    uint64_t previous_seq = 0;
    for (uint64_t ordinal = first; ordinal < end; ++ordinal) {
        unsigned slot = ordinal % SLOTS;
        const uint8_t *p = full + slot * FULL;
        const uint8_t *overview = views + slot * VIEW;
        if (split) {
            unsigned prev = (slot + SLOTS - 1) % SLOTS;
            unsigned prefix = SHIFT - last_view;
            memcpy(reconstruction, full + prev * FULL + last_length - prefix, prefix);
            memcpy(reconstruction + prefix, views + prev * VIEW, last_view);
            memcpy(reconstruction + SHIFT, p, length - SHIFT);
            overview = p + length - SHIFT;
            p = reconstruction;
            last_length = length;
            last_view = bins / divisor;
        }
        if (!has_header(p, ordinal))
            print_locations(first % SLOTS, (unsigned)(end-first));
        check(has_header(p, ordinal), "FULL header placement/ordinal");
        unsigned flags = le(p+44, 4);
        check(le(p+4, 4) == 0x400001 && le(p+32, 4) == bins && le(p+36, 4) == 64 &&
              le(p+40, 4) == 62500000 && le(p+48, 4) == 3 && le(p+52, 4) == length &&
              flags == (1u | ((mode & 3) << 3) | ((mode & 4) << 5)), "FULL ABI/geometry/K/flags");
        for (unsigned i = 64; i < 128; ++i)
            check(!p[i], "FULL header padding");
        for (unsigned bin = 0; bin < bins; bin += divisor) {
            unsigned max_code = 0;
            for (unsigned i = 0; i < divisor; ++i) {
                uint64_t word = le(p + 128 + 6 * (bin + i), 6);
                uint64_t mantissa = word & ((UINT64_C(1) << 45) - 1);
                unsigned code = mantissa ? 64 - __builtin_clzll(mantissa) + 2 * (word >> 45) : 0;
                if (max_code < code) max_code = code;
            }
            check(overview[bin / divisor] == max_code, "VIEW reduction or placement");
        }
        uint64_t seq = le(p+8, 8);
        check(ordinal == first || seq - previous_seq == 64, "static FFT cadence");
        previous_seq = seq;
    }
}

int main(void)
{
    struct { uint64_t full, view; uint32_t full_len, view_len, cached, line, abi, reserved; } info;
    int lease = open("/run/maia-observation.lock", O_CREAT | O_RDWR, 0600);
    check(lease >= 0 && !flock(lease, LOCK_EX | LOCK_NB), "spectral owner busy");
    int dma = open("/dev/obs_dma", O_RDWR);
    check(dma >= 0 && !ioctl(dma, 0x4f01, &info) && info.abi == 1 &&
          info.full_len == SLOTS * FULL && info.view_len == SLOTS * VIEW, "DMA INFO");
    full = mmap(NULL, info.full_len, PROT_READ, MAP_SHARED, dma, 0);
    views = mmap(NULL, info.view_len, PROT_READ, MAP_SHARED, dma, 0x1000000);
    int mem = open("/dev/mem", O_RDWR | O_SYNC);
    check(mem >= 0, "open Maia CSR");
    regs = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, mem, 0x7c460000);
    check(full != MAP_FAILED && views != MAP_FAILED && regs != MAP_FAILED, "DMA or CSR mmap");
    printf("Initial abi=%08x spectrum=%08x field_run=%08x guard=%08x session=%08x\n",
           regs[0x50/4], regs[0x64/4], regs[0x284/4], regs[0x1e0/4], regs[0x354/4]);
    check(regs[0x50/4] == OBN5 && regs[0x64/4] == 16 && !regs[0x354/4] && !regs[0x1e0/4],
          "stopped OBN5 with no other owner");
    regs[0x284/4] = 0;
    check(!regs[0x284/4], "stop unowned real8 stream before the static reference");
    printf("DMA full=0x%08" PRIx64 " view=0x%08" PRIx64 " cached=%u\n", info.full, info.view, info.cached);
    for (unsigned test = 0; test < 6; ++test) {
        unsigned config = 2 * (test % 3);
        unsigned bins = 4096 >> (test % 3);
        unsigned mode = test < 3 ? 2 : 4;
        unsigned hold = test < 3 ? 1 : 1023;
        regs[0x54/4] = 32;
        uint64_t first = reg64(0x100);
        check(first == reg64(0x110) && first == reg64(0x108), "empty starting cursor");
        regs[0x1b8/4] = 0;
        regs[0x1a8/4] = info.full;
        regs[0x1ac/4] = info.view;
        regs[0x1b0/4] = info.full_len;
        regs[0x1b4/4] = info.view_len;
        regs[0x164/4] = 0;
        regs[0x20/4] = 2 * 64;
        regs[0x58/4] = 100;
        regs[0x5c/4] = 0;
        regs[0x348/4] = config;
        regs[0x398/4] = hold;
        regs[0x1a4/4] = mode;
        regs[0x1b8/4] = 0x56494557;
        check(regs[0x1bc/4] == 1 && regs[0x398/4] == hold, "memory and hold readback");
        regs[0x54/4] = 3;
        check(regs[0x64/4] & 1, "START");
        check(regs[0x34c/4] == config, "active FFT mode");
        delay_ms(30);
        regs[0x54/4] = 4;
        for (unsigned i = 0; i < 100 && regs[0x64/4] != 16; ++i) delay_ms(2);
        check(regs[0x64/4] == 16, "STOP/DRAINED");
        regs[0x54/4] = 32;
        uint64_t end = reg64(0x100);
        check(end >= first + 4 && end - first < SLOTS && end == reg64(0x108), "committed range");
        uint32_t full_sync[] = {0, 0, info.full_len, 1};
        uint32_t view_sync[] = {1, 0, info.view_len, 1};
        check(!ioctl(dma, 0x4f02, full_sync) && !ioctl(dma, 0x4f02, view_sync), "DMA sync");
        validate(first, end, bins, mode);
        check(!regs[0x140/4] && !regs[0x144/4] && !regs[0x160/4], "spectrum loss counters");
        regs[0xe8/4] = end;
        regs[0xec/4] = end >> 32;
        regs[0xf0/4] = regs[0x150/4];
        regs[0x54/4] = 64;
        regs[0x54/4] = 32;
        check(reg64(0x110) == end, "cumulative release");
        printf("PASS bins=%u view_mode=%u hold_setting=%u records=%" PRIu64 " ordinals=%" PRIu64 "..%" PRIu64 "\n",
               bins, mode, hold, end - first, first, end - 1);
    }
    return close(dma) != 0;
}
