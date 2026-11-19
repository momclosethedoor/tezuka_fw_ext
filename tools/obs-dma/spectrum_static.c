/* Local K700 acceptance, using the owned r3 ring and its existing CSR ABI. */
#define _POSIX_C_SOURCE 200809L
#include <fcntl.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/file.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

enum { SLOTS = 1024, FULL = 32768, VIEW = 2048, PREFIX = 4352, ACTIVE_VIEW = 512 };

static volatile uint32_t *csr;
static const uint8_t *full, *view;

static void require(bool condition, const char *label)
{
	if (!condition) {
		fprintf(stderr, "K700: %s\n", label);
		exit(1);
	}
}

static void wait_ms(unsigned msec)
{
	struct timespec t = { .tv_nsec = msec * 1000000L };
	nanosleep(&t, NULL);
}

static uint64_t pair(unsigned index)
{
	return csr[index / 4] | ((uint64_t)csr[index / 4 + 1] << 32);
}

static void sync_region(int fd, unsigned region, unsigned len)
{
	uint32_t sync[] = {region, 0, len, 1};
	require(!ioctl(fd, 0x4f02, sync), "DMA access");
}

static uint64_t read_le(const uint8_t *p, unsigned len)
{
	uint64_t v = 0;
	for (unsigned i = 0; i < len; i++)
		v |= (uint64_t)p[i] << (8 * i);
	return v;
}

static void inspect(uint64_t begin, uint64_t end)
{
	uint64_t previous_seq = 0;
	for (uint64_t ordinal = begin + 1; ordinal < end; ordinal++) {
		uint32_t slot = ordinal % SLOTS, prev = (slot + SLOTS - 1) % SLOTS;
		const uint8_t *prefix = full + (prev + 1) * FULL - PREFIX + ACTIVE_VIEW;
		const uint8_t *middle = view + prev * VIEW;
		const uint8_t *suffix = full + slot * FULL;
		const uint8_t *overview = suffix + FULL - PREFIX;
		uint8_t h[64];
		for (unsigned i = 0; i < sizeof h; i++)
			h[i] = prefix[8 * i + 7];
		printf("raw ordinal=%" PRIu64 " abi=%" PRIx64 " epoch=%" PRIu64 " k=%" PRIu64 " flags=%" PRIx64 "\n",
		       read_le(h + 56, 8), read_le(h, 4), read_le(h + 16, 8),
		       read_le(h + 36, 4), read_le(h + 44, 4));
		require(read_le(h, 4) == 0x324e424f && read_le(h + 4, 2) == 3 &&
			read_le(h + 6, 2) == 64 && read_le(h + 16, 8) == 100 &&
			read_le(h + 32, 4) == 4096 && read_le(h + 36, 4) == 700 &&
			read_le(h + 44, 4) == 9 && read_le(h + 56, 8) == ordinal,
			"static header or ordinal");
		uint64_t seq = read_le(h + 8, 8);
		require(ordinal == begin + 1 || seq - previous_seq == 700, "static FFT cadence");
		previous_seq = seq;
		for (unsigned bin = 0; bin < 4096; bin += 8) {
			unsigned q = 0;
			for (unsigned k = bin; k < bin + 8; k++) {
				unsigned offset = k * 8;
				const uint8_t *address = offset < PREFIX - ACTIVE_VIEW ? prefix + offset :
					offset < PREFIX ? middle + offset - (PREFIX - ACTIVE_VIEW) :
					suffix + offset - PREFIX;
				uint64_t w = read_le(address, 8);
				uint64_t power = w & ((UINT64_C(1) << 47) - 1);
				unsigned code = power ? 64 - __builtin_clzll(power) + 2 * ((w >> 47) & 7) : 0;
				if (code > q)
					q = code;
			}
			require(overview[bin / 8] == q, "static VIEW reduction");
		}
		printf("ordinal=%" PRIu64 " epoch=100 seq=%" PRIu64 " tick=%" PRIu64 "\n",
		       ordinal, seq, read_le(h + 24, 8));
	}
}

int main(void)
{
	struct { uint64_t full, view; uint32_t full_len, view_len, cached, cache_line, abi, reserved; } info;
	int lease = open("/run/maia-observation.lock", O_CREAT | O_RDWR, 0600);
	require(lease >= 0 && !flock(lease, LOCK_EX | LOCK_NB), "spectrometer owner busy");
	int dma = open("/dev/obs_dma", O_RDWR);
	require(dma >= 0 && !ioctl(dma, 0x4f01, &info) && info.abi == 1 &&
		info.full_len == SLOTS * FULL && info.view_len == SLOTS * VIEW, "ring1024 INFO");
	full = mmap(NULL, info.full_len, PROT_READ, MAP_SHARED, dma, 0);
	view = mmap(NULL, info.view_len, PROT_READ, MAP_SHARED, dma, 0x1000000);
	int mem = open("/dev/mem", O_RDWR | O_SYNC);
	require(mem >= 0, "open Maia CSR");
	csr = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, mem, 0x7c460000);
	require(full != MAP_FAILED && view != MAP_FAILED && csr != MAP_FAILED, "mmap");
	require(csr[0x1c0 / 4] == SLOTS && csr[0x64 / 4] == 16 &&
		!csr[0x284 / 4] && !csr[0x1e0 / 4], "quiescent static ownership");
	csr[0x54 / 4] = 32;
	uint64_t start = pair(0x100);
	require(start == pair(0x110), "drained starting cursor");
	csr[0x1b8 / 4] = 0;
	csr[0x1a8 / 4] = info.full;
	csr[0x1ac / 4] = info.view;
	csr[0x1b0 / 4] = info.full_len;
	csr[0x1b4 / 4] = info.view_len;
	csr[0x1a4 / 4] = 1;
	csr[0x164 / 4] = 0;
	csr[0x20 / 4] = 2 * 700;
	csr[0x58 / 4] = 100;
	csr[0x5c / 4] = 0;
	csr[0x1b8 / 4] = 0x56494557;
	require(csr[0x1bc / 4] == 1 && csr[0x164 / 4] == 0, "memory validation and scan disabled");
	uint64_t initial_rx_epoch = pair(0x180);
	csr[0x54 / 4] = 3;
	require(csr[0x64 / 4] & 1, "START");
	wait_ms(270);
	csr[0x54 / 4] = 4;
	for (int i = 0; i < 100 && csr[0x64 / 4] != 16; i++)
		wait_ms(2);
	require(csr[0x64 / 4] == 16, "STOP/DRAINED");
	require(pair(0x180) == initial_rx_epoch, "static must not recall");
	csr[0x54 / 4] = 32;
	uint64_t end = pair(0x100);
	require(end >= start + 4 && end - start < SLOTS, "number of complete windows");
	sync_region(dma, 0, info.full_len);
	sync_region(dma, 1, info.view_len);
	inspect(start, end);
	csr[0xe8 / 4] = (uint32_t)end;
	csr[0xec / 4] = (uint32_t)(end >> 32);
	csr[0xf0 / 4] = csr[0x150 / 4];
	csr[0x54 / 4] = 64;
	csr[0x54 / 4] = 32;
	require(pair(0x110) == end, "final cumulative release");
	printf("verified=%" PRIu64 " first_excluded_for_split=%" PRIu64 " drained=%" PRIu64 "\n",
	       end - start - 1, start, end);
	return 0;
}
