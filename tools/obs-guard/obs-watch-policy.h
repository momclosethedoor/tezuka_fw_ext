#ifndef OBS_WATCH_POLICY_H
#define OBS_WATCH_POLICY_H
#include <stdint.h>
#include <stddef.h>
#include <string.h>
#define WATCH_MAX 128u
#define WATCH_RESIDENT_MAX 65535u
#define WATCH_HEADER 32u
#define WATCH_RECORD 32u
#define WATCH_MAX_BYTES (WATCH_HEADER+WATCH_MAX*WATCH_RECORD)
struct watch_target {uint64_t hz,generation;uint32_t enabled,nco,code;};
struct watch_map {uint64_t revision;uint32_t version,count,enabled;struct watch_target target[WATCH_MAX];};
static inline uint32_t watch_u32(const uint8_t *p){return (uint32_t)p[0]|((uint32_t)p[1]<<8)|((uint32_t)p[2]<<16)|((uint32_t)p[3]<<24);}
static inline uint64_t watch_u64(const uint8_t *p){return watch_u32(p)|((uint64_t)watch_u32(p+4)<<32);}
struct watch_table {const uint8_t *rows;uint64_t revision;uint32_t version,count,enabled;};
static inline struct watch_target watch_row(const struct watch_table *t,uint32_t i){
 const uint8_t *p=t->rows+WATCH_RECORD*i;
 return (struct watch_target){.enabled=watch_u32(p),.hz=watch_u64(p+8),
  .nco=watch_u32(p+16),.code=watch_u32(p+20),.generation=watch_u64(p+24)};
}
static inline int watch_table_read(struct watch_table *out,const uint8_t *p,size_t n,uint32_t max,uint64_t max_rf){
 if(n<WATCH_HEADER||watch_u32(p)!=0x31544157u||watch_u32(p+4)!=1)return -1;
 struct watch_table table={.rows=p+WATCH_HEADER,.version=watch_u32(p+8),
  .count=watch_u32(p+12),.revision=watch_u64(p+16),.enabled=watch_u32(p+24)};
 if(!table.version||!table.revision||!table.count||table.count>max||watch_u32(p+28)!=WATCH_RECORD
    ||n!=WATCH_HEADER+table.count*WATCH_RECORD)return -1;
 uint32_t enabled=0;
 for(uint32_t i=0;i<table.count;++i){
  struct watch_target row=watch_row(&table,i);
  if(row.enabled>1||watch_u32(table.rows+i*WATCH_RECORD+4)!=i)return -1;
  if(row.enabled&&(!row.generation||row.hz<70000000ull||row.hz>max_rf||row.nco>=0x10000000u||row.code!=24))return -1;
  enabled+=row.enabled;
 }
 if(enabled!=table.enabled)return -1;
 *out=table;return 0;
}
/* A map replaces desired state atomically. No attempt outcome mutates it. */
static inline int watch_parse(struct watch_map *out,const uint8_t *p,size_t n){
 struct watch_table table;
 if(watch_table_read(&table,p,n,WATCH_MAX,6000000000ull))return -1;
 struct watch_map m={.revision=table.revision,.version=table.version,.count=table.count,.enabled=table.enabled};
 for(unsigned i=0;i<m.count;++i)m.target[i]=watch_row(&table,i);
 *out=m;return 0;
}
/* Choose a future eligible visit; intent survives SUCCESS/TIMEOUT/busy.
 * The exact epoch of this particular attempt is latched only after IIO is ready. */
static inline int watch_next(const struct watch_map *m,unsigned current,unsigned *advance){
 if(current>=m->count||!m->enabled)return -1;
 for(unsigned a=2;a<m->count+2;a++){
  unsigned p=(current+a)%m->count;
  if(m->target[p].enabled){*advance=a;return (int)p;}
 }
 return -1;
}
static inline int watch_same(const struct watch_map *m,unsigned position,const struct watch_target *old){
 if(position>=m->count)return 0;
 const struct watch_target *t=&m->target[position];
 return t->enabled&&t->generation==old->generation&&t->hz==old->hz&&t->nco==old->nco&&t->code==old->code;
}
/* Revisions must advance. A removed/re-added target gets a new generation,
 * including when its intermediate disabled table was coalesced by inotify. */
static inline int watch_update(struct watch_map *old,const struct watch_map *next){
 if(old->revision){
  if(next->revision<old->revision)return -1;
  if(next->revision==old->revision)return memcmp(old,next,sizeof(*old))?-1:0;
  if(next->version==old->version){
   if(next->count!=old->count)return -1;
   for(unsigned i=0;i<old->count;i++){
    const struct watch_target *a=&old->target[i],*b=&next->target[i];
    if(b->generation<a->generation)return -1;
    if(b->enabled&&(!a->enabled||b->hz!=a->hz||b->nco!=a->nco||b->code!=a->code)&&b->generation<=a->generation)return -1;
   }
  }
 }
 *old=*next;return 0;
}
#endif
