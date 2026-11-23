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
       BLOCK=262144, FIELD_MAX=337924, IIO_BLOCKS=4,
       HISTORY_BLOCKS=3, RETAIN=HISTORY_BLOCKS*BLOCK };
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
};
static int load_api(void *lib,struct iio_api *api)
{
#define LOAD(name) do { api->name=dlsym(lib,"iio_" #name); if (!api->name) return -1; } while(0)
    LOAD(create_local_context); LOAD(context_destroy); LOAD(context_find_device);
    LOAD(context_set_timeout); LOAD(device_find_channel); LOAD(channel_enable);
    LOAD(device_set_kernel_buffers_count); LOAD(device_create_buffer); LOAD(buffer_destroy);
    LOAD(buffer_set_blocking_mode); LOAD(buffer_get_poll_fd);
#undef LOAD
    return 0;
}

/* ADI's legacy mmap block ABI, also used by libiio 0.26 local.c. Let libiio
 * configure and tear down the session. Use its fd for explicit dequeue/enqueue
 * so receipt handling and send never run with a block checked out. Do not
 * mix this with iio_buffer_refill(), which maintains its own dequeued ID. */
struct iio_block {
    uint32_t id, size, bytes_used, type, flags, offset;
    uint64_t timestamp;
};
_Static_assert(sizeof(struct iio_block)==32,"legacy IIO block ABI");
#define IIO_QUERY_BLOCK _IOWR('i',0xa2,struct iio_block)
#define IIO_ENQUEUE_BLOCK _IOWR('i',0xa3,struct iio_block)
#define IIO_DEQUEUE_BLOCK _IOWR('i',0xa4,struct iio_block)

struct iio_blocks {
    void *maps[IIO_BLOCKS];
#ifdef OBS_SCAN_STREAM_BENCH
    uint64_t dequeued, returned;
    double max_checkout;
    unsigned last_id;
#endif
};

static int block_ioctl(int fd, unsigned long request, struct iio_block *block)
{
    int result;
    do { result=ioctl(fd,request,block); } while(result<0 && errno==EINTR && !stopping);
    return result<0 ? -errno : result;
}

static int map_blocks(int fd, struct iio_blocks *blocks)
{
    for(unsigned i=0;i<IIO_BLOCKS;++i) {
        struct iio_block block={.id=i};
        int result=block_ioctl(fd,IIO_QUERY_BLOCK,&block);
        if(result<0) return result;
        if(block.id!=i || block.size!=BLOCK) return -EINVAL;
        void *map=mmap(NULL,BLOCK,PROT_READ,MAP_SHARED,fd,block.offset);
        if(map==MAP_FAILED) return -errno;
        blocks->maps[i]=map;
    }
    return 0;
}

static ssize_t read_iio_block(int fd, struct iio_blocks *blocks,
                              struct preview_history *history, const char **problem)
{
    struct iio_block block={0};
    int result=block_ioctl(fd,IIO_DEQUEUE_BLOCK,&block);
    if(result<0) {
        if(result!=-EAGAIN && result!=-EINTR) *problem="IIO block dequeue failed";
        return result;
    }
#ifdef OBS_SCAN_STREAM_BENCH
    ++blocks->dequeued;
    double start=monotonic();
#endif
    if(block.id>=IIO_BLOCKS || block.size!=BLOCK) {
        *problem="IIO block identity invalid";
        return -EINVAL;
    }
    size_t bytes=block.bytes_used;
    bool bad=bytes>BLOCK || preview_append(history,blocks->maps[block.id],bytes);
    block.bytes_used=block.size;
    result=block_ioctl(fd,IIO_ENQUEUE_BLOCK,&block);
    if(result<0) {
        history->retained=0;
        *problem="IIO block return failed";
        return result;
    }
#ifdef OBS_SCAN_STREAM_BENCH
    ++blocks->returned;
    blocks->last_id=block.id;
    double elapsed=monotonic()-start;
    if(elapsed>blocks->max_checkout) blocks->max_checkout=elapsed;
#endif
    if(bad) {
        *problem="IIO block length invalid";
        return -EINVAL;
    }
    return (ssize_t)bytes;
}
struct observation {
    uint64_t epoch;
    uint32_t version,position,outcome,started,first,end;
};
#include "obs-fsb1-stream.h"

