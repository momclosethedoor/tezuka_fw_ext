#ifndef OBS_FSB1_STREAM_H
#define OBS_FSB1_STREAM_H

enum {
    FSB_ID=0x388, FSB_LIVE=0x38c, FSB_DIAG=0x390, FSB_CAUSES=0x394,
    FSB_CAUSE_MASK=7, FSB_DIAG_READY=1u<<5, FSB_MAX_READS=3,
    FIELD_EPOCH_LO=0x29c, FIELD_EPOCH_HI=0x2a0, FIELD_FIRST=0x2a8, FIELD_END_BYTE=0x2ac,
    MAP_VERSION=0x22c, MAP_POSITION=0x234, REAL8_BYTES_PER_S=15360000
};

struct field_live {
    struct observation identity;
    uint32_t phase;
    uint64_t iio_through;
};

static bool same_visit(const struct observation *a, const struct observation *b)
{
    return a->epoch==b->epoch && a->version==b->version && a->position==b->position;
}

static struct field_live sample_live(volatile uint32_t *reg, uint64_t consumed)
{
    struct field_live s = {.phase=reg[FSB_LIVE/4],.iio_through=consumed};
    if(s.phase==1 || s.phase==2) {
        uint64_t hi=reg[FIELD_EPOCH_HI/4];
        s.identity.epoch=(hi<<32)|reg[FIELD_EPOCH_LO/4];
        s.identity.version=reg[MAP_VERSION/4];
        s.identity.position=reg[MAP_POSITION/4];
        s.identity.first=reg[FIELD_FIRST/4];
        if(s.phase==2) s.identity.end=reg[FIELD_END_BYTE/4];
    } else s.phase=0;
    __sync_synchronize();
    return s;
}

static struct field_live stable_live(volatile uint32_t *reg, uint64_t consumed)
{
    struct field_live a=sample_live(reg,consumed);
    for(unsigned n=0;n<FSB_MAX_READS;++n) {
        struct field_live b=sample_live(reg,consumed);
        if(a.phase==b.phase && same_visit(&a.identity,&b.identity)
            && a.identity.first==b.identity.first && a.identity.end==b.identity.end)
            return b;
        a=b;
    }
    return (struct field_live){0};
}

struct field_wire {
    struct observation visit;
    uint64_t first, next, tag;
    bool active;
};

static int stream_begin(struct field_wire *w, const struct observation *o, uint64_t first,
                        const struct watch_table *wat, struct field_egress *q)
{
    if(w->active) return same_visit(&w->visit,o) && w->first==first ? 0 : -1;
    if(q->count==FIELD_QUEUE) return 1;
    if(wat->version!=o->version || o->position>=wat->count) return -1;
    struct watch_target t=watch_row(wat,o->position);
    if(!t.enabled) return -1;
    char meta[FIELD_META];
    int n=snprintf(meta,sizeof meta,
        "{\"kind\":\"begin\",\"stream_tag\":%llu,\"epoch\":%llu,\"map_version\":%u,"
        "\"target_position\":%u,\"target_hz\":%llu,\"watch_revision\":%llu,\"watch_generation\":%llu,"
        "\"source_first\":%u,\"code\":24,\"g\":0,\"fs\":%u,\"block_bytes\":%u,"
        "\"capture_seconds\":1,\"capture_limit_bytes\":%u,\"unqualified_raw\":false}",
        (unsigned long long)(w->tag+1),(unsigned long long)o->epoch,o->version,o->position,
        (unsigned long long)t.hz,(unsigned long long)wat->revision,
        (unsigned long long)t.generation,o->first,REAL8_BYTES_PER_S,BLOCK,REAL8_BYTES_PER_S+BLOCK);
    if(n<0 || (size_t)n>=sizeof meta ||
       field_queue(q,FIELD_BEGIN,w->tag+1,meta,NULL,0,monotonic())) return -1;
    w->tag++;
    w->visit=*o;
    w->first=w->next=first;
    w->active=true;
    return 0;
}

static enum preview_result stream_data(struct field_wire *w, uint64_t through,
    const struct preview_history *history, struct field_egress *q)
{
    if(through<w->next) return PREVIEW_INVALID;
    if(w->next<history->consumed-history->retained) return PREVIEW_LOST;
    while(w->next<through && w->next<history->consumed && q->count<FIELD_QUEUE) {
        uint64_t count=through-w->next;
        size_t index=(size_t)(w->next%history->capacity);
        if(count>history->consumed-w->next) count=history->consumed-w->next;
        if(count>history->capacity-index) count=history->capacity-index;
        if(count>BLOCK) count=BLOCK;
        char meta[FIELD_META];
        int n=snprintf(meta,sizeof meta,"{\"kind\":\"data\",\"stream_tag\":%llu,\"offset\":%llu}",
            (unsigned long long)w->tag,(unsigned long long)(w->next-w->first));
        if(n<0 || (size_t)n>=sizeof meta ||
           field_queue(q,FIELD_DATA,w->tag,meta,history->bytes+index,(size_t)count,monotonic()))
            return PREVIEW_LOST;
        w->next+=count;
    }
    return w->next==through ? PREVIEW_READY : PREVIEW_WAIT;
}

static int stream_end(struct field_wire *w, const struct observation *o, const struct watch_table *wat,
                      enum preview_result payload_status, struct field_egress *q)
{
    if(!w->active || !same_visit(&w->visit,o) || wat->version!=o->version || o->position>=wat->count)
        return -1;
    if(q->count==FIELD_QUEUE) return 1;
    struct watch_target t=watch_row(wat,o->position);
    uint64_t bytes=w->next-w->first;
    bool success=o->outcome==1 && payload_status==PREVIEW_READY;
    char meta[FIELD_META];
    int n=snprintf(meta,sizeof meta,
        "{\"kind\":\"end\",\"stream_tag\":%llu,\"epoch\":%llu,\"map_version\":%u,"
        "\"target_position\":%u,\"target_hz\":%llu,\"watch_revision\":%llu,\"watch_generation\":%llu,"
        "\"source_first\":%u,\"source_end\":%u,\"started\":%u,\"pl_outcome\":%u,"
        "\"first\":0,\"end\":%u,\"generation_bytes\":%llu,\"error\":%u,\"qualified_field\":%s}",
        (unsigned long long)w->tag,(unsigned long long)o->epoch,o->version,o->position,
        (unsigned long long)t.hz,(unsigned long long)wat->revision,(unsigned long long)t.generation,
        o->first,o->end,o->started,o->outcome,success?o->end-o->first:0,
        (unsigned long long)bytes,(unsigned)payload_status,success?"true":"false");
    if(n<0 || (size_t)n>=sizeof meta ||
       field_queue(q,FIELD_END,w->tag,meta,NULL,0,monotonic())) return -1;
    w->active=false;
    return 0;
}
#endif
