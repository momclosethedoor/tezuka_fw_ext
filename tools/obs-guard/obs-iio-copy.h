#ifndef OBS_IIO_COPY_H
#define OBS_IIO_COPY_H
#include <stdint.h>
#include <stddef.h>
#include <string.h>
/* IIO owns the source until the next refill. Destination is ordinary owned RAM.
 * No over-read: aligned complete blocks only; libc handles alignment and tail. */
static inline void obs_iio_copy(void *dst,const void *src,size_t n,unsigned width){
 uint8_t *to=dst;const uint8_t *from=src;
#if defined(__arm__)
 if((((uintptr_t)to|(uintptr_t)from)&7u)==0){
  if(width==32){while(n>=32){
   __asm__ __volatile__("vldmia %0!, {d0-d3}\n\tvstmia %1!, {d0-d3}" : "+r"(from),"+r"(to) :: "d0","d1","d2","d3","memory");n-=32;
  }}else if(width==64){while(n>=64){
   __asm__ __volatile__("vldmia %0!, {d0-d7}\n\tvstmia %1!, {d0-d7}" : "+r"(from),"+r"(to) :: "d0","d1","d2","d3","d4","d5","d6","d7","memory");n-=64;
  }}else if(width==128){while(n>=128){
   __asm__ __volatile__("vldmia %0!, {d0-d15}\n\tvstmia %1!, {d0-d15}" : "+r"(from),"+r"(to) :: "d0","d1","d2","d3","d4","d5","d6","d7","d8","d9","d10","d11","d12","d13","d14","d15","memory");n-=128;
  }}
 }
#else
 (void)width;
#endif
 if(n)memcpy(to,from,n);
}
#endif
