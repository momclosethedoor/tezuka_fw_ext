/* Real PL -> A9 -> Windows TCP ACK -> PL route exerciser. No GPIO or TX here. */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <time.h>
#include <errno.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <arpa/inet.h>
static volatile uint32_t *r;
static uint32_t rd(unsigned o){return r[o/4];}
static void wr(unsigned o,uint32_t v){r[o/4]=v;__sync_synchronize();}
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec*1e-9;}
int main(int argc,char**argv){
 if(argc!=3)return 2;unsigned seconds=strtoul(argv[2],0,0);if(!seconds||seconds>120)return 2;
 int fd=open("/dev/mem",O_RDWR|O_SYNC);r=mmap(0,4096,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0x7c460000);if(r==MAP_FAILED||rd(0x224)!=0x31444c46)return 3;
 FILE *out=fopen(argv[1],"wx");if(!out)return 3;
 const char *cases[]={"immediate","delayed","missing_then_correct","stale","duplicate"};unsigned count=0,last=0;double start=now();
 while(now()-start<seconds&&count<2000){
  if(!(rd(0x228)&8)||rd(0x240)==last){usleep(100);continue;}
  unsigned id=rd(0x240),version=rd(0x22c),position=rd(0x234),route=rd(0x244);uint64_t epoch=((uint64_t)rd(0x250)<<32)|rd(0x24c);
  const char *test=cases[count%5];double marker=now();last=id;count++;
  int s=socket(AF_INET,SOCK_STREAM,0);struct timeval tv={1,0};setsockopt(s,SOL_SOCKET,SO_RCVTIMEO,&tv,sizeof(tv));setsockopt(s,SOL_SOCKET,SO_SNDTIMEO,&tv,sizeof(tv));
  struct sockaddr_in a={.sin_family=AF_INET,.sin_port=htons(18008)};inet_pton(AF_INET,"192.168.2.10",&a.sin_addr);
  if(connect(s,(void*)&a,sizeof(a))){fprintf(out,"{\"event\":\"connect_error\",\"marker_id\":%u,\"errno\":%d}\n",id,errno);close(s);break;}
  char msg[512];int n=snprintf(msg,sizeof(msg),"{\"marker_id\":%u,\"epoch\":%llu,\"version\":%u,\"position\":%u,\"route\":%u,\"a9_send_s\":%.9f,\"test\":\"%s\"}\n",id,(unsigned long long)epoch,version,position,route,now(),test);
  if(send(s,msg,n,0)!=n){close(s);break;}
  char reply[2048];size_t have=0;int accepted=0;
  while(now()-marker<2&&have<sizeof(reply)-1){
   n=recv(s,reply+have,sizeof(reply)-1-have,0);if(n<=0)break;have+=n;reply[have]=0;
   char *line=reply,*end;
   while((end=strchr(line,'\n'))){
    *end=0;unsigned ack=0;
    if(sscanf(line,"ACK %u",&ack)==1){
     unsigned before=rd(0x240);int pending=!!(rd(0x228)&8);double received=now();
     wr(0x218,ack);wr(0x220,2);usleep(30);
     int correct=ack==id&&before==id&&pending;
     if(correct)accepted=1;
     fprintf(out,"{\"event\":\"ack_applied\",\"test\":\"%s\",\"marker_id\":%u,\"ack\":%u,\"version\":%u,\"position\":%u,\"epoch\":%llu,\"marker_observed_s\":%.9f,\"receive_s\":%.9f,\"after_s\":%.9f,\"pending_before\":%d,\"id_before\":%u,\"id_after\":%u,\"pending_after\":%d,\"correct\":%d}\n",test,id,ack,version,position,(unsigned long long)epoch,marker,received,now(),pending,before,rd(0x240),!!(rd(0x228)&8),correct);
    }
    line=end+1;
   }
   size_t tail=reply+have-line;memmove(reply,line,tail);have=tail;
  }
  close(s);fflush(out);if(!accepted)break;
 }
 fclose(out);munmap((void*)r,4096);close(fd);return 0;
}
