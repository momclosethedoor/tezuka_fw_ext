/* RAM-only FIELD traffic. Visits trigger acquisition, samples become ready in blocks. */
#include <sys/un.h>
#include <pthread.h>

#define SYNTH_FIELD_BYTES 307208u
#define SYNTH_BLOCK_BYTES 65536u
#define SYNTH_STAMP_OFFSET 8192u
enum { SYNTH_SIGNALS=10, SYNTH_QUEUE=4 };
static volatile sig_atomic_t synth_stop;
static void stop_synthetic(int sig){(void)sig;synth_stop=1;}

struct synth_job { uint64_t visit[9], field_end_ns; uint32_t tag, delay_ms; };
struct synth_sender {
 pthread_mutex_t lock;
 pthread_cond_t wake;
 struct synth_job jobs[SYNTH_QUEUE];
 unsigned head,count,peak;
 bool closed,pal;
 const uint8_t *waveform;
 uint8_t first_block[SYNTH_BLOCK_BYTES];
 uint64_t sent,failed,dropped,wire,accepted;
 double oldest,max_service,stop_deadline;
};

static uint64_t synthetic_clock_ns(void){
 struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);
 return (uint64_t)t.tv_sec*1000000000ull+t.tv_nsec;
}

static void synthetic_wait(uint64_t ns){
 struct timespec t={.tv_sec=ns/1000000000ull,.tv_nsec=ns%1000000000ull};
 while(clock_nanosleep(CLOCK_MONOTONIC,TIMER_ABSTIME,&t,NULL)==EINTR){}
}

static uint64_t synthetic_block_ready(const struct synth_job *job,size_t end){
 return job->field_end_ns-((SYNTH_FIELD_BYTES-end)*1000000000ull)/REAL8_FS;
}

static int synthetic_field(struct synth_sender *s,const struct synth_job *job){
 const uint64_t *v=job->visit;uint32_t tag=job->tag;
 char identity[896],meta[1536];
 snprintf(identity,sizeof(identity),"\"stream_tag\":%u,\"epoch\":%llu,\"watch_generation\":%llu,\"map_version\":%llu,"
          "\"pass_id\":%llu,\"pass_phase\":%llu,\"target_hz\":%llu,\"target_position\":%llu,\"signal_id\":%llu,"
          "\"watch_revision\":1,\"synthetic\":true,\"trigger\":\"scan_visit\",\"synthetic_state\":\"%s\","
          "\"visit_arm_ns\":%llu,\"field_end_arm_ns\":%llu,\"capture_delay_ms\":%u",
          tag,(unsigned long long)v[4],(unsigned long long)v[0],(unsigned long long)v[1],
          (unsigned long long)v[2],(unsigned long long)v[3],(unsigned long long)v[7],(unsigned long long)v[8],
          (unsigned long long)v[6],s->pal?"pal":"present_no_sync",(unsigned long long)v[5],
          (unsigned long long)job->field_end_ns,job->delay_ms);
 for(unsigned i=0;i<8;i++)s->first_block[SYNTH_STAMP_OFFSET+i]=(uint8_t)(64+((tag>>(4*i))&15));
 snprintf(meta,sizeof(meta),"{\"kind\":\"begin\",%s,\"fs\":%u,\"code\":24,\"g\":0,\"block_bytes\":%u,"
          "\"capture_seconds\":1,\"capture_limit_bytes\":%u,\"unqualified_raw\":%s,\"replay\":true}",
          identity,REAL8_FS,SYNTH_BLOCK_BYTES,REAL8_FS+SYNTH_BLOCK_BYTES,s->pal?"false":"true");
 uint64_t first=synthetic_block_ready(job,SYNTH_BLOCK_BYTES);
 synthetic_wait(first);
 double start=now(),age=start-first*1e-9;
 if(age>s->oldest)s->oldest=age;
 if(connect_video()||packet(meta,NULL,0))return -1;
 for(size_t offset=0;offset<SYNTH_FIELD_BYTES;offset+=SYNTH_BLOCK_BYTES){
  size_t n=SYNTH_FIELD_BYTES-offset;if(n>SYNTH_BLOCK_BYTES)n=SYNTH_BLOCK_BYTES;
  uint64_t ready=synthetic_block_ready(job,offset+n);
  synthetic_wait(ready);
  snprintf(meta,sizeof(meta),"{\"kind\":\"data\",\"stream_tag\":%u,\"offset\":%zu,\"ready_arm_ns\":%llu,\"send_arm_ns\":%llu}",
           tag,offset,(unsigned long long)ready,(unsigned long long)synthetic_clock_ns());
  const uint8_t *raw=offset?s->waveform+offset:s->first_block;
  if(packet(meta,raw,n))return -1;
 }
 snprintf(meta,sizeof(meta),"{\"kind\":\"end\",%s,\"first\":4,\"end\":%u,\"generation_bytes\":%u,"
          "\"pl_outcome\":%u,\"error\":0,\"qualified_field\":%s}",identity,SYNTH_FIELD_BYTES,SYNTH_FIELD_BYTES,
          s->pal?1:0,s->pal?"true":"false");
 if(packet(meta,NULL,0))return -1;
 double elapsed=now()-start;if(elapsed>s->max_service)s->max_service=elapsed;
 return 0;
}

static void *synth_send_loop(void *arg){
 struct synth_sender *s=arg;
 memcpy(s->first_block,s->waveform,SYNTH_BLOCK_BYTES);
 for(;;){
  pthread_mutex_lock(&s->lock);
  while(!s->closed&&!s->count)pthread_cond_wait(&s->wake,&s->lock);
  if(!s->count||(s->closed&&now()>s->stop_deadline)){pthread_mutex_unlock(&s->lock);break;}
  struct synth_job job=s->jobs[s->head];s->head=(s->head+1)%SYNTH_QUEUE;s->count--;
  pthread_mutex_unlock(&s->lock);
  if(synthetic_field(s,&job)){s->failed++;packet_abort();continue;}
  s->sent++;
 }
 s->wire=wire_bytes;s->accepted=wire_accepted;packet_abort();return NULL;
}

