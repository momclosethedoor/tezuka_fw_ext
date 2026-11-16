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
#include <arpa/inet.h>
static volatile uint32_t *r;
static uint32_t rd(unsigned o){return r[o/4];}
static void wr(unsigned o,uint32_t x){r[o/4]=x;__sync_synchronize();}
static uint64_t epoch(void){uint32_t h,l,b;do{h=rd(0x184);l=rd(0x180);b=rd(0x184);}while(h!=b);return ((uint64_t)h<<32)|l;}
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec*1e-9;}
static unsigned segments(const uint8_t *p,size_t n,size_t *offsets){unsigned count=0;for(size_t i=0;i+4<=n;i+=4)if(p[i]==128&&p[i+1]==128&&p[i+2]==128&&p[i+3]<64){if(count<8)offsets[count]=i;count++;}return count;}
static int sendall(int s,const void *p,size_t n){const uint8_t *b=p;while(n){ssize_t k=send(s,b,n,0);if(k<=0)return -1;b+=k;n-=k;}return 0;}
#define LOAD(ret,n,args) ret(*n)args=dlsym(lib,#n);if(!n){fprintf(stderr,"missing %s\n",#n);return 3;}
int main(int argc,char **argv){
 if(argc!=4&&argc!=5){fprintf(stderr,"usage: obs-field1 output-prefix timeout-ms tag [--unqualified-raw]\n");return 2;}
 if(argc==5&&strcmp(argv[4],"--unqualified-raw"))return 2;
 if(strspn(argv[1],"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/_-.")!=strlen(argv[1]))return 2;
 int diagnostic=getenv("OBS_FIELD1_DIAGNOSTIC_RAW")&&!strcmp(getenv("OBS_FIELD1_DIAGNOSTIC_RAW"),"1");
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
 void *buf=iio_device_create_buffer(dev,65536,false);if(!buf){perror("iio buffer");return 3;}
 if(iio_buffer_set_blocking_mode(buf,false))return 3;
 ssize_t initial=iio_buffer_refill(buf);
 if(initial!=-EAGAIN){fprintf(stderr,"new generation not empty: %zd\n",initial);iio_buffer_destroy(buf);iio_context_destroy(ctx);return 3;}
 /* stream_run=0 suppresses FM output while a fresh empty DMA generation is created.
    No previous generation can contribute bytes to this context. */
 uint8_t *data=malloc(8*1024*1024);if(!data)return 3;
 char path[512];snprintf(path,sizeof(path),"%s.trace.jsonl",argv[1]);FILE *trace=fopen(path,"wx");if(!trace)return 3;
 fprintf(trace,"{\"event\":\"raw_policy_latched\",\"time\":%.9f,\"unqualified_raw\":%s,\"diagnostic_raw\":%s}\n",now(),unqualified_raw?"true":"false",diagnostic?"true":"false");
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
 wr(0x170,(uint32_t)target);wr(0x174,target>>32);wr(0x290,1);armed=now();
 fprintf(trace,"{\"event\":\"arm\",\"time\":%.9f,\"target_epoch\":%llu,\"tag\":%u,\"timeout_ticks\":%u}\n",armed,(unsigned long long)target,tag,rd(0x28c));
 while(now()-start<2){
  ssize_t n=iio_buffer_refill(buf);double t=now();
  if(n>0){
   uint8_t *p=iio_buffer_start(buf);size_t span=(uint8_t*)iio_buffer_end(buf)-p;
   if((size_t)n>span||bytes+n>8*1024*1024){rc=4;break;}
   memcpy(data+bytes,p,n);fprintf(trace,"{\"event\":\"owned_block\",\"time\":%.9f,\"first\":%zu,\"end\":%zu,\"tag\":%u}\n",t,bytes,bytes+n,tag);bytes+=n;blocks++;
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
   if(outcome!=1){size_t offsets[8];if(segments(data,bytes,offsets)>=3&&(!route_case||route_acked))break;else continue;}
   if(end<first||end>8*1024*1024){rc=5;break;}
   if(bytes>=end&&(!route_case||route_acked))break;
  }
  usleep(100);
 }
 wr(0x290,2);wr(0x284,0);iio_buffer_destroy(buf);iio_context_destroy(ctx);wr(0x38,old_mode);
 if(route_sock>=0)close(route_sock);
 size_t offsets[8]={0},attempt_first=0,attempt_end=bytes;unsigned nsegments=segments(data,bytes,offsets);int attempt_epoch_proven=0;
 if(!rc&&nsegments>=2&&nsegments<=3){
  attempt_first=offsets[1];
  if(outcome==1&&first>=attempt_first&&end<=bytes){attempt_end=end;attempt_epoch_proven=1;}
  else if(outcome==2&&nsegments==3){attempt_end=offsets[2];attempt_epoch_proven=1;}
 }
 /* A timeout still delivers native FM bytes. Qualification and assembly are
    separate outcomes; an unproven range remains an explicitly marked capture. */
 snprintf(path,sizeof(path),"%s.attempt.real8",argv[1]);FILE *attempt=NULL;if(outcome==1||unqualified_raw||diagnostic)attempt=fopen(path,"wx");if(attempt){fwrite(data+attempt_first,1,attempt_end-attempt_first,attempt);fclose(attempt);}
 snprintf(path,sizeof(path),"%s.attempt.json",argv[1]);attempt=fopen(path,"wx");if(attempt){fprintf(attempt,"{\"qualified_field\":%s,\"epoch_proven\":%s,\"outcome\":%d,\"epoch\":%llu,\"tag\":%u,\"first\":%zu,\"end\":%zu,\"segments\":%u,\"fm_triggered\":true,\"range_rule\":\"new-generation marker; FIELD accept reset; post-completion recall reset\"}\n",outcome==1&&!rc?"true":"false",attempt_epoch_proven?"true":"false",outcome,(unsigned long long)target,tag,attempt_first,attempt_end,nsegments);fclose(attempt);}
 if(outcome==1&&!rc&&bytes>=end){snprintf(path,sizeof(path),"%s.field.real8",argv[1]);FILE *f=fopen(path,"wx");if(!f)rc=6;else{if(fwrite(data+first,1,end-first,f)!=end-first)rc=6;fclose(f);}}
 /* Ordinary delivery keeps its qualified-field contract. Unqualified raw
    bytes are an explicit diagnostic option; control status reports TIMEOUT. */
 if(no_delivery){fprintf(trace,"{\"event\":\"delivery_disabled\",\"time\":%.9f,\"scope\":\"explicit A9-only bench control\"}\n",now());}
 if(!no_delivery&&((outcome==1&&!rc)||unqualified_raw||diagnostic)&&attempt_end>attempt_first&&attempt_end-attempt_first<=2097152){
  if(getenv("OBS_FIELD1_DELAY_DELIVERY_MS")){unsigned delay=strtoul(getenv("OBS_FIELD1_DELAY_DELIVERY_MS"),0,10);if(delay>2000)return 2;fprintf(trace,"{\"event\":\"bench_delivery_delay\",\"time\":%.9f,\"ms\":%u}\n",now(),delay);fflush(trace);usleep(delay*1000);}
  char command[640],digest[65]={0};snprintf(command,sizeof(command),"sha256sum %s.attempt.real8",argv[1]);FILE *hash=popen(command,"r");int hashed=hash&&fscanf(hash,"%64s",digest)==1;if(hash)pclose(hash);
  if(hashed){
   char meta[2048];uint32_t payload=(uint32_t)(attempt_end-attempt_first);
   int m=snprintf(meta,sizeof(meta),"{\"delivery_id\":\"field-%u-%u\",\"replay\":false,\"epoch\":%llu,\"epoch_proven\":%s,\"target_hz\":0,\"target_position\":%u,\"map_version\":%u,\"stream_tag\":%u,\"sha256\":\"%s\",\"payload_bytes\":%u,\"qualified_field\":%s,\"pl_outcome\":%d,\"fm_triggered\":true,\"code\":%u,\"g\":0,\"fs\":15360000,\"parity\":null,\"active_rows\":%u,\"finite_field\":%s,\"field_first_byte\":%u,\"a9_send_monotonic_s\":%.9f}",tag,serial,(unsigned long long)target,attempt_epoch_proven?"true":"false",target_position,map_version,tag,digest,payload,outcome==1&&!rc?"true":"false",outcome,rd(0x40)&63,(rd(0x40)&2)?240:288,outcome==1&&!rc?"true":"false",first>=attempt_first?(unsigned)(first-attempt_first):0,now());
   int s=socket(AF_INET,SOCK_STREAM,0);struct timeval tv={1,0};setsockopt(s,SOL_SOCKET,SO_SNDTIMEO,&tv,sizeof(tv));struct sockaddr_in a={.sin_family=AF_INET,.sin_port=htons(18009)};inet_pton(AF_INET,"192.168.2.10",&a.sin_addr);
   uint32_t header[3]={0x31503852,(uint32_t)m,payload};int sent=connect(s,(void*)&a,sizeof(a))==0&&sendall(s,header,12)==0&&sendall(s,meta,m)==0&&sendall(s,data+attempt_first,payload)==0;
   fprintf(trace,"{\"event\":\"capture_delivery\",\"time\":%.9f,\"bytes\":%u,\"sent\":%s,\"qualified_field\":%s}\n",now(),payload,sent?"true":"false",outcome==1&&!rc?"true":"false");close(s);
  }
 }
 snprintf(path,sizeof(path),"%s.generation.real8",argv[1]);FILE *f=NULL;if(diagnostic)f=fopen(path,"wx");if(f){fwrite(data,1,bytes,f);fclose(f);}
 snprintf(path,sizeof(path),"%s.json",argv[1]);f=fopen(path,"wx");if(f){fprintf(f,"{\"outcome\":%d,\"error\":%d,\"serial\":%u,\"target_epoch\":%llu,\"tag\":%u,\"first\":%u,\"end\":%u,\"blocks\":%u,\"owned_bytes\":%zu,\"arm_s\":%.9f,\"busy_observed_s\":%.9f,\"done_s\":%.9f,\"timeout_ms\":%u,\"epoch_after\":%llu}\n",outcome,rc,serial,(unsigned long long)target,tag,first,end,blocks,bytes,armed,busy,done,ms,(unsigned long long)epoch());fclose(f);}
 struct rusage usage;if(!getrusage(RUSAGE_SELF,&usage))fprintf(trace,"{\"event\":\"collector_resources\",\"time\":%.9f,\"user_cpu_s\":%.6f,\"system_cpu_s\":%.6f,\"max_rss_kib\":%ld}\n",now(),usage.ru_utime.tv_sec+usage.ru_utime.tv_usec/1e6,usage.ru_stime.tv_sec+usage.ru_stime.tv_usec/1e6,usage.ru_maxrss);
 fclose(trace);free(data);munmap((void*)r,4096);close(fd);close(lock);return rc?rc:(!outcome?7:0);
}
