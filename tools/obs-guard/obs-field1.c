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
 * send returns. No field-sized allocation, raw file, checksum or line parser. */
static int video_sock=-1;
static uint64_t wire_bytes=0,wire_chunks=0;
static double wire_send_s=0;
static int sendall(int s,const void *p,size_t n){const uint8_t *b=p;while(n){ssize_t k=send(s,b,n,MSG_NOSIGNAL);if(k<0&&errno==EINTR)continue;if(k<=0)return -1;b+=k;n-=k;}return 0;}
static int connect_video(void){
 if(video_sock>=0)return 0;
 int s=socket(AF_INET,SOCK_STREAM,0);if(s<0)return -1;
 struct timeval tv={1,0};int one=1;
 setsockopt(s,SOL_SOCKET,SO_SNDTIMEO,&tv,sizeof(tv));setsockopt(s,IPPROTO_TCP,TCP_NODELAY,&one,sizeof(one));
 struct sockaddr_in a={.sin_family=AF_INET,.sin_port=htons(18009)};inet_pton(AF_INET,"192.168.2.10",&a.sin_addr);
 if(connect(s,(void*)&a,sizeof(a))){close(s);return -1;}video_sock=s;return 0;
}
static int packet(const char *meta,const void *raw,size_t n){
 size_t m=strlen(meta);uint32_t header[3]={0x32503852,(uint32_t)m,(uint32_t)n};double t=now();
 int rc=sendall(video_sock,header,sizeof(header))||sendall(video_sock,meta,m)||(n&&sendall(video_sock,raw,n));
 wire_send_s+=now()-t;
 if(rc){close(video_sock);video_sock=-1;return -1;}
 wire_bytes+=sizeof(header)+m+n;wire_chunks++;return 0;
}
#define LOAD(ret,n,args) ret(*n)args=dlsym(lib,#n);if(!n){fprintf(stderr,"missing %s\n",#n);return 3;}
static int capture(int argc,char **argv){
 if(argc!=4&&argc!=5){fprintf(stderr,"usage: obs-field1 output-prefix timeout-ms tag [--unqualified-raw]\n");return 2;}
 if(argc==5&&strcmp(argv[4],"--unqualified-raw"))return 2;
 if(strspn(argv[1],"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/_-.")!=strlen(argv[1]))return 2;

 int no_delivery=getenv("OBS_FIELD1_NO_DELIVERY")&&!strcmp(getenv("OBS_FIELD1_NO_DELIVERY"),"1");
 const char *route_case=getenv("OBS_FIELD1_ROUTE_CASE");
 int next_on_busy=getenv("OBS_FIELD1_NEXT_ON_BUSY")&&!strcmp(getenv("OBS_FIELD1_NEXT_ON_BUSY"),"1");
 int route_sock=-1,route_acked=0;unsigned route_id=0;char route_reply[128];size_t route_have=0;
 if(route_case&&strcmp(route_case,"immediate")&&strcmp(route_case,"delayed")&&strcmp(route_case,"missing_then_correct"))return 2;
 int unqualified_raw=argc==5||(getenv("OBS_FIELD1_UNQUALIFIED_RAW")&&!strcmp(getenv("OBS_FIELD1_UNQUALIFIED_RAW"),"1"));
 FILE *policy=fopen("/tmp/obs007a-field-system/unqualified-raw","r");if(policy){unqualified_raw|=fgetc(policy)=='1';fclose(policy);}
 unsigned ms=strtoul(argv[2],0,0),tag=strtoul(argv[3],0,0);if(!ms||ms>68719||!tag)return 2;
 int lock=open("/run/obs-field1.lock",O_RDWR|O_CREAT,0600);if(lock<0||flock(lock,LOCK_EX|LOCK_NB))return 2;
 int fd=open("/dev/mem",O_RDWR|O_SYNC);r=mmap(0,4096,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0x7c460000);
 if(r==MAP_FAILED||rd(0x280)!=0x31444c46)return 3;
 /* PHY sysfs callbacks are intentionally denied while PL owns SPI. The
    guard's take receipt contains actual TX readback, and TX routes are off. */
 int guard=open("/dev/obs_fl_guard",O_RDONLY);uint32_t gs[128]={0};
 int tx_authorized=getenv("OBS_FIELD1_TX_AUTHORIZED")&&!strcmp(getenv("OBS_FIELD1_TX_AUTHORIZED"),"1");
 if(guard<0||ioctl(guard,0x4605,gs)||gs[0]!=2||gs[1]!=2||gs[2]||gs[112]!=(tx_authorized?0u:1u)){fprintf(stderr,"guard TX receipt differs from explicit series authorization\n");return 3;}
 close(guard);
 void *lib=dlopen("libiio.so.0",RTLD_NOW);if(!lib){fprintf(stderr,"dlopen: %s\n",dlerror());return 3;}
 LOAD(void*,iio_create_local_context,(void));LOAD(void*,iio_context_find_device,(void*,const char*));
 LOAD(void*,iio_device_find_channel,(void*,const char*,bool));LOAD(void,iio_channel_enable,(void*));
 LOAD(int,iio_context_set_timeout,(void*,unsigned));LOAD(int,iio_device_set_kernel_buffers_count,(void*,unsigned));
 LOAD(void*,iio_device_create_buffer,(void*,size_t,bool));LOAD(ssize_t,iio_buffer_refill,(void*));
 LOAD(void*,iio_buffer_start,(void*));LOAD(void*,iio_buffer_end,(void*));
 LOAD(int,iio_buffer_set_blocking_mode,(void*,bool));LOAD(void,iio_buffer_destroy,(void*));LOAD(void,iio_context_destroy,(void*));
 uint32_t old_mode=rd(0x38);wr(0x290,2);wr(0x284,0);wr(0x38,1);usleep(1000);
 void *ctx=iio_create_local_context();if(!ctx){perror("iio context");return 3;}
 void *dev=iio_context_find_device(ctx,"cf-ad9361-lpc");if(!dev){fprintf(stderr,"ADC device missing\n");return 3;}
 iio_context_set_timeout(ctx,250);iio_device_set_kernel_buffers_count(dev,4);
 void *ch0=iio_device_find_channel(dev,"voltage0",false),*ch1=iio_device_find_channel(dev,"voltage1",false);if(!ch0||!ch1)return 3;
 iio_channel_enable(ch0);iio_channel_enable(ch1);
 void *buf=iio_device_create_buffer(dev,IIO_BLOCK_SAMPLES,false);if(!buf){perror("iio buffer");return 3;}
 size_t block_bytes=(uint8_t*)iio_buffer_end(buf)-(uint8_t*)iio_buffer_start(buf);
 size_t capture_limit=REAL8_FS*CAPTURE_SECONDS+block_bytes;
 if(iio_buffer_set_blocking_mode(buf,false))return 3;
 ssize_t initial=iio_buffer_refill(buf);
 if(initial!=-EAGAIN){fprintf(stderr,"new generation not empty: %zd\n",initial);iio_buffer_destroy(buf);iio_context_destroy(ctx);return 3;}
 /* stream_run=0 suppresses FM output while a fresh empty DMA generation is created.
    No previous generation can contribute bytes to this context. */
 uint64_t sent_before=wire_bytes,chunks_before=wire_chunks;double send_before=wire_send_s;
 if(!no_delivery&&connect_video()){iio_buffer_destroy(buf);iio_context_destroy(ctx);wr(0x38,old_mode);close(fd);close(lock);return 9;}
 char path[512];snprintf(path,sizeof(path),"%s.trace.jsonl",argv[1]);FILE *trace=fopen(path,"wx");if(!trace)return 3;
 fprintf(trace,"{\"event\":\"raw_policy_latched\",\"time\":%.9f,\"unqualified_raw\":%s}\n",now(),unqualified_raw?"true":"false");
 wr(0x288,tag);wr(0x28c,ms*62500u);if(rd(0x288)!=tag||rd(0x28c)!=ms*62500u)return 3;
 wr(0x284,1);double start=now(),armed=0,busy=0,done=0;uint64_t target=0;size_t bytes=0;unsigned blocks=0,status=0,serial=0,first=0,end=0,target_position=0,map_version=0;int outcome=0,rc=0;
 /* A target two visits ahead avoids the multi-register write race. */
 uint64_t current_epoch;unsigned current_position;do{current_epoch=epoch();current_position=rd(0x234);}while(current_epoch!=epoch());
 unsigned advance=2;const char *want_env=getenv("OBS_FIELD1_POSITION"),*count_env=getenv("OBS_FIELD1_POSITIONS");
 if(want_env&&count_env){unsigned wanted=strtoul(want_env,0,0),count=strtoul(count_env,0,0);if(!count||count>128||wanted>=count){wr(0x284,0);iio_buffer_destroy(buf);iio_context_destroy(ctx);return 3;}while((current_position+advance)%count!=wanted)advance++;}
 target=current_epoch+advance;
 if(route_case){
  uint64_t marker_epoch=((uint64_t)rd(0x250)<<32)|rd(0x24c);
  route_id=rd(0x240);if(!(rd(0x228)&8)||marker_epoch!=current_epoch){fprintf(stderr,"current-visit field needs matching hardware antenna marker\n");return 3;}
  target=current_epoch;
  route_sock=socket(AF_INET,SOCK_STREAM,0);struct timeval tv={1,0};setsockopt(route_sock,SOL_SOCKET,SO_SNDTIMEO,&tv,sizeof(tv));
  struct sockaddr_in a={.sin_family=AF_INET,.sin_port=htons(18008)};inet_pton(AF_INET,"192.168.2.10",&a.sin_addr);
  if(connect(route_sock,(void*)&a,sizeof(a)))return 3;
 }
 if(!no_delivery){
  char meta[768];snprintf(meta,sizeof(meta),"{\"kind\":\"begin\",\"delivery_id\":\"field-%u-%llu\",\"stream_tag\":%u,\"epoch\":%llu,\"fs\":%u,\"code\":%u,\"g\":0,\"block_bytes\":%zu,\"capture_seconds\":%u,\"capture_limit_bytes\":%zu,\"unqualified_raw\":%s,\"replay\":false}",tag,(unsigned long long)target,tag,(unsigned long long)target,REAL8_FS,rd(0x40)&63,block_bytes,CAPTURE_SECONDS,capture_limit,unqualified_raw?"true":"false");
  if(packet(meta,NULL,0)){wr(0x284,0);iio_buffer_destroy(buf);iio_context_destroy(ctx);wr(0x38,old_mode);fclose(trace);close(fd);close(lock);return 9;}
 }
 wr(0x170,(uint32_t)target);wr(0x174,target>>32);wr(0x290,1);armed=now();
 fprintf(trace,"{\"event\":\"arm\",\"time\":%.9f,\"target_epoch\":%llu,\"tag\":%u,\"timeout_ticks\":%u}\n",armed,(unsigned long long)target,tag,rd(0x28c));
 while(now()-start<CAPTURE_SECONDS){
  ssize_t n=iio_buffer_refill(buf);double t=now();
  if(n>0){
   uint8_t *p=iio_buffer_start(buf);size_t span=(uint8_t*)iio_buffer_end(buf)-p;
   if((size_t)n>span||bytes+(size_t)n>capture_limit){rc=4;break;}
   if(!no_delivery){char meta[160];snprintf(meta,sizeof(meta),"{\"kind\":\"data\",\"stream_tag\":%u,\"offset\":%zu}",tag,bytes);if(packet(meta,p,(size_t)n)){rc=9;break;}}
   bytes+=(size_t)n;blocks++;
  }else if(n!=-EAGAIN){fprintf(trace,"{\"event\":\"iio_error\",\"code\":%zd}\n",n);rc=4;break;}
  status=rd(0x294);if((status&1)&&!busy){busy=t;target_position=rd(0x234);map_version=rd(0x22c);
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
   if(e!=target||rt!=tag||rd(0x294)&16){rc=5;break;}
   if(outcome!=1){if(!route_case||route_acked)break;else continue;}
   if(end<first||end>capture_limit){rc=5;break;}
   if(bytes>=end&&(!route_case||route_acked))break;
  }
  usleep(100);
 }
 wr(0x290,2);wr(0x284,0);iio_buffer_destroy(buf);iio_context_destroy(ctx);wr(0x38,old_mode);
 if(route_sock>=0)close(route_sock);
 /* Only the host interprets the native sync words and assembles pixels.
    END qualifies the provisional byte stream; no ARM frame ACK is returned. */
 if(!outcome&&!rc)rc=7;
 if(!no_delivery&&video_sock>=0){
  char meta[1024];snprintf(meta,sizeof(meta),"{\"kind\":\"end\",\"stream_tag\":%u,\"epoch\":%llu,\"target_hz\":0,\"target_position\":%u,\"map_version\":%u,\"serial\":%u,\"first\":%u,\"end\":%u,\"generation_bytes\":%zu,\"pl_outcome\":%d,\"error\":%d,\"qualified_field\":%s,\"a9_send_monotonic_s\":%.9f}",tag,(unsigned long long)target,target_position,map_version,serial,first,end,bytes,outcome,rc,outcome==1&&!rc&&bytes>=end?"true":"false",now());
  if(packet(meta,NULL,0))rc=9;
 }
 fprintf(trace,"{\"event\":\"stream_delivery\",\"time\":%.9f,\"wire_bytes\":%llu,\"messages\":%llu,\"send_s\":%.9f,\"payload_bytes\":%zu,\"error\":%d}\n",now(),(unsigned long long)(wire_bytes-sent_before),(unsigned long long)(wire_chunks-chunks_before),wire_send_s-send_before,bytes,rc);
 FILE *f=NULL;
 snprintf(path,sizeof(path),"%s.json",argv[1]);f=fopen(path,"wx");if(f){fprintf(f,"{\"outcome\":%d,\"error\":%d,\"serial\":%u,\"target_epoch\":%llu,\"tag\":%u,\"first\":%u,\"end\":%u,\"blocks\":%u,\"owned_bytes\":%zu,\"arm_s\":%.9f,\"busy_observed_s\":%.9f,\"done_s\":%.9f,\"timeout_ms\":%u,\"epoch_after\":%llu}\n",outcome,rc,serial,(unsigned long long)target,tag,first,end,blocks,bytes,armed,busy,done,ms,(unsigned long long)epoch());fclose(f);}
 struct rusage usage;if(!getrusage(RUSAGE_SELF,&usage))fprintf(trace,"{\"event\":\"collector_resources\",\"time\":%.9f,\"user_cpu_s\":%.6f,\"system_cpu_s\":%.6f,\"max_rss_kib\":%ld}\n",now(),usage.ru_utime.tv_sec+usage.ru_utime.tv_usec/1e6,usage.ru_stime.tv_sec+usage.ru_stime.tv_usec/1e6,usage.ru_maxrss);
 fclose(trace);dlclose(lib);munmap((void*)r,4096);close(fd);close(lock);return rc?rc:(!outcome?7:0);
}

