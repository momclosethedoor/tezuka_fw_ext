#ifndef OBS_FIELD_EGRESS_H
#define OBS_FIELD_EGRESS_H

#include <arpa/inet.h>
#include <fcntl.h>
#include <linux/sockios.h>
#include <sys/socket.h>
#include <sys/time.h>

enum { FIELD_QUEUE = 4, FIELD_META = 512, FIELD_SEND_SLICE = 64*1024, FIELD_SEND_DEADLINE_S = 1 };
enum field_message { FIELD_BEGIN, FIELD_DATA, FIELD_END };

struct field_packet {
    uint8_t *bytes;
    size_t length, used;
    enum field_message kind;
    uint64_t tag;
    double ready, first_send;
};

struct field_egress {
    struct field_packet packets[FIELD_QUEUE];
    unsigned head, count, peak;
    size_t bytes, peak_bytes;
    int fd, socket_peak;
    uint64_t sent, rejected, abandoned, partial, accepted_bytes, sent_bytes, blocked;
    uint64_t complete[3];
    double max_send_s, max_queue_s, max_ready_s;
};

static int field_connect(struct field_egress *q, const char *host, const char *port, size_t max_payload)
{
    char *end;
    unsigned long number = strtoul(port, &end, 10);
    struct sockaddr_in addr = {.sin_family=AF_INET, .sin_port=htons(number)};
    struct timeval timeout = {.tv_sec=FIELD_SEND_DEADLINE_S};
    if (*end || !number || number > 65535 || inet_pton(AF_INET, host, &addr.sin_addr) != 1)
        return -1;
    for (unsigned i=0; i<FIELD_QUEUE; ++i) {
        q->packets[i].bytes=malloc(12+FIELD_META+max_payload);
        if (!q->packets[i].bytes) return -1;
    }
    q->fd=socket(AF_INET,SOCK_STREAM|SOCK_CLOEXEC,0);
    if (q->fd<0 || setsockopt(q->fd,SOL_SOCKET,SO_SNDTIMEO,&timeout,sizeof timeout) ||
        connect(q->fd,(struct sockaddr *)&addr,sizeof addr) || fcntl(q->fd,F_SETFL,O_NONBLOCK))
        return -1;
    return 0;
}

static int field_queue(struct field_egress *q, enum field_message kind, uint64_t tag, const char *meta,
    const uint8_t *payload, size_t length, double ready)
{
    size_t meta_len=strlen(meta);
    if (q->fd<0 || q->count==FIELD_QUEUE || meta_len>=FIELD_META) {
        ++q->rejected;
        return -1;
    }
    struct field_packet *p=&q->packets[(q->head+q->count)%FIELD_QUEUE];
    uint32_t header[3]={0x32503852, (uint32_t)meta_len, (uint32_t)length};
    memcpy(p->bytes,header,sizeof header);
    memcpy(p->bytes+sizeof header,meta,meta_len);
    if (length) memcpy(p->bytes+sizeof header+meta_len,payload,length);
    p->length=sizeof header+meta_len+length;
    p->used=0;
    p->kind=kind;
    p->tag=tag;
    p->ready=ready;
    p->first_send=0;
    ++q->count;
    q->bytes+=p->length;
    if (q->count>q->peak) q->peak=q->count;
    if (q->bytes>q->peak_bytes) q->peak_bytes=q->bytes;
    return 0;
}

static void field_abandon(struct field_egress *q)
{
    if (q->fd>=0) close(q->fd);
    q->fd=-1;
    if (q->count && q->packets[q->head].used) ++q->partial;
    q->abandoned+=q->count;
    q->count=0;
    q->bytes=0;
}

static void field_pump(struct field_egress *q)
{
    if (!q->count) return;
    struct field_packet *p=&q->packets[q->head];
    double now=monotonic();
    if (now-p->ready>FIELD_SEND_DEADLINE_S) { field_abandon(q);return; }
    if (!p->first_send) p->first_send=now;
    size_t left=p->length-p->used;
    ssize_t n=send(q->fd,p->bytes+p->used,left>FIELD_SEND_SLICE ? FIELD_SEND_SLICE : left,MSG_NOSIGNAL);
    if (n<0 && (errno==EAGAIN || errno==EWOULDBLOCK || errno==EINTR)) { ++q->blocked;return; }
    if (n<=0) { field_abandon(q);return; }
    q->accepted_bytes+=(size_t)n;
    p->used+=(size_t)n;
    q->bytes-=(size_t)n;
    int queued;
    if (!ioctl(q->fd,SIOCOUTQ,&queued) && queued>q->socket_peak) q->socket_peak=queued;
    if (p->used!=p->length) return;
    double complete=monotonic();
    double wait=p->first_send-p->ready, sending=complete-p->first_send, total=complete-p->ready;
    if (wait>q->max_queue_s) q->max_queue_s=wait;
    if (sending>q->max_send_s) q->max_send_s=sending;
    if (total>q->max_ready_s) q->max_ready_s=total;
    ++q->sent;
    ++q->complete[p->kind];
    q->sent_bytes+=p->length;
    q->head=(q->head+1)%FIELD_QUEUE;
    --q->count;
}

#endif
