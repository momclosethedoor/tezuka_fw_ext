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
#include <sys/socket.h>
#include <sys/uio.h>
#include <poll.h>
#include "obs-watch-io.h"
#include "obs-transport-queue.h"
static struct transport_queue queue;
#include <sys/resource.h>
#include <signal.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>
static volatile uint32_t *r;
/* This bench uses the verified Fs=61.44 MS/s, real8 DDC4 contract. */
#define REAL8_FS (61440000u/4u)
#define CAPTURE_SECONDS 2u
#define IIO_BLOCK_SAMPLES 65536u
static uint32_t rd(unsigned o){return r[o/4];}
static void wr(unsigned o,uint32_t x){r[o/4]=x;__sync_synchronize();}
static uint64_t epoch(void){uint32_t h,l,b;do{h=rd(0x184);l=rd(0x180);b=rd(0x184);}while(h!=b);return ((uint64_t)h<<32)|l;}
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec*1e-9;}
/* One TCP session carries many attempts; the IIO block remains owned until
 * local TCP accepts it, or it is copied to a bounded reusable transport block. */
static int video_sock=-1;
static uint64_t wire_bytes=0,wire_chunks=0,wire_accepted=0;
static double wire_send_s=0,wire_send_cpu_s=0;
static int cpu_phases=0;
static size_t tcp_pump_bytes=0;
static const char *managed_boot=NULL;
static double thread_cpu(void){struct timespec t;clock_gettime(CLOCK_THREAD_CPUTIME_ID,&t);return t.tv_sec+t.tv_nsec*1e-9;}
static int sendall(int s,const void *p,size_t n){const uint8_t *b=p;while(n){ssize_t k=send(s,b,n,MSG_NOSIGNAL);if(k<0&&errno==EINTR)continue;if(k<=0)return -1;b+=k;n-=k;}return 0;}
static int connect_video(void){
 if(video_sock>=0)return 0;
 int s=socket(AF_INET,SOCK_STREAM,0);if(s<0)return -1;
 struct timeval tv={1,0};int one=1;
 setsockopt(s,SOL_SOCKET,SO_SNDTIMEO,&tv,sizeof(tv));setsockopt(s,IPPROTO_TCP,TCP_NODELAY,&one,sizeof(one));
 const char *host=managed_boot?getenv("OBS_FIELD1_HOST"):"192.168.2.10";
 const char *port=managed_boot?getenv("OBS_FIELD1_PORT"):NULL;
 unsigned long number=port?strtoul(port,NULL,10):18009;
 struct sockaddr_in a={.sin_family=AF_INET,.sin_port=htons(number)};
 if(!host||!number||number>65535||inet_pton(AF_INET,host,&a.sin_addr)!=1){close(s);return -1;}
 if(connect(s,(void*)&a,sizeof(a))){close(s);return -1;}if(fcntl(s,F_SETFL,fcntl(s,F_GETFL,0)|O_NONBLOCK)){close(s);return -1;}video_sock=s;return 0;
}
/* One active packet references either IIO (direct variant) or a protected
 * transport block. Header and metadata storage are fixed. */
static struct {uint32_t header[3];char meta[2048];struct iovec vec[3];
 struct msghdr msg;double started;size_t wire_size;int active;} tx;
static void packet_abort(void){if(video_sock>=0)close(video_sock);video_sock=-1;tx.active=0;}
static int packet_begin(const char *meta,const void *raw,size_t n){
 size_t m=strlen(meta);if(tx.active||m>=sizeof(tx.meta)||video_sock<0)return -1;
 memcpy(tx.meta,meta,m);tx.header[0]=0x32503852;tx.header[1]=(uint32_t)m;tx.header[2]=(uint32_t)n;
 tx.vec[0]=(struct iovec){tx.header,sizeof(tx.header)};tx.vec[1]=(struct iovec){tx.meta,m};tx.vec[2]=(struct iovec){(void*)raw,n};
 memset(&tx.msg,0,sizeof(tx.msg));tx.msg.msg_iov=tx.vec;tx.msg.msg_iovlen=n?3:2;
 tx.started=now();tx.wire_size=sizeof(tx.header)+m+n;tx.active=1;return 0;
}
/* One nonblocking syscall per pump. 0=pending, 1=finished, -1=failed. */
static int packet_pump(void){
 if(!tx.active)return 1;
 if(now()-tx.started>1.0){packet_abort();return -1;}
 double cpu0=cpu_phases?thread_cpu():0;
 /* Limit work in one kernel call, independently of the R8P2 block size.
    IIO service gets another turn after each accepted slice, without a timer.
    MSG_MORE retains only the unfinished packet's TCP tail. */
 struct msghdr call=tx.msg;struct iovec pieces[3];int more=0;
 if(tcp_pump_bytes){
  size_t left=tcp_pump_bytes,count=0;
  for(size_t i=0;i<tx.msg.msg_iovlen;i++){
   if(!left){more=1;break;}
   pieces[count]=tx.msg.msg_iov[i];if(pieces[count].iov_len>left){pieces[count].iov_len=left;more=1;}
   left-=pieces[count].iov_len;count++;
  }
  call.msg_iov=pieces;call.msg_iovlen=count;
 }
 ssize_t k=sendmsg(video_sock,&call,MSG_NOSIGNAL|(more?MSG_MORE:0));
 if(cpu_phases)wire_send_cpu_s+=thread_cpu()-cpu0;
 if(k<0&&(errno==EAGAIN||errno==EWOULDBLOCK||errno==EINTR))return 0;
 if(k<=0){packet_abort();return -1;}
 wire_accepted+=(size_t)k;
 size_t used=(size_t)k;
 while(tx.msg.msg_iovlen&&used>=tx.msg.msg_iov[0].iov_len){used-=tx.msg.msg_iov[0].iov_len;tx.msg.msg_iov++;tx.msg.msg_iovlen--;}
 if(used&&tx.msg.msg_iovlen){tx.msg.msg_iov[0].iov_base=(uint8_t*)tx.msg.msg_iov[0].iov_base+used;tx.msg.msg_iov[0].iov_len-=used;}
 if(tx.msg.msg_iovlen)return 0;
 wire_send_s+=now()-tx.started;wire_bytes+=tx.wire_size;wire_chunks++;tx.active=0;return 1;
}
/* Control packets are bounded setup/teardown work, outside live block service. */
static int packet(const char *meta,const void *raw,size_t n){
 if(packet_begin(meta,raw,n))return -1;
 for(;;){
  int rc=packet_pump();if(rc)return rc>0?0:-1;
  struct pollfd fd={video_sock,POLLOUT,0};
  double remaining=1.0-(now()-tx.started);if(remaining<=0){packet_abort();return -1;}
  int ready=poll(&fd,1,(int)(remaining*1000)+1);
  if(ready<0&&errno!=EINTR){packet_abort();return -1;}
 }
}
/* Session metadata may be reused; every attempt still destroys its old DMA
   buffer, creates an empty generation and assigns a fresh stream tag. */
