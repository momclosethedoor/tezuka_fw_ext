#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <time.h>
#include <signal.h>
#include <dlfcn.h>
#include <stdbool.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/file.h>
#include <sys/socket.h>
#include <arpa/inet.h>
#include "field-native.h"
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
static volatile int interrupted;
static void stop(int x){(void)x;interrupted=1;}
static volatile uint32_t*r;
static uint32_t rd(unsigned o){return r[o/4];}
static void wr(unsigned o,uint32_t x){r[o/4]=x;__sync_synchronize();}
static uint64_t rd64(unsigned o){uint32_t hi,lo,b;do{hi=rd(o+4);lo=rd(o);b=rd(o+4);}while(b!=hi);return((uint64_t)hi<<32)|lo;}
static void event(FILE*f,const char*name,uint64_t epoch,uint32_t id){fprintf(f,"{\"event\":\"%s\",\"a9_monotonic_s\":%.9f,\"epoch\":%llu,\"marker_id\":%u,\"version\":%u,\"position\":%u,\"fld_status\":%u,\"rx_status\":%u}\n",name,now(),(unsigned long long)epoch,id,rd(0x22c),rd(0x234),rd(0x228),rd(0x188));fflush(f);}
static int save_field(const char*dir,Field*f){char p[512];snprintf(p,sizeof(p),"%s/field.pgm",dir);FILE*out=fopen(p,"wb");if(!out)return -1;int rows=f->ntsc?240:288;fprintf(out,"P5\n720 %d\n255\n",rows);if(fwrite(f->pixels,720,rows,out)!=(size_t)rows){fclose(out);return -1;}fflush(out);fsync(fileno(out));fclose(out);return 0;}
#define LOAD(ret,name,args) ret(*name)args=dlsym(lib,#name);if(!name){fprintf(stderr,"missing %s\n",#name);return 3;}
static int offline(const char*input,const char*dir){mkdir(dir,0700);FILE*in=fopen(input,"rb");if(!in)return 2;Field f;if(!field_init(&f))return 2;int8_t raw[16384];size_t n,total=0;double start=now();while((n=fread(raw,1,sizeof(raw),in))&&total+n<=FCAP){total+=n;if(field_feed(&f,raw,n)>0)break;}if(f.ready)save_field(dir,&f);printf("{\"outcome\":\"%s\",\"rows\":%d,\"standard\":\"%s\",\"start_sample\":%.6f,\"end_sample\":%.6f,\"samples_fed\":%zu,\"seconds\":%.9f}\n",f.ready?"FIELD_READY":"NO_FIELD",f.rows,f.ntsc?"ntsc":"pal",f.begin,f.end,total,now()-start);field_free(&f);fclose(in);return 0;}
int obs_preview_run(int argc,char**argv){
 interrupted=0;
 if(argc==4&&!strcmp(argv[1],"--offline"))return offline(argv[2],argv[3]);
 if(argc<4){fprintf(stderr,"usage: output-directory version position [normal|stale|timeout]\n");return 2;}
 const char*dir=argv[1],*test=argc>4?argv[4]:"normal";uint32_t version=strtoul(argv[2],0,0),position=strtoul(argv[3],0,0);
 if(mkdir(dir,0700)){perror("unique output directory");return 2;}
 char path[512];snprintf(path,sizeof(path),"%s/trace.jsonl",dir);FILE*trace=fopen(path,"w");if(!trace)return 2;
 int lock=open("/run/obs-preview.lock",O_CREAT|O_RDWR,0600);if(lock<0||flock(lock,LOCK_EX|LOCK_NB))return 2;
 int fd=open("/dev/mem",O_RDWR|O_SYNC);r=mmap(0,4096,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0x7c460000);if(r==MAP_FAILED||rd(0x224)!=0x31444c46||rd(0x1e0)!=3)return 3;
 void*lib=dlopen("libiio.so.0",RTLD_NOW);if(!lib){fprintf(stderr,"%s\n",dlerror());return 3;}
 LOAD(void*,iio_create_local_context,(void));LOAD(void*,iio_context_find_device,(void*,const char*));LOAD(void*,iio_device_find_channel,(void*,const char*,bool));LOAD(void,iio_channel_enable,(void*));LOAD(int,iio_context_set_timeout,(void*,unsigned));LOAD(int,iio_device_set_kernel_buffers_count,(void*,unsigned));LOAD(void*,iio_device_create_buffer,(void*,size_t,bool));LOAD(ssize_t,iio_buffer_refill,(void*));LOAD(void*,iio_buffer_start,(void*));LOAD(void*,iio_buffer_end,(void*));LOAD(void,iio_buffer_destroy,(void*));LOAD(void,iio_context_destroy,(void*));
 /* Cancellation belongs to the enclosing ARM agent. */
 void*ctx=NULL,*buf=NULL;Field field;if(!field_init(&field))return 3;
 double t0=now(),held=0,segment_time=0,ready_time=0,ack_time=0;uint64_t epoch=0;uint32_t id=0,old_mode=rd(0x38),old_nco=rd(0x30),old_cfg=rd(0x40);int owned=0,segment=0,stale_ignored=0,blocks=0;size_t bytes=0,discard=0;const char*outcome="TARGET_NOT_OWNED";FILE*rawout=NULL;
 while(now()-t0<5&&!interrupted){uint32_t m=rd(0x240);uint64_t e=rd64(0x24c);if(m==rd(0x240)&&(rd(0x228)&8)&&rd(0x22c)==version&&rd(0x234)==position){id=m;epoch=e;held=now();owned=1;break;}usleep(50);}
 if(!owned)goto finish;event(trace,"A9_MARKER_DETECTED",epoch,id);wr(0x178,rd(0x178)|1);event(trace,"VIDEO_HOLD_SET",epoch,id);
 /* Test stale marker first while route remains unchanged; never claims GPIO. */
 wr(0x218,id-1);wr(0x220,2);usleep(1000);if(!(rd(0x228)&8)||rd(0x240)!=id){outcome="STALE_MARKER_ACCEPTED";goto finish;}event(trace,"STALE_MARKER_IGNORED",epoch,id);
 /* Actual TCP round trip to host; ACK means already-observed fixed route ready. */
 {int sock=socket(AF_INET,SOCK_STREAM,0);struct timeval tv={1,0};setsockopt(sock,SOL_SOCKET,SO_RCVTIMEO,&tv,sizeof(tv));setsockopt(sock,SOL_SOCKET,SO_SNDTIMEO,&tv,sizeof(tv));struct sockaddr_in a={.sin_family=AF_INET,.sin_port=htons(18008)};inet_pton(AF_INET,"192.168.2.10",&a.sin_addr);
  if(connect(sock,(void*)&a,sizeof(a))){close(sock);outcome="HOST_ROUTE_UNAVAILABLE";goto finish;}char msg[512];int n=snprintf(msg,sizeof(msg),"{\"marker_id\":%u,\"epoch\":%llu,\"version\":%u,\"position\":%u,\"route\":1,\"a9_send_s\":%.9f,\"test\":\"%s\"}\n",id,(unsigned long long)epoch,version,position,now(),test);event(trace,"TCP_SEND",epoch,id);if(send(sock,msg,n,0)!=n){close(sock);outcome="HOST_ROUTE_SEND";goto finish;}char answer[512];n=recv(sock,answer,sizeof(answer)-1,0);close(sock);if(n<=0){outcome="HOST_ROUTE_TIMEOUT";goto finish;}answer[n]=0;unsigned ack=0;if(sscanf(answer,"ACK %u",&ack)!=1||ack!=id){outcome="HOST_ROUTE_BAD_ACK";goto finish;}fprintf(trace,"{\"event\":\"HOST_REPLY\",\"a9_receive_s\":%.9f,\"reply\":\"%s\"}\n",now(),answer);event(trace,"TCP_ACK_RECEIVED",epoch,id);}
 if(rd64(0x24c)!=epoch||rd(0x240)!=id||rd(0x234)!=position){outcome="TARGET_CHANGED";goto finish;}
 wr(0x218,id);wr(0x220,2);event(trace,"MARKER_ACK_APPLIED",epoch,id);
 wr(0x38,0);wr(0x30,(uint32_t)llround(4200000./61440000*268435456.));wr(0x40,24);
 ctx=iio_create_local_context();if(!ctx){outcome="IIO_CONTEXT";goto finish;}iio_context_set_timeout(ctx,150);
 void*dev=iio_context_find_device(ctx,"cf-ad9361-lpc");if(!dev){outcome="IIO_DEVICE";goto finish;}void*c0=iio_device_find_channel(dev,"voltage0",false),*c1=iio_device_find_channel(dev,"voltage1",false);if(!c0||!c1){outcome="IIO_CHANNEL";goto finish;}iio_channel_enable(c0);iio_channel_enable(c1);
 if(iio_device_set_kernel_buffers_count(dev,2)){outcome="IIO_BUFFER_COUNT";goto finish;}
 buf=iio_device_create_buffer(dev,65536,false);if(!buf){outcome="IIO_BUFFER_CREATE";goto finish;}
 event(trace,"IIO_GENERATION_CREATED",epoch,id);wr(0x38,1);event(trace,"NEW_REAL8_SEGMENT_REQUESTED",epoch,id);
 snprintf(path,sizeof(path),"%s/owned.real8",dir);rawout=fopen(path,"wb");if(!rawout){outcome="FILE_ERROR";goto finish;}
 double acquisition=now();outcome="NO_FIELD_TIMEOUT";
 while(now()-acquisition<.125&&!interrupted){ssize_t n=iio_buffer_refill(buf);double completed=now();if(n<=0){outcome="TRANSPORT";break;}int8_t*p=iio_buffer_start(buf);if((char*)iio_buffer_end(buf)-(char*)p<n){outcome="BUFFER_BOUNDS";break;}blocks++;bytes+=n;event(trace,"IIO_BLOCK_OWNED",epoch,id);
  if(rd64(0x180)!=epoch||rd(0x234)!=position||rd(0x22c)!=version){outcome="EPOCH_CHANGED";break;}
  if(fwrite(p,1,n,rawout)!=(size_t)n){outcome="FILE_ERROR";break;}
  size_t begin=0;if(!segment){for(begin=0;begin+4<=(size_t)n;begin+=4)if((uint8_t)p[begin]==128&&(uint8_t)p[begin+1]==128&&(uint8_t)p[begin+2]==128&&(uint8_t)p[begin+3]==24){segment=1;segment_time=completed;begin+=4;event(trace,"FRESH_SEGMENT_OBSERVED",epoch,id);break;}discard+=begin;if(!segment)continue;}
  if(!strcmp(test,"timeout"))continue;
  int result=field_feed(&field,p+begin,n-begin);if(result<0){outcome="DECODER_CAPACITY";break;}if(result>0){ready_time=now();if(save_field(dir,&field)){outcome="FIELD_SAVE_ERROR";break;}outcome="FIELD_READY_NATIVE_PORT";event(trace,"FIELD_OWNED_AND_SAVED",epoch,id);break;}
 }
 if(!segment)outcome="EPOCH_UNPROVEN";
finish:
 if(rawout){fflush(rawout);fclose(rawout);}if(buf)iio_buffer_destroy(buf);if(ctx)iio_context_destroy(ctx);
 if(owned&&rd64(0x180)==epoch){
  if(!strcmp(test,"stale")){wr(0x170,(uint32_t)(epoch-1));wr(0x174,(uint32_t)((epoch-1)>>32));wr(0x17c,1);usleep(2000);stale_ignored=rd64(0x180)==epoch;event(trace,"STALE_VIDEO_ACK_TEST",epoch,id);}
  /* Persist outcome before completion pulse; failure is still attempt-done. */
  snprintf(path,sizeof(path),"%s/outcome-before-ack.json",dir);FILE*out=fopen(path,"w");if(out){fprintf(out,"{\"outcome\":\"%s\",\"epoch\":%llu,\"position\":%u,\"rows\":%d}\n",outcome,(unsigned long long)epoch,position,field.rows);fflush(out);fsync(fileno(out));fclose(out);}
  wr(0x178,rd(0x178)&~1u);wr(0x170,(uint32_t)epoch);wr(0x174,epoch>>32);ack_time=now();wr(0x17c,1);event(trace,"A9_ATTEMPT_DONE_ACK",epoch,id);
  if(rd(0x228)&8){wr(0x218,id);wr(0x220,2);}double t=now();while(rd64(0x180)==epoch&&now()-t<.02)usleep(20);event(trace,"NEXT_EPOCH_OBSERVED",rd64(0x180),id);
 }
 wr(0x38,0);wr(0x30,old_nco);wr(0x40,old_cfg);wr(0x38,old_mode);
 snprintf(path,sizeof(path),"%s/result.json",dir);FILE*out=fopen(path,"w");if(out){fprintf(out,"{\"outcome\":\"%s\",\"epoch\":%llu,\"marker_id\":%u,\"version\":%u,\"position\":%u,\"held_detected_s\":%.9f,\"segment_owned_s\":%.9f,\"field_ready_s\":%.9f,\"ack_s\":%.9f,\"observed_hold_s\":%.9f,\"blocks\":%d,\"bytes\":%zu,\"discard_before_segment\":%zu,\"segment_proven\":%s,\"stale_video_ignored\":%s,\"rows\":%d,\"standard\":\"%s\",\"field_start_sample\":%.6f,\"field_end_sample\":%.6f,\"decoder\":\"native experimental port; reference equivalence separately checked\",\"source_gap_counter\":false}\n",outcome,(unsigned long long)epoch,id,version,position,held,segment_time,ready_time,ack_time,ack_time?ack_time-held:0,blocks,bytes,discard,segment?"true":"false",stale_ignored?"true":"false",field.rows,field.ntsc?"ntsc":"pal",field.begin,field.end);fclose(out);}
 printf("%s epoch=%llu blocks=%d rows=%d hold=%.6f\n",outcome,(unsigned long long)epoch,blocks,field.rows,ack_time?ack_time-held:0);fclose(trace);field_free(&field);munmap((void*)r,4096);close(fd);close(lock);return 0;
}

void obs_preview_cancel(void){interrupted=1;}