#ifdef OBS_SCAN_STREAM_BENCH
enum {
    DMAC_BASE=0x7c400000, DMAC_PAGE=4096, DMAC_HISTORY=8,
    DMAC_CONTROL=0x400, DMAC_NEXT_ID=0x404, DMAC_SUBMIT=0x408,
    DMAC_DONE=0x428, DMAC_EOT_ID=0x42c, DMAC_PROGRESS=0x448
};
struct dmac_sample {
    double begin, end;
    uint32_t cause, status, control, next_id, submit, done, eot_id, progress;
    uint64_t returned;
    unsigned block;
};
struct dmac_trace {
    volatile const uint32_t *reg;
    struct dmac_sample history[DMAC_HISTORY];
    unsigned next, count;
};
static struct dmac_trace dmac;

static struct dmac_sample dmac_read(const struct dmac_trace *trace, volatile const uint32_t *field,
                                    const struct iio_blocks *blocks)
{
    volatile const uint32_t *reg=trace->reg;
    struct dmac_sample sample={.begin=monotonic(),.returned=blocks->returned,.block=blocks->last_id};
    sample.cause=field[FSB_CAUSES/4];
    sample.status=field[STATUS/4];
    sample.control=reg[DMAC_CONTROL/4];
    sample.submit=reg[DMAC_SUBMIT/4];
    sample.next_id=reg[DMAC_NEXT_ID/4];
    sample.eot_id=reg[DMAC_EOT_ID/4];
    sample.done=reg[DMAC_DONE/4];
    sample.progress=reg[DMAC_PROGRESS/4];
    sample.end=monotonic();
    return sample;
}

static void dmac_remember(struct dmac_trace *trace, volatile const uint32_t *field,
                          const struct iio_blocks *blocks)
{
    trace->history[trace->next]=dmac_read(trace,field,blocks);
    trace->next=(trace->next+1)%DMAC_HISTORY;
    if(trace->count<DMAC_HISTORY) ++trace->count;
}

static void dmac_print(const struct dmac_sample *s, const char *stage)
{
    fprintf(stderr,"FSB1_DMAC stage=%s t0=%.9f t1=%.9f cause=%02x fld=%08x "
        "ctrl=%08x submit=%u next=%u eot=%u done=%08x progress=%u returned=%llu iio_id=%u\n",
        stage,s->begin,s->end,s->cause,s->status,s->control,s->submit,s->next_id,
        s->eot_id,s->done,s->progress,(unsigned long long)s->returned,s->block);
}

static void dmac_dump(const struct dmac_trace *trace, const struct dmac_sample *fault)
{
    unsigned first=(trace->next+DMAC_HISTORY-trace->count)%DMAC_HISTORY;
    for(unsigned n=0;n<trace->count;++n) dmac_print(&trace->history[(first+n)%DMAC_HISTORY],"after_return");
    dmac_print(fault,"before_stop");
}
#endif

static int wait_register(volatile uint32_t *reg, unsigned offset, uint32_t mask, uint32_t value)
{
    double deadline=monotonic()+0.1;
    while ((reg[offset/4]&mask)!=value) {
        if(monotonic()>=deadline) return -1;
        usleep(1000);
    }
    return 0;
}

static void quiesce_source(volatile uint32_t *reg, const char *reason, ssize_t iio_result,
                           uint64_t consumed, double refill_gap, const struct iio_blocks *blocks)
{
#ifdef OBS_SCAN_STREAM_BENCH
    struct dmac_sample fault={0};
    if(dmac.reg) fault=dmac_read(&dmac,reg,blocks);
#else
    (void)blocks;
#endif
    uint32_t status=reg[STATUS/4], session=reg[SESSION/4], before=reg[FSB_CAUSES/4];
    write_reg(reg,SESSION,0);
    write_reg(reg,COMMAND,FIELD_CANCEL);
    write_reg(reg,RUN,0);
    int settled=wait_register(reg,STATUS,FIELD_ACTIVE|FIELD_SOURCE_READY,0)==0;
    uint32_t after=reg[FSB_CAUSES/4];
    /* Allow sticky sampling-domain bits to settle while IIO remains open. */
    for(unsigned n=0;n<FSB_MAX_READS;++n) {
        usleep(1000);
        uint32_t next=reg[FSB_CAUSES/4];
        if(next==after) break;
        after=next;
    }
    fprintf(stderr,"FSB1_FAULT before=%02x after=%02x mask_before=%u mask_after=%u "
        "status=%08x session=%u iio=%zd consumed=%llu refill_gap_s=%.9f settled=%d stage=%s\n",
        before,after,before&FSB_CAUSE_MASK,after&FSB_CAUSE_MASK,status,session,iio_result,
        (unsigned long long)consumed,refill_gap,settled,reason);
#ifdef OBS_SCAN_STREAM_BENCH
    if(dmac.reg) dmac_dump(&dmac,&fault);
#endif
}
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
                  enum preview_result status, size_t bytes, bool send_enabled, double receipt_time)
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
#ifdef OBS_SCAN_STREAM_BENCH
    if (!send_enabled || getenv("OBS_FFM1_TRACE")) {
        if (puts(metadata)<0) return -1;
    }