static void *session_context=NULL,*session_library=NULL;
static int reuse_context=0;
static struct watch_input watching;
static int watch_active=0,watch_owner=-1;
static double watch_until=0;
static unsigned watch_position=0;
static struct watch_target watch_selected;
static uint64_t watch_revision=0;
static unsigned watch_saved_nco,watch_saved_code;static int watch_saved=0;
/* 11: explicitly removed/replaced; 12: bounded bench window ended.
 * Neither completion nor cancellation consumes a desired-state flag. */
static int watch_check(void){
 if(!watch_active)return 0;
 if(now()>=watch_until)return 12;
 if(watch_refresh(&watching))return 13;
 if(watching.map.version!=rd(0x22c)||watching.map.count!=rd(0x1e8))return 13;
 return watch_same(&watching.map,watch_position,&watch_selected)?0:11;
}
static void context_done(void (*destroy)(void*),void *ctx){if(!reuse_context)destroy(ctx);}
static void library_done(void *lib){if(!reuse_context)dlclose(lib);}
static int missed_early(const char *prefix,unsigned tag,uint64_t target,uint64_t current,double setup){
 char path[768];snprintf(path,sizeof(path),"%s.json",prefix);FILE *f=fopen(path,"wx");if(!f)return 3;
 fprintf(f,"{\"outcome\":0,\"error\":10,\"not_armed\":true,\"target_epoch\":%llu,\"tag\":%u,\"epoch_after\":%llu,\"setup_s\":%.9f}\n",(unsigned long long)target,tag,(unsigned long long)current,setup);fclose(f);
 snprintf(path,sizeof(path),"%s.trace.jsonl",prefix);f=fopen(path,"wx");if(!f)return 3;
 fprintf(f,"{\"event\":\"missed_before_iio\",\"time\":%.9f,\"requested_epoch\":%llu,\"current_epoch\":%llu}\n",now(),(unsigned long long)target,(unsigned long long)current);fclose(f);return 10;
}
#define LOAD(ret,n,args) ret(*n)args=dlsym(lib,#n);if(!n){fprintf(stderr,"missing %s\n",#n);return 3;}
static int capture(int argc,char **argv){
 double capture_enter=now();
 if(argc!=4&&argc!=5){fprintf(stderr,"usage: obs-field1 output-prefix timeout-ms tag [--unqualified-raw]\n");return 2;}
 if(argc==5&&strcmp(argv[4],"--unqualified-raw"))return 2;
 if(strspn(argv[1],"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/_-.")!=strlen(argv[1]))return 2;

 int copy_only=getenv("OBS_FIELD1_COPY_ONLY")&&!strcmp(getenv("OBS_FIELD1_COPY_ONLY"),"1");
 int no_delivery=getenv("OBS_FIELD1_NO_DELIVERY")&&!strcmp(getenv("OBS_FIELD1_NO_DELIVERY"),"1");
 if(copy_only)no_delivery=1;
 const char *copy=getenv("OBS_IIO_COPY_WIDTH");unsigned width=copy?strtoul(copy,NULL,10):0;
 if(width!=0&&width!=32&&width!=64&&width!=128)return 2;
 queue.copy_width=width;
 const char *route_case=getenv("OBS_FIELD1_ROUTE_CASE");
 int next_on_busy=getenv("OBS_FIELD1_NEXT_ON_BUSY")&&!strcmp(getenv("OBS_FIELD1_NEXT_ON_BUSY"),"1");
 int route_sock=-1,route_acked=0;unsigned route_id=0;char route_reply[128];size_t route_have=0;
 if(route_case&&strcmp(route_case,"immediate")&&strcmp(route_case,"delayed")&&strcmp(route_case,"missing_then_correct"))return 2;
 int unqualified_raw=argc==5||(getenv("OBS_FIELD1_UNQUALIFIED_RAW")&&!strcmp(getenv("OBS_FIELD1_UNQUALIFIED_RAW"),"1"));
 FILE *policy=fopen("/tmp/obs007a-field-system/unqualified-raw","r");if(policy){unqualified_raw|=fgetc(policy)=='1';fclose(policy);}
 unsigned ms=strtoul(argv[2],0,0),tag=strtoul(argv[3],0,0);if(!ms||ms>68719||!tag)return 2;
 int lock=watch_active?dup(watch_owner):open("/run/obs-field1.lock",O_RDWR|O_CREAT,0600);if(lock<0||flock(lock,LOCK_EX|LOCK_NB))return 2;
 int fd=open("/dev/mem",O_RDWR|O_SYNC);r=mmap(0,4096,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0x7c460000);
 if(r==MAP_FAILED||rd(0x280)!=0x31444c46)return 3;
 /* PHY sysfs callbacks are intentionally denied while PL owns SPI. The
    guard's take receipt contains actual TX readback, and TX routes are off. */
 int guard=open("/dev/obs_fl_guard",O_RDONLY);uint32_t gs[128]={0};
 int tx_authorized=getenv("OBS_FIELD1_TX_AUTHORIZED")&&!strcmp(getenv("OBS_FIELD1_TX_AUTHORIZED"),"1");
 if(guard<0||ioctl(guard,0x4605,gs)||gs[0]!=2||gs[1]!=2||gs[2]||gs[112]!=(tx_authorized?0u:1u)){fprintf(stderr,"guard TX receipt differs from explicit series authorization\n");return 3;}
 close(guard);
 /* Exact schedules can be offered seconds in advance. Wait with STREAM_RUN
    still off, before allocating/arming IIO, so the bounded capture is not
    consumed by pre-target bytes. Late admission is counted below. */
 const char *scheduled=getenv("OBS_FIELD1_TARGET_EPOCH");
 if(scheduled){
  uint64_t requested=strtoull(scheduled,NULL,10);double wait_start=now();
  while(epoch()+512<requested && now()-wait_start<5.0){__asm__ __volatile__("yield" ::: "memory");}
  uint64_t current=epoch();if(requested<current+2){
   int miss=missed_early(argv[1],tag,requested,current,now()-capture_enter);
   munmap((void*)r,4096);close(fd);close(lock);return miss;
  }
 }
 double prepare_start=now();
 void *lib=session_library?session_library:dlopen("libiio.so.0",RTLD_NOW);if(!lib){fprintf(stderr,"dlopen: %s\n",dlerror());return 3;}
 LOAD(void*,iio_create_local_context,(void));LOAD(void*,iio_context_find_device,(void*,const char*));
 LOAD(void*,iio_device_find_channel,(void*,const char*,bool));LOAD(void,iio_channel_enable,(void*));
 LOAD(int,iio_context_set_timeout,(void*,unsigned));LOAD(int,iio_device_set_kernel_buffers_count,(void*,unsigned));
 LOAD(void*,iio_device_create_buffer,(void*,size_t,bool));LOAD(ssize_t,iio_buffer_refill,(void*));
 LOAD(void*,iio_buffer_start,(void*));LOAD(void*,iio_buffer_end,(void*));
 LOAD(int,iio_buffer_get_poll_fd,(void*));
 LOAD(int,iio_buffer_set_blocking_mode,(void*,bool));LOAD(void,iio_buffer_destroy,(void*));LOAD(void,iio_context_destroy,(void*));
 uint32_t old_mode=rd(0x38);wr(0x290,2);
 double cancel_deadline=now()+0.1;
 while(rd(0x294)&0xc1){if(now()>cancel_deadline){fprintf(stderr,"FIELD CANCEL deadline\n");return 3;}}
 wr(0x284,0);wr(0x38,1);
 if(rd(0x284)!=0||rd(0x38)!=1){fprintf(stderr,"stream/mode readback\n");return 3;}
 void *ctx=session_context?session_context:iio_create_local_context();if(!ctx){perror("iio context");return 3;}
 double context_ready=now();if(reuse_context){session_context=ctx;session_library=lib;}
 void *dev=iio_context_find_device(ctx,"cf-ad9361-lpc");if(!dev){fprintf(stderr,"ADC device missing\n");return 3;}
 unsigned kernel_buffers=4;const char *depth_env=getenv("OBS_IIO_KERNEL_BUFFERS");
 if(depth_env){kernel_buffers=strtoul(depth_env,NULL,10);if(kernel_buffers<2||kernel_buffers>16||(kernel_buffers&(kernel_buffers-1)))return 3;}
 if(iio_context_set_timeout(ctx,250)||iio_device_set_kernel_buffers_count(dev,kernel_buffers))return 3;
 void *ch0=iio_device_find_channel(dev,"voltage0",false),*ch1=iio_device_find_channel(dev,"voltage1",false);if(!ch0||!ch1)return 3;
 iio_channel_enable(ch0);iio_channel_enable(ch1);
 unsigned block_samples=IIO_BLOCK_SAMPLES;const char *block_env=getenv("OBS_IIO_BLOCK_SAMPLES");
 if(block_env){block_samples=strtoul(block_env,NULL,10);if(block_samples<2048||block_samples>65536||(block_samples&(block_samples-1)))return 3;}
 void *buf=iio_device_create_buffer(dev,block_samples,false);if(!buf){perror("iio buffer");return 3;}
 double buffer_ready=now();
 size_t block_bytes=(uint8_t*)iio_buffer_end(buf)-(uint8_t*)iio_buffer_start(buf);
 size_t capture_limit=REAL8_FS*CAPTURE_SECONDS+block_bytes;
 unsigned transport_depth=0;const char *depth=getenv("OBS_TX_BLOCKS");
 if(depth){transport_depth=strtoul(depth,NULL,10);if(transport_depth>8||(transport_depth>1&&(transport_depth&(transport_depth-1))))return 3;}
 if(tq_reset(&queue,transport_depth,block_bytes))return 3;
 if(iio_buffer_set_blocking_mode(buf,false))return 3;
 int iio_poll_fd=iio_buffer_get_poll_fd(buf);if(iio_poll_fd<0){fprintf(stderr,"IIO poll fd unavailable\n");return 3;}
 ssize_t initial=iio_buffer_refill(buf);
 if(initial!=-EAGAIN){fprintf(stderr,"new generation not empty: %zd\n",initial);iio_buffer_destroy(buf);context_done(iio_context_destroy,ctx);return 3;}
 /* stream_run=0 suppresses FM output while a fresh empty DMA generation is created.
    No previous generation can contribute bytes to this context. */
 uint64_t sent_before=wire_bytes,chunks_before=wire_chunks;double send_before=wire_send_s,send_cpu_before=wire_send_cpu_s;
 if(!no_delivery&&connect_video()){iio_buffer_destroy(buf);context_done(iio_context_destroy,ctx);wr(0x38,old_mode);close(fd);close(lock);return 9;}
 char path[512];snprintf(path,sizeof(path),"%s.trace.jsonl",argv[1]);FILE *trace=managed_boot?fopen("/dev/null","w"):fopen(path,"wx");if(!trace)return 3;
 fprintf(trace,"{\"event\":\"iio_preparation\",\"time\":%.9f,\"context_s\":%.9f,\"buffer_s\":%.9f,\"ready_s\":%.9f,\"reuse_context\":%s}\n",now(),context_ready-prepare_start,buffer_ready-context_ready,now()-prepare_start,reuse_context?"true":"false");
 fprintf(trace,"{\"event\":\"iio_geometry\",\"time\":%.9f,\"kernel_buffers\":%u,\"block_bytes\":%zu}\n",now(),kernel_buffers,block_bytes);
 fprintf(trace,"{\"event\":\"raw_policy_latched\",\"time\":%.9f,\"unqualified_raw\":%s}\n",now(),unqualified_raw?"true":"false");
 wr(0x288,tag);wr(0x28c,ms*62500u);if(rd(0x288)!=tag||rd(0x28c)!=ms*62500u)return 3;
 double start=now(),armed=0,busy=0,done=0;uint64_t target=0;size_t bytes=0;unsigned blocks=0,status=0,serial=0,first=0,end=0,target_position=0,map_version=0;int outcome=0,rc=0,watch_cancelled=0;uint64_t target_hz=0;
 /* A target two visits ahead avoids the multi-register write race. */
 uint64_t current_epoch;unsigned current_position;do{current_epoch=epoch();current_position=rd(0x234);}while(current_epoch!=epoch());
 unsigned advance=2;const char *want_env=getenv("OBS_FIELD1_POSITION"),*count_env=getenv("OBS_FIELD1_POSITIONS");
 if(want_env&&count_env){unsigned wanted=strtoul(want_env,0,0),count=strtoul(count_env,0,0);if(!count||count>128||wanted>=count){wr(0x284,0);iio_buffer_destroy(buf);context_done(iio_context_destroy,ctx);return 3;}while((current_position+advance)%count!=wanted)advance++;}
 if(watch_active){
  if(watch_refresh(&watching)||watching.map.version!=rd(0x22c)||watching.map.count!=rd(0x1e8))rc=13;
  int selected=rc?-1:watch_next(&watching.map,current_position,&advance);
  if(selected<0&&!rc)rc=11;
  if(!rc){
   watch_position=(unsigned)selected;watch_selected=watching.map.target[selected];watch_revision=watching.map.revision;
   target_hz=watch_selected.hz;target_position=watch_position;map_version=watching.map.version;
   if(!watch_saved){watch_saved_nco=rd(0x30);watch_saved_code=rd(0x40);watch_saved=1;}
   wr(0x30,watch_selected.nco);wr(0x40,watch_selected.code);
   if(rd(0x30)!=watch_selected.nco||rd(0x40)!=watch_selected.code)rc=13;
  }
  if(rc){iio_buffer_destroy(buf);context_done(iio_context_destroy,ctx);wr(0x38,old_mode);fclose(trace);library_done(lib);munmap((void*)r,4096);close(fd);close(lock);return rc;}
 }
 target=current_epoch+advance;
 const char *exact=getenv("OBS_FIELD1_TARGET_EPOCH");
 if(exact){
  target=strtoull(exact,NULL,10);
  if(target<current_epoch+2){
   fprintf(trace,"{\"event\":\"missed_requested_visit\",\"time\":%.9f,\"requested_epoch\":%llu,\"current_epoch\":%llu,\"setup_s\":%.9f}\n",now(),(unsigned long long)target,(unsigned long long)current_epoch,now()-capture_enter);
   snprintf(path,sizeof(path),"%s.json",argv[1]);FILE *miss=fopen(path,"wx");
   if(miss){fprintf(miss,"{\"outcome\":0,\"error\":10,\"not_armed\":true,\"target_epoch\":%llu,\"tag\":%u,\"epoch_after\":%llu,\"setup_s\":%.9f}\n",(unsigned long long)target,tag,(unsigned long long)current_epoch,now()-capture_enter);fclose(miss);}
   wr(0x284,0);iio_buffer_destroy(buf);context_done(iio_context_destroy,ctx);wr(0x38,old_mode);
   fclose(trace);library_done(lib);munmap((void*)r,4096);close(fd);close(lock);return 10;
  }
 }
 wr(0x284,1);
 fprintf(trace,"{\"event\":\"generation_ready\",\"time\":%.9f,\"setup_s\":%.9f,\"requested_epoch\":%llu}\n",now(),now()-capture_enter,(unsigned long long)target);
 if(route_case){
  uint64_t marker_epoch=((uint64_t)rd(0x250)<<32)|rd(0x24c);
  route_id=rd(0x240);if(!(rd(0x228)&8)||marker_epoch!=current_epoch){fprintf(stderr,"current-visit field needs matching hardware antenna marker\n");return 3;}
  target=current_epoch;
  route_sock=socket(AF_INET,SOCK_STREAM,0);struct timeval tv={1,0};setsockopt(route_sock,SOL_SOCKET,SO_SNDTIMEO,&tv,sizeof(tv));
  struct sockaddr_in a={.sin_family=AF_INET,.sin_port=htons(18008)};inet_pton(AF_INET,"192.168.2.10",&a.sin_addr);
  if(connect(route_sock,(void*)&a,sizeof(a)))return 3;
 }
 if(!no_delivery){
  char watch_meta[320]="";
  if(watch_active){int len=snprintf(watch_meta,sizeof(watch_meta),"\"target_hz\":%llu,\"target_position\":%u,\"map_version\":%u,\"watch_revision\":%llu,\"watch_generation\":%llu,",(unsigned long long)target_hz,target_position,map_version,(unsigned long long)watch_revision,(unsigned long long)watch_selected.generation);
   if(managed_boot&&len>0&&(size_t)len<sizeof(watch_meta))snprintf(watch_meta+len,sizeof(watch_meta)-len,"\"system_boot\":\"%s\",",managed_boot);}
  char meta[1024];snprintf(meta,sizeof(meta),"{\"kind\":\"begin\",\"delivery_id\":\"field-%u-%llu\",\"stream_tag\":%u,\"epoch\":%llu,\"fs\":%u,\"code\":%u,\"g\":0,%s\"block_bytes\":%zu,\"capture_seconds\":%u,\"capture_limit_bytes\":%zu,\"unqualified_raw\":%s,\"replay\":false}",tag,(unsigned long long)target,tag,(unsigned long long)target,REAL8_FS,rd(0x40)&63,watch_meta,block_bytes,CAPTURE_SECONDS,capture_limit,unqualified_raw?"true":"false");
  if(packet(meta,NULL,0)){wr(0x284,0);iio_buffer_destroy(buf);context_done(iio_context_destroy,ctx);wr(0x38,old_mode);fclose(trace);close(fd);close(lock);return 9;}
 }
 /* BEGIN transport can yield. Recheck the exact visit immediately before
    FIELD ARM so a socket delay becomes a counted missed request. */
 if((exact||watch_active) && target<epoch()+2)rc=10;
 if(watch_active&&!rc)rc=watch_check();
 if(!rc){wr(0x170,(uint32_t)target);wr(0x174,target>>32);wr(0x290,1);armed=now();}
 fprintf(trace,"{\"event\":\"arm_attempt\",\"time\":%.9f,\"target_epoch\":%llu,\"tag\":%u,\"timeout_ticks\":%u}\n",armed,(unsigned long long)target,tag,rd(0x28c));
 size_t pending_bytes=0,received_bytes=0,held_bytes=0,transport_rejected_bytes=0;
 unsigned send_wait_polls=0,received_blocks=0;
 double refill_s=0,copy_s=0,copy_cpu_s=0,queue_wait_s=0,iio_hold_max_s=0,held_since=0,return_due=0;int input_closed=0;
 double loop_due=now(),loop_gap_max_s=0;unsigned copied_refills=0,send_with_iio_owned=0;int fault_seen=0;
 double ready_wait_s=0,ready_wait_max_s=0;unsigned ready_wait_calls=0,progress_turns=0;
 double service_started=now();struct timespec cpu_begin,cpu_end;clock_gettime(CLOCK_THREAD_CPUTIME_ID,&cpu_begin);
 while(!rc&&now()-start<CAPTURE_SECONDS){
  if(watch_active&&!watch_cancelled){
   watch_cancelled=watch_check();
   if(watch_cancelled){
    wr(0x290,2);wr(0x284,0);
    if(buf){iio_buffer_destroy(buf);buf=NULL;}input_closed=1;held_bytes=0;return_due=0;
    fprintf(trace,"{\"event\":\"watch_cancel\",\"time\":%.9f,\"reason\":%d,\"watch_revision\":%llu,\"current_revision\":%llu}\n",now(),watch_cancelled,(unsigned long long)watch_revision,(unsigned long long)watching.map.revision);
   }
  }
  size_t received_before=received_bytes;uint64_t accepted_before=wire_accepted;
  double tick=now();if(tick-loop_due>loop_gap_max_s)loop_gap_max_s=tick-loop_due;loop_due=tick;
  if(transport_depth){
   /* Drain a bounded burst before socket work. Each copied DMA block is
      returned by the very next refill, including when refill yields EAGAIN.
      Only depth=1 or sustained overload can leave a newly dequeued block held. */
   unsigned budget=kernel_buffers+1;
   while(!input_closed&&budget--){
    if(!held_bytes&&(!outcome||(outcome==1&&received_bytes<end)||return_due)){
     double t0=now();ssize_t n=iio_buffer_refill(buf);double t1=now();refill_s+=t1-t0;
     if(return_due){if(t1-return_due>iio_hold_max_s)iio_hold_max_s=t1-return_due;return_due=0;copied_refills++;}
     if(n>0){held_bytes=(size_t)n;held_since=t1;}else if(n!=-EAGAIN){rc=4;break;}
    }
    if(!held_bytes||!(transport_depth>1||!queue.count))break;
    uint8_t *p=iio_buffer_start(buf);size_t span=(uint8_t*)iio_buffer_end(buf)-p;
    if(held_bytes>span||received_bytes+held_bytes>capture_limit){rc=4;break;}
    /* Managed FIELD must end with an explicit error if its bounded transport
       saturates. Keep the already accepted blocks and never evict a prefix. */
    if(watch_active&&!no_delivery&&queue.count==queue.capacity){
     transport_rejected_bytes=held_bytes;rc=14;break;
    }
    double t0=now(),cpu0=cpu_phases?thread_cpu():0;int pushed=no_delivery&&!copy_only?0:tq_push(&queue,p,held_bytes,received_bytes);copy_s+=now()-t0;
    if(cpu_phases)copy_cpu_s+=thread_cpu()-cpu0;
    if(pushed){rc=4;break;}
    received_bytes+=held_bytes;received_blocks++;if(no_delivery){bytes=received_bytes;blocks=received_blocks;}
    return_due=held_since;held_bytes=0;
   }
   if(rc)break;
   /* Always return the last copied block before touching the socket, even
      when the burst budget is exhausted. Preserve any newly dequeued block. */
   if(return_due&&!input_closed){
    double t0=now();ssize_t n=iio_buffer_refill(buf);double t1=now();refill_s+=t1-t0;
    if(t1-return_due>iio_hold_max_s)iio_hold_max_s=t1-return_due;
    return_due=0;copied_refills++;
    if(n>0){held_bytes=(size_t)n;held_since=t1;}else if(n!=-EAGAIN){rc=4;break;}
   }
   if(copy_only){while(queue.count){if(tq_start(&queue)<0||tq_finish(&queue)){rc=4;break;}}if(rc)break;}
   if(!pending_bytes&&queue.count){
    int slot=tq_start(&queue);if(slot<0){rc=4;break;}
    struct tq_entry *e=&queue.entry[slot];char meta[160];snprintf(meta,sizeof(meta),"{\"kind\":\"data\",\"stream_tag\":%u,\"offset\":%zu}",tag,e->offset);
    if(packet_begin(meta,queue.memory+(size_t)slot*queue.stride,e->length)){rc=9;break;}pending_bytes=e->length;
   }
  }else{
   double t0=now();ssize_t n=pending_bytes||(outcome&&outcome!=1)?-EAGAIN:iio_buffer_refill(buf);refill_s+=now()-t0;
   if(n>0){
    uint8_t *p=iio_buffer_start(buf);size_t span=(uint8_t*)iio_buffer_end(buf)-p;
    if((size_t)n>span||bytes+(size_t)n>capture_limit){rc=4;break;}
    held_since=now();received_bytes+=(size_t)n;received_blocks++;
    if(!no_delivery){char meta[160];snprintf(meta,sizeof(meta),"{\"kind\":\"data\",\"stream_tag\":%u,\"offset\":%zu}",tag,bytes);if(packet_begin(meta,p,(size_t)n)){rc=9;break;}pending_bytes=(size_t)n;}
    else{bytes+=(size_t)n;blocks++;}
   }else if(n!=-EAGAIN){rc=4;break;}
  }
  if(pending_bytes){if(transport_depth&&held_bytes)send_with_iio_owned++;double t0=now();int sent=packet_pump();queue_wait_s+=now()-t0;if(sent<0){rc=9;break;}
   if(sent){bytes+=pending_bytes;pending_bytes=0;blocks++;if(transport_depth){if(tq_finish(&queue)){rc=4;break;}}else if(now()-held_since>iio_hold_max_s)iio_hold_max_s=now()-held_since;}
   else send_wait_polls++;
  }
  if(watch_cancelled){if(pending_bytes||queue.count)goto idle_service;break;}
  double t;
  t=now();status=rd(0x294);
  if((status&16)&&!fault_seen){fault_seen=1;fprintf(trace,"{\"event\":\"first_stream_fault\",\"time\":%.9f,\"status\":%u,\"received_bytes\":%zu,\"sent_bytes\":%zu,\"queue_blocks\":%u,\"held_bytes\":%zu}\n",t,status,received_bytes,bytes,queue.count,held_bytes);}
  if((status&1)&&!busy){busy=t;target_position=rd(0x234);map_version=rd(0x22c);
   if(watch_active&&(target_position!=watch_position||map_version!=watching.map.version)){rc=13;break;}
   fprintf(trace,"{\"event\":\"busy\",\"time\":%.9f,\"epoch\":%llu,\"position\":%u,\"map_version\":%u,\"field_status\":%u,\"fld_status\":%u,\"marker_id\":%u}\n",t,(unsigned long long)epoch(),target_position,map_version,status,rd(0x228),rd(0x240));
   if(next_on_busy){int g=open("/dev/obs_fl_guard",O_RDWR);unsigned v[128]={0};int result=g<0?-1:ioctl(g,0x4603,v);if(g>=0)close(g);fprintf(trace,"{\"event\":\"map_next_during_busy\",\"time\":%.9f,\"result\":%d,\"epoch\":%llu,\"field_status\":%u,\"version\":%u}\n",now(),result,(unsigned long long)epoch(),rd(0x294),rd(0x22c));if(result){rc=8;break;}}
   if(route_case){char msg[512];int n=snprintf(msg,sizeof(msg),"{\"marker_id\":%u,\"epoch\":%llu,\"version\":%u,\"position\":%u,\"route\":%u,\"a9_send_s\":%.9f,\"test\":\"%s\"}\n",route_id,(unsigned long long)target,map_version,target_position,rd(0x244),now(),route_case);if(sendall(route_sock,msg,n)){rc=8;break;}fprintf(trace,"{\"event\":\"route_request\",\"time\":%.9f,\"marker_id\":%u}\n",now(),route_id);}
  }
  if(route_sock>=0&&!route_acked&&busy){
   ssize_t n=recv(route_sock,route_reply+route_have,sizeof(route_reply)-1-route_have,MSG_DONTWAIT);
   if(n>0){route_have+=n;route_reply[route_have]=0;if(strchr(route_reply,'\n')){unsigned ack=0;if(sscanf(route_reply,"ACK %u",&ack)!=1||ack!=route_id||rd(0x240)!=route_id||!(rd(0x228)&8)){rc=8;break;}unsigned fs=rd(0x294);wr(0x218,ack);wr(0x220,2);route_acked=1;fprintf(trace,"{\"event\":\"host_ack_applied\",\"time\":%.9f,\"marker_id\":%u,\"field_status_before\":%u,\"field_status_after\":%u,\"fld_status_after\":%u,\"epoch_after\":%llu}\n",now(),ack,fs,rd(0x294),rd(0x228),(unsigned long long)epoch());}}
   else if(n==0||(n<0&&errno!=EAGAIN&&errno!=EWOULDBLOCK)){rc=8;break;}
  }
  if(!(status&0x81)&&((status>>1)&7)){
   unsigned s0=rd(0x298),st0=rd(0x294);uint64_t e=((uint64_t)rd(0x2a0)<<32)|rd(0x29c);unsigned rt=rd(0x2a4);first=rd(0x2a8);end=rd(0x2ac);
   if(s0!=rd(0x298)||st0!=rd(0x294))continue;
   serial=s0;outcome=(st0>>1)&7;
   if(!done){done=t;fprintf(trace,"{\"event\":\"pl_done\",\"time\":%.9f,\"outcome\":%d,\"epoch\":%llu,\"tag\":%u,\"first\":%u,\"end\":%u}\n",t,outcome,(unsigned long long)e,rt,first,end);}
   unsigned final_status=rd(0x294);
   if(e!=target||rt!=tag||(final_status&16)){
    fprintf(trace,"{\"event\":\"field_rejected\",\"time\":%.9f,\"status\":%u,\"expected_epoch\":%llu,\"actual_epoch\":%llu,\"expected_tag\":%u,\"actual_tag\":%u}\n",now(),final_status,(unsigned long long)target,(unsigned long long)e,tag,rt);rc=5;break;
   }
   if(outcome!=1){
    if(transport_depth&&!input_closed){wr(0x284,0);iio_buffer_destroy(buf);buf=NULL;input_closed=1;held_bytes=0;return_due=0;}
    if(pending_bytes||queue.count)goto idle_service;
    if(!route_case||route_acked)break;else continue;
   }
   if(end<first||end>capture_limit){rc=5;break;}
   if(transport_depth&&!input_closed&&received_bytes>=end){
    wr(0x284,0);double t0=now();iio_buffer_destroy(buf);buf=NULL;input_closed=1;
    if(return_due&&now()-return_due>iio_hold_max_s){iio_hold_max_s=now()-return_due;}return_due=0;
    fprintf(trace,"{\"event\":\"iio_released_before_transport\",\"time\":%.9f,\"destroy_s\":%.9f,\"queued_blocks\":%u}\n",now(),now()-t0,queue.count);
   }
   if(bytes>=end&&!pending_bytes&&!queue.count&&(!route_case||route_acked))break;
  }
 idle_service:
  /* Never delay ready work. Only the descriptors that can unblock service
     enter poll; its timeout is the capture/packet deadline, not a cadence. */
  if(received_bytes!=received_before||wire_accepted!=accepted_before){progress_turns++;continue;}
  struct pollfd ready_fds[4];nfds_t ready_count=0;
  if(!input_closed&&!held_bytes&&(transport_depth||!pending_bytes))
   ready_fds[ready_count++]=(struct pollfd){iio_poll_fd,POLLIN|POLLOUT,0};
  if(tx.active)ready_fds[ready_count++]=(struct pollfd){video_sock,POLLOUT,0};
  if(route_sock>=0&&!route_acked)ready_fds[ready_count++]=(struct pollfd){route_sock,POLLIN,0};
  if(watch_active&&!watch_cancelled)ready_fds[ready_count++]=(struct pollfd){watching.fd,POLLIN,0};
  if(ready_count){
   double limit=start+CAPTURE_SECONDS;if(tx.active&&tx.started+1.0<limit)limit=tx.started+1.0;
   if(watch_active&&!watch_cancelled&&watch_until<limit)limit=watch_until;
   double remaining=limit-now();if(remaining<=0)continue;
   double t0=now();int ready=poll(ready_fds,ready_count,(int)(remaining*1000)+1);
   double dt=now()-t0;ready_wait_s+=dt;if(dt>ready_wait_max_s)ready_wait_max_s=dt;ready_wait_calls++;
   if(ready<0&&errno!=EINTR){rc=4;break;}
   for(nfds_t i=0;i<ready_count;i++)if(ready_fds[i].revents&(POLLERR|POLLHUP|POLLNVAL)){rc=4;break;}
  }
 }
 clock_gettime(CLOCK_THREAD_CPUTIME_ID,&cpu_end);
 double service_wall_s=now()-service_started;
 double service_cpu_s=(cpu_end.tv_sec-cpu_begin.tv_sec)+(cpu_end.tv_nsec-cpu_begin.tv_nsec)*1e-9;
 fprintf(trace,"{\"event\":\"service_wait\",\"time\":%.9f,\"policy\":\"descriptor-readiness-no-sleep\",\"ready_wait_calls\":%u,\"ready_wait_s\":%.9f,\"ready_wait_max_s\":%.9f,\"progress_turns\":%u,\"service_wall_s\":%.9f,\"service_cpu_s\":%.9f}\n",now(),ready_wait_calls,ready_wait_s,ready_wait_max_s,progress_turns,service_wall_s,service_cpu_s);
 if(!rc&&watch_cancelled)rc=watch_cancelled;
 if(watch_active&&!rc)rc=watch_check();
 if(tx.active){packet_abort();if(!rc)rc=9;}
 fprintf(trace,"{\"event\":\"send_service\",\"time\":%.9f,\"pending_polls\":%u,\"maximum_owned_payload_blocks\":1}\n",now(),send_wait_polls);
 fprintf(trace,"{\"event\":\"iio_return_order\",\"time\":%.9f,\"copied_refills\":%u,\"send_with_iio_owned\":%u}\n",now(),copied_refills,send_with_iio_owned);
 fprintf(trace,"{\"event\":\"transport_phases\",\"time\":%.9f,\"depth\":%u,\"queue_high\":%u,\"queue_drop_blocks\":%llu,\"queue_drop_bytes\":%llu,\"received_bytes\":%zu,\"sent_bytes\":%zu,\"refill_s\":%.9f,\"copy_s\":%.9f,\"send_syscall_s\":%.9f,\"iio_hold_max_s\":%.9f,\"loop_gap_max_s\":%.9f}\n",now(),transport_depth,queue.high,(unsigned long long)queue.dropped_blocks,(unsigned long long)queue.dropped_bytes,received_bytes,bytes,refill_s,copy_s,queue_wait_s,iio_hold_max_s,loop_gap_max_s);
 fprintf(trace,"{\"event\":\"copy_send_cpu\",\"time\":%.9f,\"enabled\":%s,\"copy_width\":%u,\"tcp_pump_bytes\":%zu,\"copy_only\":%s,\"no_delivery\":%s,\"copy_cpu_s\":%.9f,\"send_cpu_s\":%.9f}\n",now(),cpu_phases?"true":"false",width,tcp_pump_bytes,copy_only?"true":"false",no_delivery?"true":"false",copy_cpu_s,wire_send_cpu_s-send_cpu_before);
 double payload_complete=now();
 wr(0x290,2);wr(0x284,0);if(buf)iio_buffer_destroy(buf);double buffer_closed=now();context_done(iio_context_destroy,ctx);wr(0x38,old_mode);
 fprintf(trace,"{\"event\":\"iio_cleanup\",\"time\":%.9f,\"buffer_destroy_s\":%.9f,\"context_destroy_s\":%.9f,\"tail_after_done_s\":%.9f,\"block_bytes\":%zu}\n",now(),buffer_closed-payload_complete,now()-buffer_closed,done?payload_complete-done:0,block_bytes);
 if(route_sock>=0)close(route_sock);
 /* Only the host interprets the native sync words and assembles pixels.
    END qualifies the provisional byte stream; no ARM frame ACK is returned. */
 if(!outcome&&!rc)rc=7;
 if(!no_delivery&&video_sock>=0){
  char meta[1400];snprintf(meta,sizeof(meta),"{\"kind\":\"end\",\"stream_tag\":%u,\"epoch\":%llu,\"target_hz\":%llu,\"watch_revision\":%llu,\"watch_generation\":%llu,\"target_position\":%u,\"map_version\":%u,\"serial\":%u,\"first\":%u,\"end\":%u,\"generation_bytes\":%zu,\"pl_outcome\":%d,\"error\":%d,\"qualified_field\":%s,\"a9_send_monotonic_s\":%.9f,\"iio_read_bytes\":%zu,\"tcp_data_bytes\":%zu,\"queue_high_blocks\":%u,\"queue_pending_blocks\":%u,\"queue_rejected_bytes\":%zu,\"refill_s\":%.9f,\"copy_s\":%.9f,\"send_service_s\":%.9f,\"capture_enter_s\":%.9f,\"capture_armed_s\":%.9f,\"capture_busy_s\":%.9f,\"capture_done_s\":%.9f,\"last_data_send_s\":%.9f}",tag,(unsigned long long)target,(unsigned long long)target_hz,(unsigned long long)(watch_active?watch_revision:0),(unsigned long long)(watch_active?watch_selected.generation:0),target_position,map_version,serial,first,end,bytes,outcome,rc,outcome==1&&!rc&&bytes>=end?"true":"false",now(),received_bytes+transport_rejected_bytes,bytes,queue.high,queue.count,transport_rejected_bytes,refill_s,copy_s,queue_wait_s,capture_enter,armed,busy,done,payload_complete);
  if(packet(meta,NULL,0))rc=9;
 }
 fprintf(trace,"{\"event\":\"stream_delivery\",\"time\":%.9f,\"wire_bytes\":%llu,\"messages\":%llu,\"send_s\":%.9f,\"payload_bytes\":%zu,\"error\":%d}\n",now(),(unsigned long long)(wire_bytes-sent_before),(unsigned long long)(wire_chunks-chunks_before),wire_send_s-send_before,bytes,rc);
 FILE *f=NULL;
 snprintf(path,sizeof(path),"%s.json",argv[1]);f=managed_boot?NULL:fopen(path,"wx");if(f){fprintf(f,"{\"outcome\":%d,\"error\":%d,\"serial\":%u,\"target_epoch\":%llu,\"tag\":%u,\"first\":%u,\"end\":%u,\"blocks\":%u,\"owned_bytes\":%zu,\"arm_s\":%.9f,\"busy_observed_s\":%.9f,\"done_s\":%.9f,\"timeout_ms\":%u,\"epoch_after\":%llu}\n",outcome,rc,serial,(unsigned long long)target,tag,first,end,blocks,bytes,armed,busy,done,ms,(unsigned long long)epoch());fclose(f);}
 struct rusage usage;if(!getrusage(RUSAGE_SELF,&usage))fprintf(trace,"{\"event\":\"collector_resources\",\"time\":%.9f,\"user_cpu_s\":%.6f,\"system_cpu_s\":%.6f,\"max_rss_kib\":%ld}\n",now(),usage.ru_utime.tv_sec+usage.ru_utime.tv_usec/1e6,usage.ru_stime.tv_sec+usage.ru_stime.tv_usec/1e6,usage.ru_maxrss);
 fclose(trace);library_done(lib);munmap((void*)r,4096);close(fd);close(lock);return rc?rc:(!outcome?7:0);
}

