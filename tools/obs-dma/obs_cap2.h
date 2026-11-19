/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
#ifndef OBS_CAP2_H
#define OBS_CAP2_H
#include <linux/types.h>

#define OBS_CAP2_ID 0x32504143
#define OBS_CAP2_ID_REG 0x2e0
#define OBS_CAP2_STATUS_REG 0x304
#define OBS_CAP2_MAX_N 40960
#define OBS_CAP2_GUARD 4096
#define OBS_CAP2_PAYLOAD (OBS_CAP2_MAX_N * 4)
#define OBS_CAP2_LENGTH (OBS_CAP2_PAYLOAD + 2 * OBS_CAP2_GUARD)
#define OBS_CAP2_SENTINEL 0xa5

#define OBS_CAP2_INFO 0x4f10
#define OBS_CAP2_SUBMIT 0x4f11
#define OBS_CAP2_STATUS 0x4f12
#define OBS_CAP2_CANCEL 0x4f13
#define OBS_CAP2_ACQUIRE 0x4f14
#define OBS_CAP2_RELEASE 0x4f15

struct obs_cap2_info {
	__u64 base, full, view;
	__u32 length, payload_offset, payload_length, last_job;
};
struct obs_cap2_submit {
	__u32 job, samples, lease, offset, blocks, cookie;
};
struct obs_cap2_receipt {
	__u32 status, job, captured, first, last, released, committed;
	__u32 epoch_lo, epoch_hi;
};

static inline int obs_cap2_valid_window(__u64 base, const struct obs_cap2_submit *s)
{
	return s->job && s->samples && s->samples <= OBS_CAP2_MAX_N &&
		s->lease >= 2 && s->lease <= 187500 &&
		s->offset >= OBS_CAP2_GUARD && !(s->offset & 127) &&
		s->offset < OBS_CAP2_GUARD + OBS_CAP2_PAYLOAD &&
		s->blocks >= (s->samples + 31) / 32 &&
		s->blocks <= (OBS_CAP2_GUARD + OBS_CAP2_PAYLOAD - s->offset) / 128 &&
		base < 0x20000000ULL && base <= 0x20000000ULL - OBS_CAP2_LENGTH &&
		!(base & 127);
}
#endif
