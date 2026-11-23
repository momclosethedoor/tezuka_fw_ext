#ifndef OBS_FIELD_CAPTURE_H
#define OBS_FIELD_CAPTURE_H

enum { FIELD_CAPTURE_AHEAD=4, FIELD_CAPTURE_BYTES=RETAIN+FIELD_CAPTURE_AHEAD*BLOCK,
       FIELD_CAPTURE_WAIT_MS=20, FIELD_CAPTURE_NICE=-10 };

struct capture_extent {
    uint64_t end;
    struct field_live live;
};

struct field_capture {
    unsigned produced, peak, rejected;
    int fd, wake, nice;
    pthread_t thread;
    bool started;
    atomic_bool armed, stop;
    atomic_uint head, tail;
    _Atomic(const char *) failure;
    volatile uint32_t *reg;
    struct iio_blocks *blocks;
    struct preview_history producer;
    struct capture_extent ready[FIELD_CAPTURE_AHEAD];
    double max_gap, cpu;
};

static void capture_wake(int fd)
{
    const uint64_t one=1;
    ssize_t result;
    do { result=write(fd,&one,sizeof one); } while(result<0 && errno==EINTR);
}

static void *capture_loop(void *argument)
{
    struct field_capture *c=argument;
    const char *failure=NULL;
    ssize_t result=-EAGAIN;
    double start_cpu=thread_cpu(), last_return=0, gap=0;
    if(c->nice && setpriority(PRIO_PROCESS,0,c->nice)) failure="FIELD capture priority refused";
    while(!atomic_load_explicit(&c->armed,memory_order_acquire) &&
          !atomic_load_explicit(&c->stop,memory_order_relaxed)) usleep(1000);
    while(!failure && !atomic_load_explicit(&c->stop,memory_order_relaxed)) {
        if(c->reg[STATUS/4]&FIELD_SOURCE_FAULT) {
            failure="real8 hardware reported a source fault";
            break;
        }
        struct pollfd p={.fd=c->fd,.events=POLLIN};
        int ready=poll(&p,1,FIELD_CAPTURE_WAIT_MS);
        if(ready<0 && errno!=EINTR) failure="IIO capture poll failed";
        if(p.revents&(POLLERR|POLLHUP|POLLNVAL)) failure="IIO capture poll reported a source fault";
        if(failure || !(p.revents&POLLIN)) continue;
        unsigned pending=c->produced-atomic_load_explicit(&c->tail,memory_order_acquire);
        if(pending==FIELD_CAPTURE_AHEAD) {
            ++c->rejected;
            failure="FIELD capture handoff exhausted";
            break;
        }
        result=read_iio_block(c->fd,c->blocks,&c->producer,&failure);
        if(result<=0) continue;
        double now=monotonic();
        if(last_return) {
            gap=now-last_return;
            if(gap>c->max_gap) c->max_gap=gap;
        }
        last_return=now;
        if(dmac.reg) dmac_remember(&dmac,c->reg,c->blocks);
        if(c->reg[STATUS/4]&FIELD_SOURCE_FAULT) {
            failure="real8 hardware reported a source fault";
            break;
        }
        struct capture_extent *next=&c->ready[c->produced%FIELD_CAPTURE_AHEAD];
        next->end=c->producer.consumed;
        next->live=stable_live(c->reg,next->end);
        atomic_store_explicit(&c->head,++c->produced,memory_order_release);
        if(pending+1>c->peak) c->peak=pending+1;
        capture_wake(c->wake);
    }
    if(failure) {
        if(atomic_load_explicit(&c->armed,memory_order_acquire))
            quiesce_source(c->reg,failure,result,c->producer.consumed,gap,c->blocks);
        atomic_store_explicit(&c->failure,failure,memory_order_release);
        capture_wake(c->wake);
    }
    double end_cpu=thread_cpu();
    c->cpu=start_cpu>=0 && end_cpu>=0 ? end_cpu-start_cpu : -1;
    return NULL;
}

static int capture_start(struct field_capture *c, int fd, volatile uint32_t *reg,
                         struct iio_blocks *blocks, const struct preview_history *history,
                         const char *mode)
{
    if(!strcmp(mode,"priority")) c->nice=FIELD_CAPTURE_NICE;
    else if(strcmp(mode,"normal")) return -1;
    c->fd=fd;
    c->reg=reg;
    c->blocks=blocks;
    c->producer=*history;
    c->wake=eventfd(0,EFD_NONBLOCK|EFD_CLOEXEC);
    if(c->wake<0) return -1;
    if(pthread_create(&c->thread,NULL,capture_loop,c)) {
        close(c->wake);
        c->wake=-1;
        return -1;
    }
    c->started=true;
    return 0;
}

static ssize_t capture_take(struct field_capture *c, struct preview_history *history,
                            struct field_live *live)
{
    uint64_t wake_count;
    ssize_t result=read(c->wake,&wake_count,sizeof wake_count);
    if(result<0 && errno!=EAGAIN && errno!=EINTR) return result;
    unsigned tail=atomic_load_explicit(&c->tail,memory_order_relaxed);
    if(tail==atomic_load_explicit(&c->head,memory_order_acquire)) return 0;
    struct capture_extent extent=c->ready[tail%FIELD_CAPTURE_AHEAD];
    size_t bytes=(size_t)(extent.end-history->consumed);
    history->consumed=extent.end;
    history->retained+=bytes;
    if(history->retained>RETAIN) history->retained=RETAIN;
    *live=extent.live;
    /* The writer can now advance by one more block. The consumer's last
     * RETAIN bytes still fit behind the four unpublished-to-consumer blocks. */
    atomic_store_explicit(&c->tail,tail+1,memory_order_release);
    return (ssize_t)bytes;
}

static void capture_stop(struct field_capture *c)
{
    if(!c->started) return;
    atomic_store_explicit(&c->stop,true,memory_order_relaxed);
    pthread_join(c->thread,NULL);
    close(c->wake);
    c->wake=-1;
    c->started=false;
}

#endif