static int watch_session(int argc,char **argv){
 if(argc!=7){fprintf(stderr,"usage: obs-field1 --watch prefix map.bin seconds timeout-ms first-tag\n");return 2;}
 if(strspn(argv[2],"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/_-.")!=strlen(argv[2])||strlen(argv[2])>400)return 2;
 char *endptr;unsigned seconds=strtoul(argv[4],&endptr,10);if(*endptr||(!seconds&&!managed_boot)||seconds>120)return 2;
 unsigned ms=strtoul(argv[5],&endptr,10);if(*endptr||!ms||ms>1000)return 2;
 unsigned long long tag0=strtoull(argv[6],&endptr,10);if(*endptr||!tag0||tag0>UINT32_MAX-65536)return 2;
 /* All watch attempts use bounded copied transport, so cancel returns IIO
    immediately and can finish any already-started TCP packet from owned RAM. */
 const char *depth=getenv("OBS_TX_BLOCKS");if(!depth||strtoul(depth,NULL,10)<2)return 2;
 if(getenv("OBS_FIELD1_ROUTE_CASE")||getenv("OBS_FIELD1_NEXT_ON_BUSY"))return 2;
 unsetenv("OBS_FIELD1_TARGET_EPOCH");unsetenv("OBS_FIELD1_POSITION");
 if(watch_open(&watching,argv[3])){fprintf(stderr,"invalid watch map\n");return 13;}
 watch_owner=open("/run/obs-field1.lock",O_RDWR|O_CREAT,0600);
 if(watch_owner<0||flock(watch_owner,LOCK_EX|LOCK_NB))return 2;
 reuse_context=1;watch_active=1;watch_until=seconds?now()+seconds:1e30;int rc=0;unsigned attempts=0;
 while(now()<watch_until){
  if(watch_refresh(&watching)){rc=13;break;}
  if(!watching.map.enabled){
   if(managed_boot)break;
   struct pollfd f={watching.fd,POLLIN,0};double remaining=watch_until-now();if(remaining<=0)break;
   if(poll(&f,1,(int)(remaining*1000)+1)<0&&errno!=EINTR){rc=13;break;}
   continue;
  }
  if(attempts>=65536){rc=13;break;}
  char prefix[512],id[32];snprintf(prefix,sizeof(prefix),"%s-%u",argv[2],attempts+1);snprintf(id,sizeof(id),"%llu",tag0+attempts);
  char *args[]={argv[0],prefix,argv[5],id};attempts++;rc=capture(4,args);
  printf("WATCH_ATTEMPT %u %d %llu %u %llu\n",attempts,rc,(unsigned long long)watch_revision,watch_position,(unsigned long long)watch_selected.generation);fflush(stdout);
  if(rc==12){rc=0;break;}
  if(rc&&rc!=10&&rc!=11)break;
  rc=0;
 }
 /* Fail closed even if a preparation failure returned before normal cleanup.
    Keep the session lease until the producer is stopped. Never retake SPI. */
 int g=open("/dev/obs_fl_guard",O_RDONLY);uint32_t state[128]={0};
 if(g>=0&&!ioctl(g,0x4605,state)&&state[1]==2&&!state[2]){
  int mem=open("/dev/mem",O_RDWR|O_SYNC);
  volatile uint32_t *regs=mem<0?MAP_FAILED:mmap(NULL,4096,PROT_READ|PROT_WRITE,MAP_SHARED,mem,0x7c460000);
  if(regs!=MAP_FAILED){
   r=regs;if(rd(0x280)==0x31444c46){
    wr(0x290,2);wr(0x284,0);double deadline=now()+0.1;
    while((rd(0x294)&0xc1)&&now()<deadline){}
    if(rd(0x294)&0xc1)rc=13;
    if(watch_saved){wr(0x30,watch_saved_nco);wr(0x40,watch_saved_code);}
   }else rc=13;
   munmap((void*)regs,4096);
  }else rc=13;
  if(mem>=0)close(mem);
 }
 if(g>=0)close(g);
 if(video_sock>=0)close(video_sock);
 if(session_context){void (*destroy)(void*)=dlsym(session_library,"iio_context_destroy");if(destroy)destroy(session_context);}
 if(session_library)dlclose(session_library);
 close(watch_owner);close(watching.fd);
 printf("WATCH_STOP attempts=%u enabled=%u revision=%llu error=%d\n",attempts,watching.map.enabled,(unsigned long long)watching.map.revision,rc);
 return rc;
}

