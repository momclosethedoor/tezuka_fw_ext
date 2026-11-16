/* FIELD1 collector: no pixel decoding or ARM frame ACK. RAM diagnostics only. */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdbool.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <time.h>
#include <dlfcn.h>
#include <sys/mman.h>
#include <sys/file.h>
#include <sys/ioctl.h>
static volatile uint32_t *r;
static uint32_t rd(unsigned o){return r[o/4];}
static void wr(unsigned o,uint32_t x){r[o/4]=x;__sync_synchronize();}
static uint64_t epoch(void){uint32_t h,l,b;do{h=rd(0x184);l=rd(0x180);b=rd(0x184);}while(h!=b);return ((uint64_t)h<<32)|l;}
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec*1e-9;}
#define LOAD(ret,n,args) ret(*n)args=dlsym(lib,#n);if(!n){fprintf(stderr,"missing %s\n",#n);return 3;}
int main(int argc,char **argv){
 if(argc!=4){fprintf(stderr,"usage: obs-field1 output-prefix timeout-ms tag\n");return 2;}
 unsigned ms=strtoul(argv[2],0,0),tag=strtoul(argv[3],0,0);if(!ms||ms>68719||!tag)return 2;
 int lock=open("/run/obs-field1.lock",O_RDWR|O_CREAT,0600);if(lock<0||flock(lock,LOCK_EX|LOCK_NB))return 2;
 int fd=open("/dev/mem",O_RDWR|O_SYNC);r=mmap(0,4096,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0x7c460000);
 if(r==MAP_FAILED||rd(0x280)!=0x31444c46)return 3;
 /* PHY sysfs callbacks are intentionally denied while PL owns SPI. The
    guard's take receipt contains actual TX readback, and TX routes are off. */
 int guard=open("/dev/obs_fl_guard",O_RDONLY);uint32_t gs[128]={0};
 if(guard<0||ioctl(guard,0x4605,gs)||gs[0]!=2||gs[1]!=2||gs[2]||gs[112]!=1){fprintf(stderr,"guard TX-off receipt required\n");return 3;}
 close(guard);
 void *lib=dlopen("libiio.so.0",RTLD_NOW);if(!lib){fprintf(stderr,"dlopen: %s\n",dlerror());return 3;}
 LOAD(void*,iio_create_local_context,(void));LOAD(void*,iio_context_find_device,(void*,const char*));
 LOAD(void*,iio_device_find_channel,(void*,const char*,bool));LOAD(void,iio_channel_enable,(void*));
 LOAD(int,iio_context_set_timeout,(void*,unsigned));LOAD(int,iio_device_set_kernel_buffers_count,(void*,unsigned));
 LOAD(void*,iio_device_create_buffer,(void*,size_t,bool));LOAD(ssize_t,iio_buffer_refill,(void*));
 LOAD(void*,iio_buffer_start,(void*));LOAD(void*,iio_buffer_end,(void*));
 LOAD(int,iio_buffer_set_blocking_mode,(void*,bool));LOAD(void,iio_buffer_destroy,(void*));LOAD(void,iio_context_destroy,(void*));
 wr(0x290,2);wr(0x284,0);usleep(1000);
 void *ctx=iio_create_local_context();if(!ctx){perror("iio context");return 3;}
 void *dev=iio_context_find_device(ctx,"cf-ad9361-lpc");if(!dev){fprintf(stderr,"ADC device missing\n");return 3;}
 iio_context_set_timeout(ctx,250);iio_device_set_kernel_buffers_count(dev,4);
 void *ch0=iio_device_find_channel(dev,"voltage0",false),*ch1=iio_device_find_channel(dev,"voltage1",false);if(!ch0||!ch1)return 3;
 iio_channel_enable(ch0);iio_channel_enable(ch1);
 void *buf=iio_device_create_buffer(dev,65536,false);if(!buf){perror("iio buffer");return 3;}
 if(iio_buffer_set_blocking_mode(buf,false))return 3;
 ssize_t initial=iio_buffer_refill(buf);
 if(initial!=-EAGAIN){fprintf(stderr,"new generation not empty: %zd\n",initial);iio_buffer_destroy(buf);iio_context_destroy(ctx);return 3;}
 /* stream_run=0 suppresses FM output while a fresh empty DMA generation is created.
    No previous generation can contribute bytes to this context. */
 uint8_t *data=malloc(8*1024*1024);if(!data)return 3;
 char path[512];snprintf(path,sizeof(path),"%s.trace.jsonl",argv[1]);FILE *trace=fopen(path,"wx");if(!trace)return 3;
 wr(0x288,tag);wr(0x28c,ms*62500u);if(rd(0x288)!=tag||rd(0x28c)!=ms*62500u)return 3;
 wr(0x284,1);double start=now(),armed=0,busy=0,done=0;uint64_t target=0;size_t bytes=0;unsigned blocks=0,status=0,serial=0,first=0,end=0;int outcome=0,rc=0;
 /* A target two visits ahead avoids the multi-register write race. */
 target=epoch()+2;wr(0x170,(uint32_t)target);wr(0x174,target>>32);wr(0x290,1);armed=now();
 fprintf(trace,"{\"event\":\"arm\",\"time\":%.9f,\"target_epoch\":%llu,\"tag\":%u,\"timeout_ticks\":%u}\n",armed,(unsigned long long)target,tag,rd(0x28c));
 while(now()-start<2){
  ssize_t n=iio_buffer_refill(buf);double t=now();
  if(n>0){
   uint8_t *p=iio_buffer_start(buf);size_t span=(uint8_t*)iio_buffer_end(buf)-p;
   if((size_t)n>span||bytes+n>8*1024*1024){rc=4;break;}
   memcpy(data+bytes,p,n);fprintf(trace,"{\"event\":\"owned_block\",\"time\":%.9f,\"first\":%zu,\"end\":%zu,\"tag\":%u}\n",t,bytes,bytes+n,tag);bytes+=n;blocks++;
  }else if(n!=-EAGAIN){fprintf(trace,"{\"event\":\"iio_error\",\"code\":%zd}\n",n);rc=4;break;}
  status=rd(0x294);if((status&1)&&!busy)busy=t;
  if(!(status&0x81)&&((status>>1)&7)){
   unsigned s0=rd(0x298),st0=rd(0x294);uint64_t e=((uint64_t)rd(0x2a0)<<32)|rd(0x29c);unsigned rt=rd(0x2a4);first=rd(0x2a8);end=rd(0x2ac);
   if(s0!=rd(0x298)||st0!=rd(0x294))continue;
   serial=s0;outcome=(st0>>1)&7;
   if(!done){done=t;fprintf(trace,"{\"event\":\"pl_done\",\"time\":%.9f,\"outcome\":%d,\"epoch\":%llu,\"tag\":%u,\"first\":%u,\"end\":%u}\n",t,outcome,(unsigned long long)e,rt,first,end);}
   if(e!=target||rt!=tag||rd(0x294)&16){rc=5;break;}
   if(outcome!=1)break;
   if(end<first||end>8*1024*1024){rc=5;break;}
   if(bytes>=end)break;
  }
  usleep(100);
 }
 wr(0x290,2);wr(0x284,0);iio_buffer_destroy(buf);iio_context_destroy(ctx);
 if(outcome==1&&!rc&&bytes>=end){snprintf(path,sizeof(path),"%s.field.real8",argv[1]);FILE *f=fopen(path,"wx");if(!f)rc=6;else{if(fwrite(data+first,1,end-first,f)!=end-first)rc=6;fclose(f);}}
 snprintf(path,sizeof(path),"%s.generation.real8",argv[1]);FILE *f=fopen(path,"wx");if(f){fwrite(data,1,bytes,f);fclose(f);}
 snprintf(path,sizeof(path),"%s.json",argv[1]);f=fopen(path,"wx");if(f){fprintf(f,"{\"outcome\":%d,\"error\":%d,\"serial\":%u,\"target_epoch\":%llu,\"tag\":%u,\"first\":%u,\"end\":%u,\"blocks\":%u,\"owned_bytes\":%zu,\"arm_s\":%.9f,\"busy_observed_s\":%.9f,\"done_s\":%.9f,\"timeout_ms\":%u,\"epoch_after\":%llu}\n",outcome,rc,serial,(unsigned long long)target,tag,first,end,blocks,bytes,armed,busy,done,ms,(unsigned long long)epoch());fclose(f);}
 fclose(trace);free(data);munmap((void*)r,4096);close(fd);close(lock);return rc?rc:(!outcome?7:0);
}
