#include <assert.h>
#include <stdio.h>
#include "obs-transport-queue.h"
int main(void){
 struct transport_queue q={0};uint8_t a[8];memset(a,11,8);assert(!tq_reset(&q,2,8));
 assert(!tq_push(&q,a,8,0));memset(a,99,8);int active=tq_start(&q);assert(active>=0&&q.memory[active*8]==11);
 memset(a,22,8);assert(!tq_push(&q,a,8,8));memset(a,33,8);assert(!tq_push(&q,a,8,16));
 assert(q.dropped_blocks==1&&q.dropped_bytes==8&&q.count==2);assert(q.memory[active*8]==11);
 assert(!tq_finish(&q));active=tq_start(&q);assert(q.entry[active].offset==16&&q.memory[active*8]==33);assert(!tq_finish(&q));
 uint8_t *saved=q.memory;assert(!tq_reset(&q,1,8));assert(q.memory==saved);assert(!tq_push(&q,a,8,40));active=tq_start(&q);
 assert(tq_push(&q,a,8,48)==1&&q.dropped_blocks==0&&q.memory[active*8]==33);assert(!tq_finish(&q));
 assert(tq_finish(&q)==-1&&tq_push(&q,a,9,0)==-1);assert(tq_reset(&q,9,8)==-1);
 for(unsigned depth=2;depth<=8;depth*=2){assert(!tq_reset(&q,depth,8));for(unsigned i=0;i<100;i++){memset(a,i,8);assert(!tq_push(&q,a,8,i*8));assert(q.count<=depth);}while(q.count){active=tq_start(&q);assert(q.memory[active*q.stride]==q.entry[active].offset/8);assert(!tq_finish(&q));}}
 free(q.memory);puts("bounded transport: ownership, copy isolation, drop-oldest, offsets, wrap and reuse PASS");return 0;
}
