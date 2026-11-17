#ifndef OBS_TRANSPORT_QUEUE_H
#define OBS_TRANSPORT_QUEUE_H
#include <stdint.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
struct tq_entry {size_t length,offset;int used;};
struct transport_queue {uint8_t *memory;size_t stride,allocated;unsigned capacity,count,high,order[8];int active;
 struct tq_entry entry[8];uint64_t dropped_bytes,dropped_blocks;};
static inline int tq_reset(struct transport_queue *q,unsigned capacity,size_t stride){
 if(capacity>8||!stride||stride>262144)return -1;
 size_t bytes=capacity*stride;
 if(bytes>q->allocated){void *p=realloc(q->memory,bytes);if(!p)return -1;q->memory=p;q->allocated=bytes;}
 q->stride=stride;q->capacity=capacity;q->count=0;q->high=0;q->active=-1;q->dropped_bytes=q->dropped_blocks=0;
 memset(q->entry,0,sizeof(q->entry));return 0;
}
static inline void tq_remove(struct transport_queue *q,unsigned position){
 q->entry[q->order[position]].used=0;
 memmove(q->order+position,q->order+position+1,(q->count-position-1)*sizeof(q->order[0]));q->count--;
}
static inline int tq_push(struct transport_queue *q,const void *data,size_t n,size_t offset){
 if(!q->capacity||!n||n>q->stride)return -1;
 if(q->count==q->capacity){
  unsigned victim=q->active>=0?1:0;if(victim>=q->count)return 1;
  q->dropped_bytes+=q->entry[q->order[victim]].length;q->dropped_blocks++;tq_remove(q,victim);
 }
 unsigned slot=0;while(slot<q->capacity&&q->entry[slot].used)slot++;if(slot==q->capacity)return -1;
 memcpy(q->memory+slot*q->stride,data,n);q->entry[slot]=(struct tq_entry){n,offset,1};q->order[q->count++]=slot;
 if(q->count>q->high){q->high=q->count;}
 return 0;
}
static inline int tq_start(struct transport_queue *q){if(q->active>=0||!q->count)return -1;q->active=(int)q->order[0];return q->active;}
static inline int tq_finish(struct transport_queue *q){if(q->active<0||!q->count||q->order[0]!=(unsigned)q->active)return -1;tq_remove(q,0);q->active=-1;return 0;}
#endif
