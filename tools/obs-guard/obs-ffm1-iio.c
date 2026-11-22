/* RAM acceptance reader for an existing WAT1-marked map. One libiio buffer
 * for the complete session. This process cannot program a visit or RF map. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <glob.h>
#include <poll.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include "obs-field-preview.h"
#include "obs-stream-002.h"
#include "obs-watch-policy.h"

enum { FFM_ID=OBS_FFM1_ID_REG, SESSION=OBS_FFM1_SESSION, POP=OBS_FFM1_POP, LEVEL=OBS_FFM1_LEVEL,
       LOST=OBS_FFM1_LOST, HEAD=OBS_FFM1_HEAD, RUN=0x284, COMMAND=0x290, STATUS=0x294,
       FIELD_CANCEL=1u<<1, FIELD_SOURCE_FAULT=1u<<4, FIELD_SOURCE_READY=1u<<5,
       FIELD_ACTIVE=(1u<<0)|(1u<<6)|(1u<<7),
       FFM_LEVEL_COUNT=0x1ffu, FFM_HEAD_AVAILABLE=1u<<16,
       BLOCK=262144, FIELD_MAX=337924, RETAIN=2*BLOCK+2*FIELD_MAX };
static volatile sig_atomic_t stopping;
static void stop_session(int sig) { (void)sig; stopping=1; }
static double monotonic(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC,&t);
    return t.tv_sec+t.tv_nsec*1e-9;
}
#include "obs-field-egress.h"
static int iio_in_use(void)
{
    glob_t paths={0};
    int status=glob("/sys/bus/iio/devices/iio:device*/name",0,NULL,&paths);
    int busy=-1;
    if(status) return -1;
    for(size_t i=0;i<paths.gl_pathc && busy<0;++i) {
        char name[128],pattern[1024];
        FILE *file=fopen(paths.gl_pathv[i],"r");
        if(!file) continue;
        int found=0;
        if(fgets(name,sizeof name,file)) {
            name[strcspn(name,"\r\n")]=0;
            found=!strcmp(name,"cf-ad9361-lpc");
        }
        fclose(file);
        if(!found) continue;
        size_t prefix=strlen(paths.gl_pathv[i])-strlen("name");
        if(prefix+sizeof("buffer*/enable")>sizeof pattern) break;
        memcpy(pattern,paths.gl_pathv[i],prefix);
        strcpy(pattern+prefix,"buffer*/enable");
        glob_t buffers={0};
        if(glob(pattern,0,NULL,&buffers)) break;
        busy=0;
        for(size_t j=0;j<buffers.gl_pathc;++j) {
            file=fopen(buffers.gl_pathv[j],"r");
            int state=file?fgetc(file):EOF;
            if(file) fclose(file);
            if(state!='0') {busy=1;break;}
        }
        globfree(&buffers);
    }
    globfree(&paths);
    return busy;
}
static void write_reg(volatile uint32_t *reg, unsigned offset, uint32_t value)
{
    reg[offset/4]=value;
    __sync_synchronize();
}
static void report_error(volatile uint32_t *reg, const char *reason, ssize_t iio_result)
{
    fprintf(stderr,"FFM1_ERROR stage=%s iio=%zd status=%08x session=%u level=%08x\n",
        reason,iio_result,reg==MAP_FAILED?0:reg[STATUS/4],
        reg==MAP_FAILED?0:reg[SESSION/4],reg==MAP_FAILED?0:reg[LEVEL/4]);
}
static void quiesce_source(volatile uint32_t *reg, const char *reason, ssize_t iio_result)
{
    report_error(reg,reason,iio_result);
    write_reg(reg,SESSION,0);
    write_reg(reg,COMMAND,FIELD_CANCEL);
    write_reg(reg,RUN,0);
}
struct iio_api {
    void *(*create_local_context)(void);
    void (*context_destroy)(void *);
    void *(*context_find_device)(void *,const char *);
    int (*context_set_timeout)(void *,unsigned);
    void *(*device_find_channel)(void *,const char *,bool);
    void (*channel_enable)(void *);
    int (*device_set_kernel_buffers_count)(void *,unsigned);
    void *(*device_create_buffer)(void *,size_t,bool);
    void (*buffer_destroy)(void *);
    int (*buffer_set_blocking_mode)(void *,bool);
    int (*buffer_get_poll_fd)(void *);
    ssize_t (*buffer_refill)(void *);
    void *(*buffer_start)(void *);
    void *(*buffer_end)(void *);
};
static int load_api(void *lib,struct iio_api *api)
{
#define LOAD(name) do { api->name=dlsym(lib,"iio_" #name); if (!api->name) return -1; } while(0)
    LOAD(create_local_context); LOAD(context_destroy); LOAD(context_find_device);
    LOAD(context_set_timeout); LOAD(device_find_channel); LOAD(channel_enable);
    LOAD(device_set_kernel_buffers_count); LOAD(device_create_buffer); LOAD(buffer_destroy);
    LOAD(buffer_set_blocking_mode); LOAD(buffer_get_poll_fd); LOAD(buffer_refill);
    LOAD(buffer_start); LOAD(buffer_end);
#undef LOAD
    return 0;
}
struct observation {
    uint64_t epoch;
    uint32_t version,position,outcome,started,first,end;
};
static struct observation get_head(volatile uint32_t *reg)
{
    uint32_t words[6];
    for(unsigned i=0;i<6;++i) words[i]=reg[HEAD/4+i];
    __sync_synchronize();
    return (struct observation){words[0]|((uint64_t)words[1]<<32),words[2],
        words[3]&65535,(words[3]>>16)&15,(words[3]>>20)&1,words[4],words[5]};
}
static int load_version(const char *directory,uint32_t version,struct watch_table *wat,uint8_t *data)
{
    char path[1024];
    int n=snprintf(path,sizeof path,"%s/WAT1-%u.bin",directory,version);
    if(n<0 || (size_t)n>=sizeof path) return -1;
    FILE *f=fopen(path,"rb");
    if(!f) return -1;
    size_t limit=WATCH_HEADER+WATCH_RECORD*WATCH_RESIDENT_MAX+1;
    size_t len=fread(data,1,limit,f);
    int bad=ferror(f) || len==limit;
    if(fclose(f) || bad || watch_table_read(wat,data,len,WATCH_RESIDENT_MAX,13000000000ull)) return -1;
    return wat->version==version?0:-1;
}
static int report(const struct observation *o, const struct watch_table *wat,
                  enum preview_result status, const uint8_t *payload, size_t bytes,
                  struct field_egress *egress, bool send_enabled, double receipt_time)
{
    struct watch_target task={0};
    if(o->version==wat->version && o->position<wat->count) task=watch_row(wat,o->position);
    bool known=task.enabled;
    char metadata[FIELD_META];
    double ready=monotonic();
    int count=snprintf(metadata,sizeof metadata,
        "{\"kind\":\"field\",\"version\":%u,\"position\":%u,\"epoch\":%llu,\"started\":%u,"
        "\"outcome\":%u,\"revision\":%llu,\"generation\":%llu,\"target_hz\":%llu,"
        "\"first\":%u,\"end\":%u,\"payload_status\":%u,\"consumed_field_bytes\":%zu,"
        "\"receipt_arm_s\":%.9f,\"payload_ready_arm_s\":%.9f}",
        o->version,o->position,(unsigned long long)o->epoch,o->started,o->outcome,
        (unsigned long long)(known?wat->revision:0),
        (unsigned long long)task.generation,(unsigned long long)task.hz,
        o->first,o->end,(unsigned)status,bytes,receipt_time,ready);
    if(count<0 || (size_t)count>=sizeof metadata) return -1;
    if(send_enabled) field_queue(egress,metadata,(size_t)count,payload,bytes,ready);
#ifdef OBS_SCAN_STREAM_BENCH
    if (!send_enabled || getenv("OBS_FFM1_TRACE")) {
        if (puts(metadata)<0) return -1;
    }
#endif
    return !known;
}

