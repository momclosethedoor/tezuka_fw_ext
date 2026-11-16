#include <arm_neon.h>
#include <stdint.h>
#include <stddef.h>
/* Diagnostic complete read without checksum or copying. Volatile vector loads
 * prevent the compiler from replacing a full read by a pointer observation. */
void obs_neon_read(const uint8_t *p,size_t n){
 size_t i=0;
 for(;i+32<=n;i+=32){
  if(i+256<n)__builtin_prefetch(p+i+256,0,3);
  __asm__ volatile("vld1.8 {d0-d3}, [%0]"::"r"(p+i):"d0","d1","d2","d3","memory");
 }
 for(;i<n;i++){volatile uint8_t x=*(const volatile uint8_t*)(p+i);(void)x;}
}
/* Private copy of exactly the owned span. No speculative read beyond it. */
void obs_neon_copy(uint8_t *dst,const uint8_t *src,size_t n){
 size_t i=0;
 for(;i+64<=n;i+=64){
  if(i+256<n)__builtin_prefetch(src+i+256,0,3);
  uint8x16_t a=vld1q_u8(src+i),b=vld1q_u8(src+i+16);
  uint8x16_t c=vld1q_u8(src+i+32),d=vld1q_u8(src+i+48);
  vst1q_u8(dst+i,a);vst1q_u8(dst+i+16,b);
  vst1q_u8(dst+i+32,c);vst1q_u8(dst+i+48,d);
 }
 for(;i<n;i++)dst[i]=src[i];
}