#endif
    (void)send_enabled;
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
    struct iio_blocks blocks={0};
    volatile uint32_t *reg=MAP_FAILED;
    struct watch_table wat={0};
    uint8_t *watch_bytes=malloc(WATCH_HEADER+WATCH_RECORD*WATCH_RESIDENT_MAX+1);
    uint8_t *storage=malloc(RETAIN);
    struct field_egress egress={.fd=-1};
    const char *host=getenv("OBS_FFM1_HOST"), *port=getenv("OBS_FFM1_PORT");
    bool send_enabled=host && port;
    struct preview_history history;
    if(!watch_bytes || preview_init(&history,storage,RETAIN) ||
       (!!host!=!!port) || (send_enabled && field_connect(&egress,host,port,BLOCK))) goto done;
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
    if(reg==MAP_FAILED || reg[FFM_ID/4]!=OBS_FFM1 || reg[0x280/4]!=OBS_FLD2 ||
       reg[FSB_ID/4]!=UINT32_C(0x31425346) || reg[SESSION/4] ||
       reg[STATUS/4]&FIELD_ACTIVE || reg[LEVEL/4]&FFM_LEVEL_COUNT) goto done;
#ifdef OBS_SCAN_STREAM_BENCH
    if(getenv("OBS_FSB1_DMAC_TRACE")) {
        problem="map ADI DMAC read-only for bench diagnostics";
        void *mapping=mmap(NULL,DMAC_PAGE,PROT_READ,MAP_SHARED,mem,DMAC_BASE);
        if(mapping==MAP_FAILED) goto done;
        dmac.reg=mapping;
    }
#endif
    oldmode=reg[0x38/4];oldcode=reg[0x40/4];oldtimeout=reg[0x28c/4];
    oldrun=reg[RUN/4];stream_owned=1;
    write_reg(reg,RUN,0);write_reg(reg,0x38,1);write_reg(reg,0x40,24);
    write_reg(reg,0x28c,(uint32_t)ms*62500);
    problem="arm FSB1 sticky causes";
    write_reg(reg,FSB_DIAG,0);
    if(wait_register(reg,FSB_CAUSES,FSB_DIAG_READY|FSB_CAUSE_MASK,0)) goto done;
    write_reg(reg,FSB_DIAG,1);
    if(wait_register(reg,FSB_CAUSES,FSB_DIAG_READY,FSB_DIAG_READY)) goto done;
    problem="configure the IIO device and channels";
    context=io.create_local_context();
    void *dev=context?io.context_find_device(context,"cf-ad9361-lpc"):NULL;
    if(!dev || io.context_set_timeout(context,250) || io.device_set_kernel_buffers_count(dev,IIO_BLOCKS)) goto done;
    void *i=io.device_find_channel(dev,"voltage0",false),*q=io.device_find_channel(dev,"voltage1",false);
    if(!i || !q) goto done;
    io.channel_enable(i);io.channel_enable(q);
    problem="create the nonblocking IIO buffer";
    buffer=io.device_create_buffer(dev,BLOCK/4,false);
    if(!buffer || io.buffer_set_blocking_mode(buffer,false)) goto done;
    if((pollfd=io.buffer_get_poll_fd(buffer))<0) goto done;
    problem="map legacy ADI IIO blocks for explicit return";
    iio_result=map_blocks(pollfd,&blocks);
    if(iio_result<0) goto done;
    problem="verify IIO is empty before real8 RUN";
    const char *probe_error=NULL;
    iio_result=read_iio_block(pollfd,&blocks,&history,&probe_error);
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
    fprintf(stderr,"FFM1_READY block=%d history=%d dma_blocks=%d explicit_return=1\n",BLOCK,RETAIN,IIO_BLOCKS);
    struct observation obs={0};
    struct field_live live={0};
    struct field_wire wire={0};
    uint64_t first64=0,end64=0;
    uint64_t last_terminal=0, early_data_sent=0;
    double last_refill=0, refill_gap=0, max_refill_gap=0;
    enum preview_result terminal_status=PREVIEW_READY;
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
#ifdef OBS_SCAN_STREAM_BENCH
            if(dmac.reg && (reg[STATUS/4]&FIELD_SOURCE_FAULT))
                source_failure="real8 hardware reported a source fault";