int main(int argc,char **argv){
 signal(SIGPIPE,SIG_IGN);
 if(argc==2&&!strcmp(argv[1],"--session")){
  char line[768],prefix[512],extra;unsigned ms,tag;int rc=0;
  double due=now();unsigned interval_ms=getenv("OBS_FIELD1_INTERVAL_MS")?strtoul(getenv("OBS_FIELD1_INTERVAL_MS"),0,10):0;
  while(fgets(line,sizeof(line),stdin)){
   if(sscanf(line,"%511s %u %u %c",prefix,&ms,&tag,&extra)!=3){rc=2;break;}
   char timeout[32],id[32];snprintf(timeout,sizeof(timeout),"%u",ms);snprintf(id,sizeof(id),"%u",tag);
   double wait=due-now();if(wait>0)usleep((useconds_t)(wait*1e6));due+=interval_ms/1000.0;
   char *args[]={argv[0],prefix,timeout,id};rc=capture(4,args);
   printf("ATTEMPT %u %d\n",tag,rc);fflush(stdout);
   /* A broken transport/ownership attempt stops the session, never silently
      reconnects the middle of a field or presents a partial one as complete. */
   if(rc)break;
  }
  if(video_sock>=0)close(video_sock);
  return rc;
 }
 int rc=capture(argc,argv);if(video_sock>=0)close(video_sock);return rc;
}
