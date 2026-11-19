/* Bounded RAM acceptance exercise: one serialized CAP2 job, one SIG0 worker. */
#define _POSIX_C_SOURCE 200809L
#include "obs_cap2.h"
#include "sig0.h"
#include <errno.h>
#include <fcntl.h>
#include <arpa/inet.h>
#include <inttypes.h>
#include <signal.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int cap_fd = -1;
static pid_t agent = -1;
static const volatile uint32_t *csr;

static uint64_t now(clockid_t clock)
{
	struct timespec ts;
	if (clock_gettime(clock, &ts))
		abort();
	return (uint64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static void pause_us(long us)
{
	struct timespec sleep = { .tv_nsec = us * 1000 };
	nanosleep(&sleep, NULL);
}

static void cleanup(void)
{
	if (cap_fd >= 0)
		close(cap_fd);
	if (agent > 0) {
		for (int i = 0; i < 600; i++) {
			if (waitpid(agent, NULL, WNOHANG) != 0)
				return;
			if (i >= 5 && csr[0x1e0 / 4] == 0)
				break;
			pause_us(10000);
		}
		int fd = socket(AF_INET, SOCK_STREAM, 0);
		struct sockaddr_in loopback = { .sin_family = AF_INET, .sin_port = htons(18080),
						.sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
		const char stop[] = "POST /api/system/stop HTTP/1.1\r\nHost: localhost\r\n"
				    "Content-Length: 0\r\nConnection: close\r\n\r\n";
		if (fd >= 0) {
			if (!connect(fd, (const struct sockaddr *)&loopback, sizeof loopback))
				(void)send(fd, stop, sizeof stop - 1, MSG_NOSIGNAL);
			close(fd);
		}
		for (int i = 0; i < 100; i++) {
			if (waitpid(agent, NULL, WNOHANG) != 0)
				return;
			pause_us(10000);
		}
		kill(agent, SIGTERM);
		waitpid(agent, NULL, 0);
	}
}

static void check(bool ok, const char *what)
{
	if (!ok) {
		fprintf(stderr, "%s (errno=%d)\n", what, errno);
		exit(1);
	}
}

static uint32_t number(const char *text)
{
	char *end;
	errno = 0;
	unsigned long value = strtoul(text, &end, 10);
	check(*text && !*end && !errno && value <= UINT32_MAX, "uint32 argument");
	return value;
}

static uint64_t epoch(void)
{
	uint32_t hi, lo;
	do {
		hi = csr[0x184 / 4];
		lo = csr[0x180 / 4];
	} while (hi != csr[0x184 / 4]);
	return (uint64_t)hi << 32 | lo;
}

static void launch(const char *path)
{
	agent = fork();
	check(agent >= 0, "fork reader");
	if (!agent) {
		int out = open("reader.log", O_WRONLY | O_CREAT | O_TRUNC, 0600);
		if (out < 0 || dup2(out, STDOUT_FILENO) < 0 || dup2(out, STDERR_FILENO) < 0)
			_exit(126);
		close(out);
		execl("./maia-delivery", "maia-delivery", "--system-observations",
		      path, "--listen", "127.0.0.1:18080", (char *)NULL);
		_exit(127);
	}
}

static unsigned state(struct obs_cap2_receipt receipt)
{
	return receipt.status & 15;
}

static struct obs_cap2_receipt wait_job(uint32_t job, bool cancel_capture,
				       uint64_t *detected, uint64_t *poll_bound)
{
	const uint64_t end = now(CLOCK_MONOTONIC) + 4000000000;
	uint64_t previous_begin = 0;
	struct obs_cap2_receipt r;
	while (now(CLOCK_MONOTONIC) < end) {
		uint64_t begin = now(CLOCK_MONOTONIC);
		check(!ioctl(cap_fd, OBS_CAP2_STATUS, &r) && r.job == job, "read job receipt");
		*detected = now(CLOCK_MONOTONIC);
		if (state(r) == 5 || state(r) == 6) {
			*poll_bound = previous_begin ? *detected - previous_begin : 0;
			check(!cancel_capture, "capture state never observed");
			return r;
		}
		previous_begin = begin;
		if (cancel_capture && state(r) == 3 && r.captured >= 64) {
			check(!ioctl(cap_fd, OBS_CAP2_CANCEL, &job), "cancel while CAPTURE observed");
			cancel_capture = false;
			printf("cancel_issued_at_captured=%u\n", r.captured);
		}
		if (!cancel_capture)
			pause_us(20);
	}
	errno = ETIMEDOUT;
	check(false, "terminal timeout");
	return r;
}

static void rejection_tests(struct obs_cap2_submit valid)
{
	struct obs_cap2_submit bad[] = {valid, valid, valid, valid, valid, valid};
	bad[0].samples = 0;
	bad[1].samples = OBS_CAP2_MAX_N + 1;
	bad[2].blocks = 0;
	bad[3].offset++;
	bad[4].offset = UINT32_MAX - 127;
	bad[5].blocks = UINT32_MAX;
	for (size_t i = 0; i < sizeof bad / sizeof bad[0]; i++)
		check(ioctl(cap_fd, OBS_CAP2_SUBMIT, &bad[i]) == -1 && errno == EINVAL,
		      "descriptor rejection");
	check(!obs_cap2_valid_window(UINT64_MAX - 127, &valid), "address-add overflow");
	check(!obs_cap2_valid_window(0x1fffff80, &valid), "32-bit device address limit");
	puts("descriptor_rejections=8");
}

static bool ranges_intersect(uint64_t a, uint64_t n, uint64_t b, uint64_t m)
{
	return a < b + m && b < a + n;
}

static void check_ranges(const struct obs_cap2_info *info, bool mapped_scan)
{
	check(!ranges_intersect(info->base, info->length, info->full, 33554432) &&
	      !ranges_intersect(info->base, info->length, info->view, 2097152), "CAP2 vs spectra");
	if (mapped_scan) {
		uint32_t guard[128];
		int fd = open("/dev/obs_fl_guard", O_RDONLY | O_CLOEXEC);
		check(fd >= 0 && !ioctl(fd, 0x4605, guard) && guard[7] == 4096, "guard info");
		close(fd);
		for (int i = 4; i <= 6; i++)
			check(guard[i] && !ranges_intersect(info->base, info->length, guard[i], guard[7]),
			      "CAP2 vs catalog/current/next");
		printf("map_dma=%08x,%08x,%08x\n", guard[4], guard[5], guard[6]);
	}
	printf("arena=%" PRIx64 " bytes=%u full=%" PRIx64 " view=%" PRIx64 "\n",
	       (uint64_t)info->base, info->length, (uint64_t)info->full, (uint64_t)info->view);
}

static void release_window(const unsigned char *arena, uint32_t length)
{
	check(ioctl(cap_fd, OBS_CAP2_RELEASE) == -1 && errno == EBUSY,
	      "release must reject live mapping");
	check(!munmap((void *)arena, length), "unmap CAP2");
	check(!ioctl(cap_fd, OBS_CAP2_RELEASE), "release CAP2 arena");
}

static const unsigned char *acquire(struct obs_cap2_receipt r, struct obs_cap2_info info)
{
	struct obs_cap2_receipt actual;
	check(!ioctl(cap_fd, OBS_CAP2_ACQUIRE, &actual) && !memcmp(&actual, &r, sizeof r),
	      "terminal receipt changed across acquire");
	void *arena = mmap(NULL, info.length, PROT_READ, MAP_SHARED, cap_fd, 0);
	check(arena != MAP_FAILED, "mmap CAP2 coherent arena");
	return arena;
}

static void canaries(const unsigned char *arena, uint32_t length, uint32_t offset, uint32_t count)
{
	for (uint32_t i = 0; i < length; i++)
		if (i < offset || i >= offset + count * 4)
			check(arena[i] == OBS_CAP2_SENTINEL, "guard or byte tail changed");
}

static void dump(const char *path, const uint32_t *iq, uint32_t n, uint32_t fs)
{
	if (!strcmp(path, "-"))
		return;
	FILE *f = fopen(path, "wb");
	check(f && fwrite(&n, 4, 1, f) == 1 && fwrite(&fs, 4, 1, f) == 1 &&
	      fwrite(iq, 4, n, f) == n && !fclose(f), "save CAP2 IQ");
}

int main(int argc, char **argv)
{
	check(argc == 8, "cap2-run good|reuse|wait-cancel|capture-cancel|miss|video N Fs job reader.json IQ|- epoch_delta");
	bool cancel_wait = !strcmp(argv[1], "wait-cancel");
	bool cancel_capture = !strcmp(argv[1], "capture-cancel");
	bool miss = !strcmp(argv[1], "miss") || !strcmp(argv[1], "video");
	bool reuse = !strcmp(argv[1], "reuse");
	bool good = !strcmp(argv[1], "good") || reuse;
	check(good || cancel_wait || cancel_capture || miss, "unknown mode");
	uint32_t n = number(argv[2]), fs = number(argv[3]), job = number(argv[4]);
	uint32_t epoch_delta = number(argv[7]);
	check(n && n <= OBS_CAP2_MAX_N && fs && job < UINT32_MAX - 1, "runner job bounds");
	check(atexit(cleanup) == 0, "cleanup handler");
	cap_fd = open("/dev/obs_cap2", O_RDWR | O_CLOEXEC);
	struct obs_cap2_info info;
	check(cap_fd >= 0 && !ioctl(cap_fd, OBS_CAP2_INFO, &info), "open CAP2 device");
	check(info.payload_offset == OBS_CAP2_GUARD && info.length == OBS_CAP2_LENGTH &&
	      info.payload_length == OBS_CAP2_PAYLOAD, "CAP2 arena layout");
	int mem = open("/dev/mem", O_RDONLY | O_SYNC | O_CLOEXEC);
	check(mem >= 0, "open read-only Maia CSR");
	csr = mmap(NULL, 4096, PROT_READ, MAP_SHARED, mem, 0x7c460000);
	close(mem);
	check(csr != MAP_FAILED && csr[OBS_CAP2_ID_REG / 4] == OBS_CAP2_ID, "CAP2 CSR ID");
	uint64_t expected_epoch = epoch_delta ? epoch() + epoch_delta : 0;
	struct obs_cap2_submit submit = {job, n, 187500, info.payload_offset, (n + 31) / 32, job};
	rejection_tests(submit);
	uint64_t started = now(CLOCK_MONOTONIC);
	check(!ioctl(cap_fd, OBS_CAP2_SUBMIT, &submit), "submit CAP2 job");
	uint64_t armed = now(CLOCK_MONOTONIC);
	check(ioctl(cap_fd, OBS_CAP2_SUBMIT, &submit) == -1 && errno == EBUSY, "busy submit");
	uint32_t wrong = job + 1;
	check(ioctl(cap_fd, OBS_CAP2_CANCEL, &wrong) == -1 && errno == EPERM, "wrong key");
	check(ioctl(cap_fd, OBS_CAP2_RELEASE) == -1 && errno == EBUSY, "release before terminal");
	if (cancel_wait)
		check(!ioctl(cap_fd, OBS_CAP2_CANCEL, &job), "cancel in WAIT");
	else
		launch(argv[5]);
	uint64_t detected, poll_bound;
	struct obs_cap2_receipt r = wait_job(job, cancel_capture, &detected, &poll_bound);
	unsigned reason = (r.status >> 4) & 15;
	uint64_t job_epoch = (uint64_t)r.epoch_hi << 32 | r.epoch_lo;
	printf("job=%u n=%u fs=%u state=%u reason=%u epoch=%" PRIu64 " count=%u first=%u last=%u rf_release=%u commit=%u submit_ns=%" PRIu64 " armed_to_detect_ns=%" PRIu64 " poll_bound_ns=%" PRIu64 "\n",
	       job, n, fs, state(r), reason, job_epoch, r.captured, r.first, r.last, r.released,
	       r.committed, armed - started, detected - armed, poll_bound);
	check(!(r.status & 512) && job_epoch == expected_epoch, "RF hold/epoch");
	check_ranges(&info, !cancel_wait);
	uint64_t acquire_start = now(CLOCK_MONOTONIC);
	const unsigned char *arena = acquire(r, info);
	uint64_t acquired = now(CLOCK_MONOTONIC);
	if (!good) {
		check(state(r) == 6 && reason == (miss ? 2u : 3u), "transport error reason");
		check(cancel_capture ? r.captured >= 64 && r.captured < n : r.captured == 0,
		      "cancel/miss captured count");
		canaries(arena, info.length, submit.offset, r.captured);
		release_window(arena, info.length);
		return 0;
	}
	check(state(r) == 5 && !reason && r.captured == n && r.last == r.released && job_epoch,
	      "READY receipt");
	const uint32_t *iq = (const uint32_t *)(arena + submit.offset);
	canaries(arena, info.length, submit.offset, n);
	dump(argv[6], iq, n, fs);
	sig0_work *work = NULL;
	check(!sig0_configure(NULL, NULL), "SIG0 configuration");
	uint64_t prepare_begin = now(CLOCK_MONOTONIC), prepare_cpu = now(CLOCK_PROCESS_CPUTIME_ID);
	check(!sig0_prepare_iq(iq, n, fs, &work), "prepare IQ");
	prepare_cpu = now(CLOCK_PROCESS_CPUTIME_ID) - prepare_cpu;
	uint64_t prepared = now(CLOCK_MONOTONIC);
	release_window(arena, info.length);
	uint64_t returned = now(CLOCK_MONOTONIC);
	if (reuse) {
		/* Same physical arena is filled with the sentinel by submit; the old
		 * map keeps cycling with its old job ID while this job waits. */
		check(ioctl(cap_fd, OBS_CAP2_SUBMIT, &submit) == -1 && errno == ESTALE, "stale job ID");
		submit.job++;
		check(!ioctl(cap_fd, OBS_CAP2_SUBMIT, &submit), "reuse arena for different ID");
		pause_us(10000);
		struct obs_cap2_receipt old;
		check(!ioctl(cap_fd, OBS_CAP2_STATUS, &old) && old.job == submit.job &&
		      state(old) == 1 && !old.captured && epoch() > job_epoch, "old map replay");
		check(!ioctl(cap_fd, OBS_CAP2_CANCEL, &submit.job), "cancel reuse probe");
		uint64_t unused;
		old = wait_job(submit.job, false, &unused, &poll_bound);
		check(state(old) == 6 && ((old.status >> 4) & 15) == 3, "reused arena cancel");
		arena = acquire(old, info);
		canaries(arena, info.length, submit.offset, 0);
		release_window(arena, info.length);
	}
	sig0_detection result;
	uint64_t finish_begin = now(CLOCK_MONOTONIC), finish_cpu = now(CLOCK_PROCESS_CPUTIME_ID);
	uint64_t epoch_before = epoch();
	check(!sig0_finish(work, &result), "SIG0 finish");
	uint64_t finished = now(CLOCK_MONOTONIC);
	finish_cpu = now(CLOCK_PROCESS_CPUTIME_ID) - finish_cpu;
	printf("first_to_last=%u release_to_commit=%u acquire_ns=%" PRIu64 " prepare_ns=%" PRIu64 " release_ns=%" PRIu64 " finish_ns=%" PRIu64 " total_ns=%" PRIu64 " prepare_cpu_ns=%" PRIu64 " finish_cpu_ns=%" PRIu64 " epoch_at_finish_begin=%" PRIu64 " epoch_at_finish_end=%" PRIu64 "\n",
	       r.last - r.first, r.committed - r.released, acquired - acquire_start,
	       prepared - prepare_begin, returned - prepared, finished - finish_begin,
	       finished - started, prepare_cpu, finish_cpu, epoch_before, epoch());
	printf("sig0 %u %u", result.status, result.count);
	for (unsigned i = 0; i < result.count; i++)
		for (unsigned j = 0; j < 16; j++)
			printf(" %.17g", result.descriptor[i][j]);
	printf("\nfamily=%s\n", result.family);
	return 0;
}