#endif
            if(!source_failure) {
                got=read_iio_block(pollfd,&blocks,&history,&source_failure);
                iio_result=got;
            }
            if(got>0) {
                double now=monotonic();
                if(last_refill) {
                    refill_gap=now-last_refill;
                    if(refill_gap>max_refill_gap) max_refill_gap=refill_gap;
                }
                last_refill=now;
#ifdef OBS_SCAN_STREAM_BENCH
                if(dmac.reg) dmac_remember(&dmac,reg,&blocks);
#endif
                live=stable_live(reg,history.consumed);
            }
            if(!source_failure && reg[STATUS/4]&FIELD_SOURCE_FAULT)
                source_failure="real8 hardware reported a source fault";
            if(source_failure) {
                /* Stop this generation of real8 before any new data can enter history.
                 * FFM1 still reports the marked visits; this remains its sole reader. */
                quiesce_source(reg,source_failure,iio_result,history.consumed,refill_gap,&blocks);
                rc=4;
            }
        }
        int expired=draining && monotonic()>=deadline;
        while(pending || reg[LEVEL/4]&FFM_HEAD_AVAILABLE) {
            if(!pending) {
                obs=get_head(reg);
                receipt_time=monotonic();
                if(wat.version!=obs.version && load_version(argv[1],obs.version,&wat,watch_bytes)) {
                    wat=(struct watch_table){0};
                    failed=1;
                }
                terminal_status=PREVIEW_READY;
                first64=end64=0;
                if(!obs.outcome || obs.outcome>7 || (obs.outcome!=1 && (obs.first||obs.end)))
                    terminal_status=PREVIEW_INVALID;
                if(obs.outcome==1) {
                    terminal_status=preview_bounds(&history,obs.first,obs.end,&first64,&end64);
                    if(end64-first64>FIELD_MAX) terminal_status=PREVIEW_INVALID;
                }
                write_reg(reg,POP,1);
                pending=1;
            }
            if(send_enabled && egress.fd>=0) {
                int opened=wire.active && same_visit(&wire.visit,&obs) && obs.outcome!=1 ? 0 :
                    stream_begin(&wire,&obs,first64,&wat,&egress);
                if(opened>0 && !expired) break;
                if(opened!=0) {
                    field_abandon(&egress);
                    terminal_status=PREVIEW_LOST;
                } else {
                    if(obs.outcome==1 && terminal_status==PREVIEW_READY) {
                        enum preview_result sent=stream_data(&wire,end64,&history,&egress);
                        if(sent==PREVIEW_WAIT && !expired && (!source_failure || end64<=history.consumed)) break;
                        terminal_status=sent==PREVIEW_WAIT?PREVIEW_LOST:sent;
                    }
                    int closed=stream_end(&wire,&obs,&wat,terminal_status,&egress);
                    if(closed>0 && !expired) break;
                    if(closed!=0) {
                        field_abandon(&egress);
                        terminal_status=PREVIEW_LOST;
                    }
                }
            } else if(send_enabled) terminal_status=PREVIEW_LOST;
            else if(obs.outcome==1 && terminal_status==PREVIEW_READY) {
                if(first64<history.consumed-history.retained) terminal_status=PREVIEW_LOST;
                else if(end64>history.consumed) {
                    if(!expired && !source_failure) break;
                    terminal_status=PREVIEW_LOST;
                }
            }
            size_t extent=terminal_status==PREVIEW_READY && obs.outcome==1?(size_t)(end64-first64):0;
            if(report(&obs,&wat,terminal_status,extent,send_enabled,receipt_time) ||
               terminal_status!=PREVIEW_READY) failed=1;
            last_terminal=obs.epoch;
            pending=0;
        }
        if(!pending && !source_failure && live.phase && live.identity.epoch>last_terminal && egress.fd>=0 &&
                (!wire.active || same_visit(&wire.visit,&live.identity))) {
            uint64_t first,through=live.iio_through;
            bool valid=preview_position(&history,live.identity.first,&first)==0;
            if(valid && live.phase==2) {
                uint64_t end;
                valid=preview_bounds(&history,live.identity.first,live.identity.end,&first,&end)==PREVIEW_READY;
                if(valid && end<through) through=end;
            }
            if(valid && first>=history.consumed-history.retained &&
                (wat.version==live.identity.version || !load_version(argv[1],live.identity.version,&wat,watch_bytes))) {
                int opened=stream_begin(&wire,&live.identity,first,&wat,&egress);
                if(opened<0 || (!opened && first<=through &&
                    stream_data(&wire,through,&history,&egress)>PREVIEW_WAIT)) {
                    field_abandon(&egress);
                    failed=1;
                }
            }
        }
        uint64_t data_complete=egress.complete[FIELD_DATA];
        uint64_t head_tag=egress.count?egress.packets[egress.head].tag:0;
        field_pump(&egress);
        if(data_complete!=egress.complete[FIELD_DATA] && wire.active && head_tag==wire.tag) {
            struct field_live at_send=stable_live(reg,history.consumed);
            if(at_send.phase==1 && same_visit(&at_send.identity,&wire.visit)) ++early_data_sent;
        }
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
            quiesce_source(reg,source_failure,iio_result,history.consumed,refill_gap,&blocks);
            rc=4;
        }
    }
    field_abandon(&egress);
    if(reg[LOST/4]!=lost_start || pending || failed || egress.abandoned || reg[LEVEL/4]&FFM_LEVEL_COUNT) rc=4;
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
    fprintf(stderr,"FSB1_EGRESS begin=%llu data=%llu end=%llu data_before_boundary=%llu max_refill_gap_s=%.9f causes=%02x\n",
        (unsigned long long)egress.complete[FIELD_BEGIN],(unsigned long long)egress.complete[FIELD_DATA],
        (unsigned long long)egress.complete[FIELD_END],(unsigned long long)early_data_sent,max_refill_gap,
        reg[FSB_CAUSES/4]);
