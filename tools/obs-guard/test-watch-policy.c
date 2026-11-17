#include "obs-watch-policy.h"
#include <assert.h>
#include <stdio.h>
static void u32(uint8_t *p,uint32_t x){for(unsigned i=0;i<4;i++)p[i]=(uint8_t)(x>>(8*i));}
static void u64(uint8_t *p,uint64_t x){u32(p,(uint32_t)x);u32(p+4,(uint32_t)(x>>32));}
int main(void){
 uint8_t wire[WATCH_HEADER+86*WATCH_RECORD]={0};
 u32(wire,0x31544157);u32(wire+4,1);u32(wire+8,7501);u32(wire+12,86);u64(wire+16,1);u32(wire+24,1);u32(wire+28,WATCH_RECORD);
 for(unsigned i=0;i<86;i++)u32(wire+WATCH_HEADER+i*WATCH_RECORD+4,i);
 uint8_t *v=wire+WATCH_HEADER+80*WATCH_RECORD;u32(v,1);u64(v+8,5865000000ull);u32(v+16,0x0f700000);u32(v+20,24);u64(v+24,1);
 struct watch_map m={0};assert(!watch_parse(&m,wire,sizeof(wire)));unsigned a=0;
 for(unsigned lap=0;lap<1000;lap++){
  assert(watch_next(&m,79,&a)==80&&a==87); /* reserve two-visit admission margin */
  assert(watch_next(&m,0,&a)==80&&a==80);
  assert(watch_next(&m,85,&a)==80&&a==81);
  assert(m.target[80].enabled&&m.enabled==1); /* captures never consume intent */
 }
 struct watch_target old=m.target[80];assert(watch_same(&m,80,&old));
 u64(wire+16,2);u32(wire+24,0);u32(v,0);assert(!watch_parse(&m,wire,sizeof(wire)));
 assert(!watch_same(&m,80,&old));assert(watch_next(&m,0,&a)<0);
 struct watch_map saved=m;u32(v,2);assert(watch_parse(&m,wire,sizeof(wire))<0);assert(!memcmp(&m,&saved,sizeof(m)));
 u32(v,0);assert(watch_parse(&m,wire,sizeof(wire)-1)<0);u32(wire+12,129);assert(watch_parse(&m,wire,sizeof(wire))<0);
 struct watch_map fresh=saved;fresh.revision++;fresh.target[80]=old;fresh.enabled=1;
 assert(watch_update(&saved,&fresh)<0); /* re-enable requires a new generation */
 fresh.target[80].generation++;assert(!watch_update(&saved,&fresh));
 assert(!watch_same(&saved,80,&old));assert(watch_update(&saved,&m)<0); /* revision rollback */
 puts("persistent watch: repeat visits, wrap, explicit removal, atomic invalid-update rejection PASS");return 0;
}
