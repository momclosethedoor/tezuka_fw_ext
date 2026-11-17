#include <arm_neon.h>
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
/* Complete volatile reads, bounded by the owned span. Separate noinline
 * functions keep each register clobber set independent for the comparison. */
static __attribute__((noinline)) void read_0(const uint8_t *p,size_t n){size_t i=0;
 for(;i+32<=n;i+=32){
  if(i+256<n)__builtin_prefetch(p+i+256,0,3);
  __asm__ volatile("vld1.8 {d0-d3}, [%0]"::"r"(p+i):"d0","d1","d2","d3","memory");
 }
 for(;i<n;i++){volatile uint8_t x=*(const volatile uint8_t*)(p+i);(void)x;}
}
static __attribute__((noinline)) void read_1(const uint8_t *p,size_t n){size_t i=0;
 for(;i+64<=n;i+=64){
  __asm__ volatile("vldmia %0, {d0-d7}"::"r"(p+i+0):"d0","d1","d2","d3","d4","d5","d6","d7","memory");
 }
 for(;i+32<=n;i+=32){
  if(i+256<n)__builtin_prefetch(p+i+256,0,3);
  __asm__ volatile("vld1.8 {d0-d3}, [%0]"::"r"(p+i):"d0","d1","d2","d3","memory");
 }
 for(;i<n;i++){volatile uint8_t x=*(const volatile uint8_t*)(p+i);(void)x;}
}
static __attribute__((noinline)) void read_2(const uint8_t *p,size_t n){size_t i=0;
 for(;i+128<=n;i+=128){
  __asm__ volatile("vldmia %0, {d0-d15}"::"r"(p+i+0):"d0","d1","d2","d3","d4","d5","d6","d7","d8","d9","d10","d11","d12","d13","d14","d15","memory");
 }
 for(;i+32<=n;i+=32){
  if(i+256<n)__builtin_prefetch(p+i+256,0,3);
  __asm__ volatile("vld1.8 {d0-d3}, [%0]"::"r"(p+i):"d0","d1","d2","d3","memory");
 }
 for(;i<n;i++){volatile uint8_t x=*(const volatile uint8_t*)(p+i);(void)x;}
}
static __attribute__((noinline)) void read_3(const uint8_t *p,size_t n){size_t i=0;
 for(;i+128<=n;i+=128){
  __asm__ volatile("vldmia %0, {d16-d31}"::"r"(p+i+0):"d16","d17","d18","d19","d20","d21","d22","d23","d24","d25","d26","d27","d28","d29","d30","d31","memory");
 }
 for(;i+32<=n;i+=32){
  if(i+256<n)__builtin_prefetch(p+i+256,0,3);
  __asm__ volatile("vld1.8 {d0-d3}, [%0]"::"r"(p+i):"d0","d1","d2","d3","memory");
 }
 for(;i<n;i++){volatile uint8_t x=*(const volatile uint8_t*)(p+i);(void)x;}
}
static __attribute__((noinline)) void read_4(const uint8_t *p,size_t n){size_t i=0;
 for(;i+256<=n;i+=256){
  __asm__ volatile("vldmia %0, {d16-d31}"::"r"(p+i+0):"d16","d17","d18","d19","d20","d21","d22","d23","d24","d25","d26","d27","d28","d29","d30","d31","memory");
  __asm__ volatile("vldmia %0, {d16-d31}"::"r"(p+i+128):"d16","d17","d18","d19","d20","d21","d22","d23","d24","d25","d26","d27","d28","d29","d30","d31","memory");
 }
 for(;i+32<=n;i+=32){
  if(i+256<n)__builtin_prefetch(p+i+256,0,3);
  __asm__ volatile("vld1.8 {d0-d3}, [%0]"::"r"(p+i):"d0","d1","d2","d3","memory");
 }
 for(;i<n;i++){volatile uint8_t x=*(const volatile uint8_t*)(p+i);(void)x;}
}
static __attribute__((noinline)) void read_5(const uint8_t *p,size_t n){size_t i=0;
 for(;i+512<=n;i+=512){
  __asm__ volatile("vldmia %0, {d16-d31}"::"r"(p+i+0):"d16","d17","d18","d19","d20","d21","d22","d23","d24","d25","d26","d27","d28","d29","d30","d31","memory");
  __asm__ volatile("vldmia %0, {d16-d31}"::"r"(p+i+128):"d16","d17","d18","d19","d20","d21","d22","d23","d24","d25","d26","d27","d28","d29","d30","d31","memory");
  __asm__ volatile("vldmia %0, {d16-d31}"::"r"(p+i+256):"d16","d17","d18","d19","d20","d21","d22","d23","d24","d25","d26","d27","d28","d29","d30","d31","memory");
  __asm__ volatile("vldmia %0, {d16-d31}"::"r"(p+i+384):"d16","d17","d18","d19","d20","d21","d22","d23","d24","d25","d26","d27","d28","d29","d30","d31","memory");
 }
 for(;i+32<=n;i+=32){
  if(i+256<n)__builtin_prefetch(p+i+256,0,3);
  __asm__ volatile("vld1.8 {d0-d3}, [%0]"::"r"(p+i):"d0","d1","d2","d3","memory");
 }
 for(;i<n;i++){volatile uint8_t x=*(const volatile uint8_t*)(p+i);(void)x;}
}
void obs_neon_read(const uint8_t *p,size_t n){
 static void (*selected)(const uint8_t*,size_t)=NULL;
 if(!selected){const char *v=getenv("OBS_READ_VARIANT");selected=read_2;
  if(v&&!strcmp(v,"vld1-32"))selected=read_0;
  if(v&&!strcmp(v,"vldm-64"))selected=read_1;
  if(v&&!strcmp(v,"vldm-128"))selected=read_2;
  if(v&&!strcmp(v,"vldm-128-hi"))selected=read_3;
  if(v&&!strcmp(v,"vldm-256"))selected=read_4;
  if(v&&!strcmp(v,"vldm-512"))selected=read_5;
 }
 selected(p,n);
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