int main(int argc,char **argv)
{
    if(argc!=3) { fprintf(stderr,"usage: obs-ffm1-iio WAT1-version-directory timeout-ms\n");return 2; }
    char *endptr;
    unsigned long ms=strtoul(argv[2],&endptr,10);
    if(*endptr || !ms || ms>1000) return 2;
    int rc=1,owner=-1,guard=-1,mem=-1,stream_owned=0,pollfd=-1;
    const char *problem="allocate history and connect FIELD transport";
    const char *source_failure=NULL;
    ssize_t iio_result=0;
    uint32_t oldmode=0,oldcode=0,oldtimeout=0,oldrun=0;
    void *lib=NULL,*context=NULL,*buffer=NULL;
    struct iio_api io={0};
    volatile uint32_t *reg=MAP_FAILED;
    struct watch_table wat={0};
    uint8_t *watch_bytes=malloc(WATCH_HEADER+WATCH_RECORD*WATCH_RESIDENT_MAX+1);
    uint8_t *storage=malloc(RETAIN),*preview=malloc(FIELD_MAX);
    struct field_egress egress={.fd=-1};
    const char *host=getenv("OBS_FFM1_HOST"), *port=getenv("OBS_FFM1_PORT");
    bool send_enabled=host && port;
    struct preview_history history;
    if(!watch_bytes || !preview || preview_init(&history,storage,RETAIN) ||
       (!!host!=!!port) || (send_enabled && field_connect(&egress,host,port,FIELD_MAX))) goto done;
    problem="load libiio";
    lib=dlopen("libiio.so.0",RTLD_NOW);
    if(!lib || load_api(lib,&io)) goto done;
    problem="acquire exclusive IIO ownership";
    owner=open("/run/obs-field1.lock",O_RDWR|O_CREAT|O_CLOEXEC,0600);
    if(owner<0 || flock(owner,LOCK_EX|LOCK_NB)) goto done;
    if(iio_in_use()!=0) goto done;
    guard=open("/dev/obs_fl_guard",O_RDONLY|O_CLOEXEC);
    uint32_t guard_status[128]={0};
    const char *authorization=getenv("OBS_FIELD1_TX_AUTHORIZED");
    unsigned expected_powerdown=!(authorization && !strcmp(authorization,"1"));
    /* Keep the accepted collector's per-series readback/ownership check. */
    problem="verify RF guard ownership and TX readback";
    if(guard<0 || ioctl(guard,0x4605,guard_status) || guard_status[0]!=2 ||
       guard_status[1]!=2 || guard_status[2] || guard_status[112]!=expected_powerdown) goto done;
    mem=open("/dev/mem",O_RDWR|O_SYNC|O_CLOEXEC);
    if(mem<0) goto done;
    problem="check FLD2/FFM1 ABI and idle session";
    reg=mmap(NULL,4096,PROT_READ|PROT_WRITE,MAP_SHARED,mem,0x7c460000);
    if(reg==MAP_FAILED || reg[FFM_ID/4]!=OBS_FFM1 || reg[0x280/4]!=OBS_FLD2 || reg[SESSION/4] ||
       reg[STATUS/4]&FIELD_ACTIVE || reg[LEVEL/4]&FFM_LEVEL_COUNT) goto done;
    oldmode=reg[0x38/4];oldcode=reg[0x40/4];oldtimeout=reg[0x28c/4];
    oldrun=reg[RUN/4];stream_owned=1;
    write_reg(reg,RUN,0);write_reg(reg,0x38,1);write_reg(reg,0x40,24);
    write_reg(reg,0x28c,(uint32_t)ms*62500);
    problem="configure the IIO device and channels";
    context=io.create_local_context();
    void *dev=context?io.context_find_device(context,"cf-ad9361-lpc"):NULL;
    if(!dev || io.context_set_timeout(context,250) || io.device_set_kernel_buffers_count(dev,4)) goto done;
    void *i=io.device_find_channel(dev,"voltage0",false),*q=io.device_find_channel(dev,"voltage1",false);
    if(!i || !q) goto done;
    io.channel_enable(i);io.channel_enable(q);
    problem="create the nonblocking IIO buffer";
    buffer=io.device_create_buffer(dev,BLOCK/4,false);
    if(!buffer || io.buffer_set_blocking_mode(buffer,false)) goto done;
    if((uint8_t*)io.buffer_end(buffer)-(uint8_t*)io.buffer_start(buffer)!=BLOCK ||
       (pollfd=io.buffer_get_poll_fd(buffer))<0) goto done;
    problem="verify IIO is empty before real8 RUN";
    iio_result=io.buffer_refill(buffer);
    if(iio_result!=-EAGAIN) goto done;
    signal(SIGINT,stop_session);signal(SIGTERM,stop_session);
    problem="real8 source failed to become ready";
    write_reg(reg,RUN,1);
    double deadline=monotonic()+0.25;
    for(;;) {
        uint32_t state=reg[STATUS/4];
        if(stopping || monotonic()>=deadline || state&FIELD_SOURCE_FAULT) goto done;
        if(state&FIELD_SOURCE_READY) break;
    }
    uint32_t lost_start=reg[LOST/4];
    write_reg(reg,SESSION,1);
    fprintf(stderr,"FFM1_READY block=%d history=%d\n",BLOCK,RETAIN);
    struct observation obs={0};
    uint64_t first64=0,end64=0;
    double receipt_time=0;
    int pending=0,draining=0,failed=0;
    rc=0;
    problem=NULL;
    for(;;) {
        if(stopping && !draining) {
            /* The map owner has stopped admission before signalling shutdown.
             * Keep IIO RUN asserted during the bounded suffix drain. */
            write_reg(reg,COMMAND,FIELD_CANCEL);
            draining=1;
            deadline=monotonic()+1.25;
        }
        ssize_t got=-EAGAIN;
        if(!source_failure) {
            got=io.buffer_refill(buffer);
            iio_result=got;
            if(got>0) {
                if(got>BLOCK || preview_append(&history,io.buffer_start(buffer),(size_t)got))
                    source_failure="invalid IIO block";
            } else if(got!=-EAGAIN && got!=-EINTR) source_failure="IIO refill failed";
            if(!source_failure && reg[STATUS/4]&FIELD_SOURCE_FAULT)
                source_failure="real8 hardware reported a source fault";
            if(source_failure) {
                /* Stop this generation of real8 before any new data can enter history.
                 * FFM1 still reports the marked visits; this remains its sole reader. */
                quiesce_source(reg,source_failure,iio_result);
                rc=4;
            }
        }
        int expired=draining && monotonic()>=deadline;
        while(pending || reg[LEVEL/4]&FFM_HEAD_AVAILABLE) {
            enum preview_result status=PREVIEW_READY;
            if(!pending) {
                obs=get_head(reg);
                receipt_time=monotonic();
                if(wat.version!=obs.version && load_version(argv[1],obs.version,&wat,watch_bytes)) {
                    wat=(struct watch_table){0};
                    failed=1;
                }
                if(!obs.outcome || obs.outcome>7 || (obs.outcome!=1 && (obs.first||obs.end))) failed=1;
                if(obs.outcome==1) status=preview_bounds(&history,obs.first,obs.end,&first64,&end64);
                write_reg(reg,POP,1);
                pending=1;
            }
            if(obs.outcome==1 && status==PREVIEW_READY)
                status=preview_copy(&history,first64,end64,preview,FIELD_MAX);
            if(status==PREVIEW_WAIT && !expired && !source_failure) break;
            if(status==PREVIEW_WAIT) status=PREVIEW_LOST;
            size_t extent=status==PREVIEW_READY && obs.outcome==1?(size_t)(end64-first64):0;
            if(report(&obs,&wat,status,preview,extent,&egress,send_enabled,receipt_time) ||
               status!=PREVIEW_READY) failed=1;
            pending=0;
        }
        field_pump(&egress);
        if(expired || (draining && !pending && !egress.count && !(reg[STATUS/4]&FIELD_ACTIVE) &&
           !(reg[LEVEL/4]&FFM_LEVEL_COUNT))) break;
        if(got>0) continue;
        struct pollfd p[2]={{.fd=source_failure?-1:pollfd,.events=POLLIN},
                            {.fd=egress.count?egress.fd:-1,.events=POLLOUT}};
        /* The accepted FIELD timeout bounds receipt observation if the IIO
         * source goes quiet; admission itself is exclusively in hardware. */
        int wait_ms=draining?(int)((deadline-monotonic())*1000)+1:(int)ms;
        if(wait_ms<0) wait_ms=0;
        if(wait_ms>20) wait_ms=20;
        if(poll(p,2,wait_ms)<0 && errno!=EINTR) {
            if(source_failure) break;
            source_failure="IIO poll failed";
        } else if(p[0].revents&(POLLHUP|POLLERR|POLLNVAL)) {
            source_failure="IIO poll reported a source fault";
        }
        if(source_failure && p[0].fd>=0) {
            quiesce_source(reg,source_failure,iio_result);
            rc=4;
        }
    }
    field_abandon(&egress);
    if(reg[LOST/4]!=lost_start || pending || failed || reg[LEVEL/4]&FFM_LEVEL_COUNT) rc=4;
    fprintf(stderr,"FFM1_STOP bytes=%llu receipt_lost=%u error=%d\n",
        (unsigned long long)history.consumed,reg[LOST/4]-lost_start,rc);
    fprintf(stderr,"FFM1_EGRESS sent=%llu sent_bytes=%llu accepted_bytes=%llu rejected=%llu abandoned=%llu "
        "partial=%llu blocked=%llu high=%u high_bytes=%zu socket_peak=%d "
        "max_wait_s=%.9f max_send_s=%.9f max_total_s=%.9f\n",
        (unsigned long long)egress.sent,(unsigned long long)egress.sent_bytes,
        (unsigned long long)egress.accepted_bytes,(unsigned long long)egress.rejected,
        (unsigned long long)egress.abandoned,(unsigned long long)egress.partial,
        (unsigned long long)egress.blocked,egress.peak,egress.peak_bytes,egress.socket_peak,
        egress.max_queue_s,egress.max_send_s,egress.max_ready_s);
done:
    if(rc && !source_failure) report_error(reg,problem?problem:"receipt, history or drain fault",iio_result);
    if(stream_owned) {write_reg(reg,SESSION,0);write_reg(reg,COMMAND,FIELD_CANCEL);write_reg(reg,RUN,0);}
    if(buffer) io.buffer_destroy(buffer);
    if(context) io.context_destroy(context);
    if(stream_owned) {
        write_reg(reg,0x28c,oldtimeout);
        write_reg(reg,0x40,oldcode);write_reg(reg,0x38,oldmode);
        if(!source_failure) write_reg(reg,RUN,oldrun);
    }
    if(reg!=MAP_FAILED) munmap((void*)reg,4096);
    if(mem>=0) close(mem);
    if(guard>=0) close(guard);
    if(owner>=0) close(owner);
    if(lib) dlclose(lib);
    if(egress.fd>=0) close(egress.fd);
    for(unsigned i=0;i<FIELD_QUEUE;++i) free(egress.packets[i].bytes);
    free(watch_bytes);free(preview);free(storage);
    return rc;
}
