/* Materialize existing WAT1 intent in a new, unpublished FFM1 RF map.
 * Usage: obs-watch-map WAT1 RF-MAP-IN NEW-RF-MAP VERSION
 * The caller retains WAT1 by RF map version until its receipts are drained. */
#define _POSIX_C_SOURCE 200809L
#include "obs-watch-policy.h"
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#ifndef O_BINARY
#define O_BINARY 0
#endif

static int read_file(const char *path, uint8_t *bytes, size_t capacity, size_t *size)
{
    FILE *file = fopen(path, "rb");
    if (!file) return -1;
    *size = fread(bytes, 1, capacity, file);
    int extra = fgetc(file);
    int error = ferror(file);
    return fclose(file) || error || extra != EOF ? -1 : 0;
}

static void put32(uint8_t *dst, uint32_t value)
{
    for (unsigned i = 0; i < 4; ++i) dst[i] = (uint8_t)(value >> (8 * i));
}

int main(int argc, char **argv)
{
    static uint8_t wat[WATCH_HEADER+WATCH_RESIDENT_MAX*WATCH_RECORD], rows[WATCH_RESIDENT_MAX*32u];
    struct watch_table tasks;
    size_t wat_size, row_size;
    if (argc != 5) return 2;
    char *end;
    errno = 0;
    unsigned long long version = strtoull(argv[4], &end, 0);
    if (errno || *end || !version || version > UINT32_MAX) return 2;
    if (read_file(argv[1], wat, sizeof(wat), &wat_size) ||
        read_file(argv[2], rows, sizeof(rows), &row_size) ||
        watch_table_read(&tasks, wat, wat_size, WATCH_RESIDENT_MAX, 13000000000ull) || version != tasks.version ||
        row_size != 32u * tasks.count) return 3;
    for (unsigned i = 0; i < tasks.count; ++i) {
        uint8_t *row = rows + 32u * i;
        uint32_t flags = watch_u32(row + 4);
        uint32_t word = watch_u32(row + 12);
        int cap2 = (flags & 8u) != 0;
        if (cap2 ? !word : (flags & 32u) ? word >= 0x10000000u : word != 0)
            return 3;
        flags &= ~32u;
        struct watch_target target=watch_row(&tasks,i);
        if (target.enabled) flags |= 32u;
        if (!cap2) put32(row + 12, target.enabled ? target.nco : 0);
        put32(row + 4, flags);
    }
    int fd = open(argv[3], O_CREAT | O_EXCL | O_WRONLY | O_BINARY, 0600);
    if (fd < 0) return 4;
    FILE *output = fdopen(fd, "wb");
    if (!output) {
        close(fd);
        remove(argv[3]);
        return 4;
    }
    int error = fwrite(rows, 1, row_size, output) != row_size;
    if (fclose(output) || error) {
        remove(argv[3]);
        return 4;
    }
    return 0;
}