static void synth_enqueue(struct synth_sender *s,const struct synth_job *job){
 pthread_mutex_lock(&s->lock);
 if(s->count<SYNTH_QUEUE){
  s->jobs[(s->head+s->count)%SYNTH_QUEUE]=*job;
  s->count++;if(s->count>s->peak)s->peak=s->count;
  pthread_cond_signal(&s->wake);
 }else s->dropped++;
 pthread_mutex_unlock(&s->lock);
}

static int synthetic_session(int argc,char **argv){
 if(argc!=8||!managed_boot)return 2;
 bool mixed=!strcmp(argv[5],"mixed");
 if(!mixed&&strcmp(argv[5],"all-pal"))return 2;
 char *end;
 unsigned long duration=strtoul(argv[6],&end,10);
 if(*end||!duration||duration>260)return 2;
 unsigned long count=strtoul(argv[7],&end,10);
 if(*end||!count||count>SYNTH_SIGNALS)return 2;
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
 static struct synth_sender senders[SYNTH_SIGNALS];
 pthread_t threads[SYNTH_SIGNALS];
 unsigned started=0;
 for(;started<count;started++){
  struct synth_sender *s=&senders[started];
  pthread_mutex_init(&s->lock,NULL);pthread_cond_init(&s->wake,NULL);
  s->pal=!mixed||started==0;s->waveform=s->pal?pal:noise;
  if(pthread_create(&threads[started],NULL,synth_send_loop,s))break;
 }
 uint64_t received=0,invalid=0,first_visit=0,last_visit=0;
 double began=now();
 while(started==count&&now()-began<duration){
  struct pollfd p={fd,POLLIN,0};
  int ready=poll(&p,1,synth_stop?0:250);
  if(ready<0){if(errno==EINTR)continue;invalid++;break;}
  if(!ready){if(synth_stop)break;continue;}
  struct synth_job job={0};uint64_t *v=job.visit;
  ssize_t length=recv(fd,v,sizeof(job.visit),MSG_TRUNC);
  if(length!=(ssize_t)sizeof(job.visit)||v[3]!=(v[2]&1)||v[6]>=count||!v[0]||!v[1]){invalid++;continue;}
  if(!first_visit)first_visit=v[5];
  last_visit=v[5];
  job.tag=(uint32_t)++received;
  job.delay_ms=40+(unsigned)((v[2]*7+v[6]*11)%21);
  job.field_end_ns=v[5]+job.delay_ms*1000000ull;
  synth_enqueue(&senders[v[6]],&job);
 }
 close(fd);unlink(a.sun_path);
 for(unsigned i=0;i<started;i++){
  struct synth_sender *s=&senders[i];
  pthread_mutex_lock(&s->lock);s->closed=true;s->stop_deadline=now()+5;pthread_cond_signal(&s->wake);pthread_mutex_unlock(&s->lock);
 }
 uint64_t finished=0,pal_count=0,failed=0,dropped=0,pending=0,wire=0,accepted=0;
 unsigned peak=0;double oldest=0,max_service=0;
 for(unsigned i=0;i<started;i++){
  pthread_join(threads[i],NULL);
  struct synth_sender *s=&senders[i];
  finished+=s->sent;pal_count+=s->pal?s->sent:0;failed+=s->failed;dropped+=s->dropped;
  wire+=s->wire;accepted+=s->accepted;pending+=s->count;
  if(s->peak>peak)peak=s->peak;
  if(s->oldest>oldest)oldest=s->oldest;
  if(s->max_service>max_service)max_service=s->max_service;
  pthread_mutex_destroy(&s->lock);pthread_cond_destroy(&s->wake);
 }
 struct rusage usage={0};getrusage(RUSAGE_SELF,&usage);
 FILE *out=fopen(argv[4],"w");if(!out)return 3;
 fprintf(out,"{\"visits_received\":%llu,\"fields_sent\":%llu,\"pal_sent\":%llu,\"present_no_sync_sent\":%llu,"
         "\"transport_failures\":%llu,\"invalid_events\":%llu,\"wire_bytes\":%llu,\"wire_accepted\":%llu,"
         "\"first_visit_arm_ns\":%llu,\"last_visit_arm_ns\":%llu,\"send_lanes\":%u,\"queue_per_lane\":%u,"
         "\"lane_queue_peak\":%u,\"lane_drops\":%llu,\"pending_at_stop\":%llu,"
         "\"duration_s\":%.6f,\"cpu_s\":%.6f,\"max_rss_kib\":%ld,\"oldest_first_block_s\":%.6f,\"max_field_service_s\":%.6f}\n",
         (unsigned long long)received,(unsigned long long)finished,(unsigned long long)pal_count,
         (unsigned long long)(finished-pal_count),(unsigned long long)failed,(unsigned long long)invalid,
         (unsigned long long)wire,(unsigned long long)accepted,(unsigned long long)first_visit,(unsigned long long)last_visit,
         started,SYNTH_QUEUE,peak,(unsigned long long)dropped,(unsigned long long)pending,
         now()-began,usage.ru_utime.tv_sec+usage.ru_stime.tv_sec+(usage.ru_utime.tv_usec+usage.ru_stime.tv_usec)*1e-6,
         usage.ru_maxrss,oldest,max_service);
 fclose(out);return started==count?0:3;
}