#ifdef OBS_SCAN_STREAM_BENCH
    fprintf(stderr,"FSB1_IIO dequeued=%llu returned=%llu max_checkout_s=%.9f\n",
        (unsigned long long)blocks.dequeued,(unsigned long long)blocks.returned,blocks.max_checkout);
#endif
done:
    if(rc && !source_failure) report_error(reg,problem?problem:"receipt, history or drain fault",iio_result);
    if(stream_owned) {write_reg(reg,SESSION,0);write_reg(reg,COMMAND,FIELD_CANCEL);write_reg(reg,RUN,0);}
    for(unsigned i=0;i<IIO_BLOCKS;++i) if(blocks.maps[i]) munmap(blocks.maps[i],BLOCK);
    if(buffer) io.buffer_destroy(buffer);
    if(context) io.context_destroy(context);
    if(stream_owned) {
        write_reg(reg,0x28c,oldtimeout);
        write_reg(reg,0x40,oldcode);write_reg(reg,0x38,oldmode);
        if(!source_failure) write_reg(reg,RUN,oldrun);
    }
    if(reg!=MAP_FAILED) munmap((void*)reg,4096);
#ifdef OBS_SCAN_STREAM_BENCH
    if(dmac.reg) munmap((void*)dmac.reg,DMAC_PAGE);
#endif
    if(mem>=0) close(mem);
    if(guard>=0) close(guard);
    if(owner>=0) close(owner);
    if(lib) dlclose(lib);
    if(egress.fd>=0) close(egress.fd);
    for(unsigned i=0;i<FIELD_QUEUE;++i) free(egress.packets[i].bytes);
    free(watch_bytes);free(storage);
    return rc;
}