int main(int argc,char **argv){
 signal(SIGPIPE,SIG_IGN);
 managed_boot=getenv("OBS_FIELD1_BOOT");
 if(managed_boot&&(strlen(managed_boot)!=36||strspn(managed_boot,"abcdef0123456789-")!=36))return 2;
 cpu_phases=getenv("OBS_FIELD1_CPU_PHASES")&&!strcmp(getenv("OBS_FIELD1_CPU_PHASES"),"1");
 const char *pump=getenv("OBS_TCP_PUMP_BYTES");tcp_pump_bytes=pump?strtoul(pump,NULL,10):0;
 if(tcp_pump_bytes!=0&&tcp_pump_bytes!=16384&&tcp_pump_bytes!=32768&&tcp_pump_bytes!=65536&&tcp_pump_bytes!=131072)return 2;
 if(argc>1&&!strcmp(argv[1],"--watch"))return watch_session(argc,argv);
 if(argc==2&&!strcmp(argv[1],"--session")){
  reuse_context=getenv("OBS_FIELD1_REUSE_CONTEXT")&&!strcmp(getenv("OBS_FIELD1_REUSE_CONTEXT"),"1");
  char line[768],prefix[512],extra;unsigned ms,tag;int rc=0;
  unsigned interval_ms=getenv("OBS_FIELD1_INTERVAL_MS")?strtoul(getenv("OBS_FIELD1_INTERVAL_MS"),0,10):0;
  if(interval_ms){fprintf(stderr,"session uses exact epochs; timer-paced interval is retired\n");return 2;}
  while(fgets(line,sizeof(line),stdin)){
   unsigned long long requested=0;int columns=sscanf(line,"%511s %u %u %llu %c",prefix,&ms,&tag,&requested,&extra);
   if(columns!=3&&columns!=4){rc=2;break;}
   if(columns==4){char value[32];snprintf(value,sizeof(value),"%llu",requested);setenv("OBS_FIELD1_TARGET_EPOCH",value,1);}else unsetenv("OBS_FIELD1_TARGET_EPOCH");
   char timeout[32],id[32];snprintf(timeout,sizeof(timeout),"%u",ms);snprintf(id,sizeof(id),"%u",tag);
   char *args[]={argv[0],prefix,timeout,id};rc=capture(4,args);
   printf("ATTEMPT %u %d\n",tag,rc);fflush(stdout);
   /* A broken transport/ownership attempt stops the session, never silently
      reconnects the middle of a field or presents a partial one as complete. */
   if(rc && !(columns==4 && rc==10))break;
   if(rc==10)rc=0;
  }
  if(video_sock>=0)close(video_sock);
  if(session_context){void (*destroy)(void*)=dlsym(session_library,"iio_context_destroy");if(destroy)destroy(session_context);}
  if(session_library)dlclose(session_library);
  return rc;
 }
 int rc=capture(argc,argv);if(video_sock>=0)close(video_sock);return rc;
}
