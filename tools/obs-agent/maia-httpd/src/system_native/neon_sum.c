#include <arm_neon.h>
#include <stdint.h>
#include <stddef.h>
/* Borrowed committed, still-owned span. All 64-bit payload words participate.
 * Prefetch remains strictly within the given span. No out-of-range touch. */
uint64_t obs_neon_sum(const uint8_t *p,size_t n){
 uint64x2_t a=vdupq_n_u64(0),b=a,c=a,d=a;
 const uint64x2_t mask=vdupq_n_u64((UINT64_C(1)<<50)-1);
 size_t i;
 for(i=0;i+64<=n;i+=64){
  if(i+256<n)__builtin_prefetch(p+i+256,0,3);
  a=vaddq_u64(a,vandq_u64(vld1q_u64((const uint64_t*)(p+i)),mask));
  b=vaddq_u64(b,vandq_u64(vld1q_u64((const uint64_t*)(p+i+16)),mask));
  c=vaddq_u64(c,vandq_u64(vld1q_u64((const uint64_t*)(p+i+32)),mask));
  d=vaddq_u64(d,vandq_u64(vld1q_u64((const uint64_t*)(p+i+48)),mask));
 }
 a=vaddq_u64(vaddq_u64(a,b),vaddq_u64(c,d));
 uint64_t sum=vgetq_lane_u64(a,0)+vgetq_lane_u64(a,1);
 for(;i<n;i+=8)sum+=*(const uint64_t*)(p+i)&((UINT64_C(1)<<50)-1);
 return sum;
}
