/* RAM bench. A real scan visit supplies identity; the input waveform is a fixture. */
#include <sys/un.h>

#define SYNTH_FIELD_BYTES 307208u
#define SYNTH_STAMP_OFFSET 8192u
static volatile sig_atomic_t synth_stop;

static void stop_synthetic(int sig){(void)sig;synth_stop=1;}

static int synthetic_field(uint64_t *v,uint32_t tag,uint8_t *raw,bool pal){
 char identity[768],meta[1280];
 snprintf(identity,sizeof(identity),"\"stream_tag\":%u,\"epoch\":%llu,\"watch_generation\":%llu,\"map_version\":%llu,"
          "\"pass_id\":%llu,\"pass_phase\":%llu,\"target_hz\":%llu,\"target_position\":%llu,\"signal_id\":%llu,"
          "\"watch_revision\":1,\"synthetic\":true,\"trigger\":\"scan_visit\",\"synthetic_state\":\"%s\",\"ready_arm_ns\":%llu",
          tag,(unsigned long long)v[4],(unsigned long long)v[0],(unsigned long long)v[1],
          (unsigned long long)v[2],(unsigned long long)v[3],(unsigned long long)v[7],(unsigned long long)v[8],
          (unsigned long long)v[6],pal?"pal":"present_no_sync",(unsigned long long)v[5]);
 for(unsigned i=0;i<8;i++)raw[SYNTH_STAMP_OFFSET+i]=(uint8_t)(64+((tag>>(4*i))&15));
 snprintf(meta,sizeof(meta),"{\"kind\":\"begin\",%s,\"fs\":%u,\"code\":24,\"g\":0,\"block_bytes\":65536,"
          "\"capture_seconds\":1,\"capture_limit_bytes\":%u,\"unqualified_raw\":%s,\"replay\":true}",
          identity,REAL8_FS,REAL8_FS+65536,pal?"false":"true");
 if(connect_video()||packet(meta,NULL,0))return -1;
 for(size_t offset=0;offset<SYNTH_FIELD_BYTES;offset+=65536){
  size_t n=SYNTH_FIELD_BYTES-offset;if(n>65536)n=65536;
  snprintf(meta,sizeof(meta),"{\"kind\":\"data\",\"stream_tag\":%u,\"offset\":%zu}",tag,offset);
  if(packet(meta,raw+offset,n))return -1;
 }
 snprintf(meta,sizeof(meta),"{\"kind\":\"end\",%s,\"first\":4,\"end\":%u,\"generation_bytes\":%u,"
          "\"pl_outcome\":%u,\"error\":0,\"qualified_field\":%s}",identity,SYNTH_FIELD_BYTES,SYNTH_FIELD_BYTES,
          pal?1:0,pal?"true":"false");
 return packet(meta,NULL,0);
}

static int synthetic_session(int argc,char **argv){
 if(argc!=7||!managed_boot)return 2;
 bool mixed=!strcmp(argv[5],"mixed");
 if(!mixed&&strcmp(argv[5],"all-pal"))return 2;
 char *end;unsigned long duration=strtoul(argv[6],&end,10);
 if(*end||!duration||duration>260)return 2;
 static uint8_t pal[SYNTH_FIELD_BYTES],noise[SYNTH_FIELD_BYTES];
 for(unsigned i=0;i<4;i++)pal[i]=noise[i]=noise[4+i]=i==3?24:128;
 FILE *fixture=fopen(argv[3],"rb");
 if(!fixture)return 2;
 size_t n=fread(pal+4,1,sizeof(pal)-4,fixture);
 fclose(fixture);
 if(n!=sizeof(pal)-4||memcmp(pal,pal+4,4))return 2;
 uint32_t state=1;
 for(unsigned i=8;i<sizeof(noise);i++){state^=state<<13;state^=state>>17;state^=state<<5;noise[i]=(uint8_t)(88+(state&15));}
 struct sockaddr_un a={.sun_family=AF_UNIX};
 if(strlen(argv[2])>=sizeof(a.sun_path))return 2;
 strcpy(a.sun_path,argv[2]);
 int fd=socket(AF_UNIX,SOCK_DGRAM,0);
 if(fd<0)return 2;
 if(bind(fd,(void*)&a,sizeof(a))){close(fd);return 2;}
 signal(SIGTERM,stop_synthetic);signal(SIGINT,stop_synthetic);
 uint64_t received=0,finished=0,pal_count=0,failures=0,invalid=0;
 double oldest=0,began=now(),drain_end=0;
 while(now()-began<duration){
  if(synth_stop && drain_end==0)drain_end=now()+4;
  if(drain_end && now()>drain_end)break;
  struct pollfd p={fd,POLLIN,0};
  int ready=poll(&p,1,drain_end?0:250);
  if(ready<0){if(errno==EINTR)continue;invalid++;break;}
  if(!ready){if(drain_end)break;continue;}
  uint64_t v[9];ssize_t length=recv(fd,v,sizeof(v),MSG_TRUNC);
  if(length!=sizeof(v)||v[3]!=(v[2]&1)||v[6]>=10||!v[0]||!v[1]){invalid++;continue;}
  uint32_t tag=(uint32_t)++received;
  double age=now()-v[5]*1e-9;if(age>oldest)oldest=age;
  bool qualified=!mixed||v[6]==0;
  if(synthetic_field(v,tag,qualified?pal:noise,qualified)){
   failures++;packet_abort();continue;
  }
  finished++;pal_count+=qualified;
 }
 packet_abort();close(fd);unlink(a.sun_path);
 struct rusage usage={0};getrusage(RUSAGE_SELF,&usage);
 FILE *out=fopen(argv[4],"w");if(!out)return 3;
 fprintf(out,"{\"visits_received\":%llu,\"fields_sent\":%llu,\"pal_sent\":%llu,\"present_no_sync_sent\":%llu,"
         "\"transport_failures\":%llu,\"invalid_events\":%llu,\"wire_bytes\":%llu,\"wire_accepted\":%llu,"
         "\"duration_s\":%.6f,\"cpu_s\":%.6f,\"max_rss_kib\":%ld,\"oldest_visit_s\":%.6f}\n",
         (unsigned long long)received,(unsigned long long)finished,(unsigned long long)pal_count,
         (unsigned long long)(finished-pal_count),(unsigned long long)failures,(unsigned long long)invalid,
         (unsigned long long)wire_bytes,(unsigned long long)wire_accepted,now()-began,
         usage.ru_utime.tv_sec+usage.ru_stime.tv_sec+(usage.ru_utime.tv_usec+usage.ru_stime.tv_usec)*1e-6,
         usage.ru_maxrss,oldest);
 fclose(out);return 0;
}
