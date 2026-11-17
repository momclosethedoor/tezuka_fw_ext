#define _GNU_SOURCE
#include "obs-watch-io.h"
#include <assert.h>
#include <stdlib.h>
#include <poll.h>
static void u32(uint8_t*p,uint32_t v){for(int i=0;i<4;i++)p[i]=v>>(8*i);}
static void u64(uint8_t*p,uint64_t v){u32(p,v);u32(p+4,v>>32);}
static void replace(const char *path,const void *p,size_t n){char tmp[600];snprintf(tmp,sizeof(tmp),"%s.tmp",path);FILE*f=fopen(tmp,"wb");assert(f);assert(fwrite(p,1,n,f)==n);assert(!fclose(f));assert(!rename(tmp,path));}
int main(void){
 char dir[]="/tmp/obs-watch-test-XXXXXX";assert(mkdtemp(dir));char path[512];snprintf(path,sizeof(path),"%s/targets.bin",dir);
 uint8_t b[WATCH_HEADER+2*WATCH_RECORD]={0};u32(b,0x31544157);u32(b+4,1);u32(b+8,7501);u32(b+12,2);u64(b+16,1);u32(b+24,1);u32(b+28,32);
 uint8_t *t=b+32;u32(t,1);u64(t+8,5865000000ull);u32(t+20,24);u64(t+24,1);u32(t+32+4,1);
 replace(path,b,sizeof(b));struct watch_input w;assert(!watch_open(&w,path));struct watch_target old=w.map.target[0];
 assert(!watch_refresh(&w));assert(watch_same(&w.map,0,&old));
 u64(b+16,2);u32(b+24,0);u32(t,0);replace(path,b,sizeof(b));
 struct pollfd f={w.fd,POLLIN,0};assert(poll(&f,1,1000)==1);assert(!watch_refresh(&w));assert(!watch_same(&w.map,0,&old));
 u64(b+16,3);u32(b+24,1);u32(t,1);u64(t+24,2);replace(path,b,sizeof(b));assert(!watch_refresh(&w));assert(!watch_same(&w.map,0,&old));
 struct watch_map saved=w.map;u64(b+16,2);replace(path,b,sizeof(b));assert(watch_refresh(&w)<0);assert(!memcmp(&saved,&w.map,sizeof(saved)));
 close(w.fd);assert(!unlink(path));assert(!rmdir(dir));puts("watch I/O: atomic replace, event wake, remove/re-add generations, rollback rejection PASS");return 0;
}
