/* Prepare the next OBN3 START using the existing spectral-owner lock. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/file.h>
#include <sys/mman.h>
#include <unistd.h>
#include "obs-stream-002.h"

static uint64_t latched64(volatile const uint32_t *csr, unsigned offset)
{
    return csr[offset / 4] | ((uint64_t)csr[offset / 4 + 1] << 32);
}

int main(int argc, char **argv)
{
    if (argc != 3 || (strcmp(argv[1], "12") && strcmp(argv[1], "8")) ||
        (strcmp(argv[2], "native") && strcmp(argv[2], "compact"))) {
        fprintf(stderr, "usage: obs-obn3-mode 12|8 native|compact\n");
        return 2;
    }
    unsigned mode = (!strcmp(argv[1], "8") ? OBS_OBN3_RAW8 : 0) |
                    (!strcmp(argv[2], "compact") ? OBS_OBN3_PACKED : 0);
    int lease = open("/run/maia-observation.lock", O_CREAT | O_RDWR | O_CLOEXEC, 0600);
    if (lease < 0 || flock(lease, LOCK_EX | LOCK_NB)) {
        perror("spectral owner");
        return 1;
    }
    int fd = open("/dev/mem", O_RDWR | O_SYNC | O_CLOEXEC);
    if (fd < 0) { perror("/dev/mem"); return 1; }
    volatile uint32_t *csr = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0x7c460000);
    if (csr == MAP_FAILED) { perror("mmap"); return 1; }
    int rc = 1;
    if (csr[OBS_SPECTRUM_ID_REG / 4] != OBS_OBN3 || csr[OBS_SPECTRUM_REVISION / 4] != 4 ||
        (csr[OBS_SPECTRUM_FEATURES / 4] & 0x60) != 0x60) {
        fprintf(stderr, "OBN3 ID, revision or capabilities mismatch\n");
        goto done;
    }
    if (csr[0x064 / 4] != 16) {
        fprintf(stderr, "requires healthy stopped and drained writer\n");
        goto done;
    }
    csr[0x054 / 4] = 32; /* SNAPSHOT, stable until another owner requests one */
    __sync_synchronize();
    uint64_t committed = latched64(csr, 0x100);
    if (csr[0x154 / 4] != 16 || latched64(csr, 0x108) != committed ||
        latched64(csr, 0x110) != committed) {
        fprintf(stderr, "reserved, committed and released cursors must match\n");
        goto done;
    }
    csr[OBS_OBN3_CONFIG / 4] = mode;
    __sync_synchronize();
    if (csr[OBS_OBN3_CONFIG / 4] != mode) {
        fprintf(stderr, "configuration readback failed\n");
        goto done;
    }
    printf("prepared=%u active_at_last_START=%u\n", mode, csr[OBS_OBN3_ACTIVE / 4]);
    rc = 0;
done:
    munmap((void *)csr, 4096);
    close(fd);
    close(lease);
    return rc;
}
