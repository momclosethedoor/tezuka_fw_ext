/* Bounded diagnostic process, never part of the receive/runtime loop. */
#define _GNU_SOURCE
#include <linux/perf_event.h>
#include <sys/syscall.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <sys/resource.h>
#include <unistd.h>
#include <fcntl.h>
#include <poll.h>
#include <time.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <errno.h>
static double mono(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec*1e-9;}
static void copy_ring(void *to,const uint8_t *data,size_t size,uint64_t tail,size_t n){
 size_t at=tail%size,first=size-at;if(first>n)first=n;memcpy(to,data+at,first);if(n>first)memcpy((uint8_t*)to+first,data,n-first);
}
int main(int argc,char **argv){
 if(argc!=3)return 2;
 char *end;long pid=strtol(argv[1],&end,10);if(*end||pid<2)return 2;
 unsigned seconds=strtoul(argv[2],&end,10);if(*end||!seconds||seconds>15)return 2;
 char path[64],comm[64];snprintf(path,sizeof(path),"/proc/%ld/comm",pid);FILE*f=fopen(path,"r");if(!f)return 3;
 if(!fgets(comm,sizeof(comm),f)||strcmp(comm,"obs-field1\n")){fclose(f);return 2;}fclose(f);
 struct perf_event_attr attr={0};attr.type=PERF_TYPE_SOFTWARE;attr.size=sizeof(attr);attr.config=PERF_COUNT_SW_CPU_CLOCK;
 attr.sample_type=PERF_SAMPLE_IP|PERF_SAMPLE_TID|PERF_SAMPLE_TIME;attr.freq=1;attr.sample_freq=401;attr.disabled=1;attr.exclude_hv=1;attr.wakeup_events=32;
 int fd=syscall(__NR_perf_event_open,&attr,pid,-1,-1,PERF_FLAG_FD_CLOEXEC);if(fd<0){perror("perf_event_open");return 3;}
 size_t page_size=(size_t)sysconf(_SC_PAGESIZE),ring_size=8*page_size;
 struct perf_event_mmap_page *page=mmap(NULL,page_size+ring_size,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0);if(page==MAP_FAILED){perror("perf mmap");close(fd);return 3;}
 uint8_t *data=(uint8_t*)page+page_size;uint64_t tail=0,samples=0,lost=0;int rc=0;
 double start=mono(),deadline=start+seconds;
 printf("PROFILE pid=%ld start=%.9f requested_seconds=%u hz=401\n",pid,start,seconds);
 if(ioctl(fd,PERF_EVENT_IOC_ENABLE,0)){perror("perf enable");munmap(page,page_size+ring_size);close(fd);return 3;}
 for(;;){
  uint64_t head=page->data_head;__sync_synchronize();
  if(head<tail||head-tail>ring_size||head>=UINT32_MAX){fprintf(stderr,"perf ring overflow/extent\n");rc=4;break;}
  while(tail<head){
   struct perf_event_header h;copy_ring(&h,data,ring_size,tail,sizeof(h));
   if(h.size<sizeof(h)||h.size>head-tail){rc=4;break;}
   if(h.type==PERF_RECORD_SAMPLE){
    struct {uint64_t ip;uint32_t pid,tid;uint64_t time;} record;
    if(h.size!=sizeof(h)+sizeof(record)){rc=4;break;}
    copy_ring(&record,data,ring_size,tail+sizeof(h),sizeof(record));samples++;
    printf("S %llx %u %u %llu %u\n",(unsigned long long)record.ip,record.pid,record.tid,(unsigned long long)record.time,h.misc);
   }else if(h.type==PERF_RECORD_LOST){uint64_t record[2];if(h.size<sizeof(h)+sizeof(record)){rc=4;break;}copy_ring(record,data,ring_size,tail+sizeof(h),sizeof(record));lost+=record[1];}
   tail+=h.size;
  }
  __sync_synchronize();page->data_tail=tail;
  if(rc||mono()>=deadline)break;
  struct pollfd p={fd,POLLIN,0};int ready=poll(&p,1,(int)((deadline-mono())*1000)+1);
  if(ready<0&&errno!=EINTR){rc=4;break;}
  if(ready>0&&(p.revents&(POLLERR|POLLNVAL))){rc=4;break;}
  if(ready>0&&(p.revents&POLLHUP)&&page->data_head==tail)break;
 }
 ioctl(fd,PERF_EVENT_IOC_DISABLE,0);struct rusage usage;getrusage(RUSAGE_SELF,&usage);
 printf("PROFILE_END samples=%llu lost=%llu seconds=%.9f profiler_user_s=%.6f profiler_system_s=%.6f error=%d\n",(unsigned long long)samples,(unsigned long long)lost,mono()-start,usage.ru_utime.tv_sec+usage.ru_utime.tv_usec/1e6,usage.ru_stime.tv_sec+usage.ru_stime.tv_usec/1e6,rc);
 munmap(page,page_size+ring_size);close(fd);return rc;
}
