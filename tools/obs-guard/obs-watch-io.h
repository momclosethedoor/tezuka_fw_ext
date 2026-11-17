#ifndef OBS_WATCH_IO_H
#define OBS_WATCH_IO_H
#include "obs-watch-policy.h"
#include <sys/inotify.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdio.h>
struct watch_input {struct watch_map map;int fd;char path[512],name[256];};
static inline int watch_load(struct watch_input *w){
 uint8_t wire[WATCH_MAX_BYTES+1];size_t used=0;
 int fd=open(w->path,O_RDONLY|O_NOFOLLOW|O_NONBLOCK);if(fd<0)return -1;
 struct stat st;if(fstat(fd,&st)||!S_ISREG(st.st_mode)||st.st_size>(off_t)WATCH_MAX_BYTES){close(fd);return -1;}
 for(;;){ssize_t n=read(fd,wire+used,sizeof(wire)-used);if(n<0&&errno==EINTR)continue;if(n<0){close(fd);return -1;}if(!n)break;used+=(size_t)n;if(used==sizeof(wire)){close(fd);return -1;}}
 close(fd);struct watch_map next;
 if(watch_parse(&next,wire,used)||watch_update(&w->map,&next))return -1;
 return 0;
}
static inline int watch_open(struct watch_input *w,const char *path){
 memset(w,0,sizeof(*w));w->fd=-1;
 if(strlen(path)>=sizeof(w->path))return -1;
 strcpy(w->path,path);char directory[512];strcpy(directory,path);char *slash=strrchr(directory,'/');
 if(!slash||!slash[1]||strlen(slash+1)>=sizeof(w->name))return -1;
 strcpy(w->name,slash+1);if(slash==directory)slash[1]=0;else *slash=0;
 w->fd=inotify_init1(IN_CLOEXEC|IN_NONBLOCK);if(w->fd<0)return -1;
 if(inotify_add_watch(w->fd,directory,IN_MOVED_TO|IN_CLOSE_WRITE|IN_DELETE|IN_DELETE_SELF|IN_MOVE_SELF)<0||watch_load(w)){
  close(w->fd);w->fd=-1;return -1;
 }
 return 0;
}
/* No file read on the normal block path unless a desired-state event arrives. */
static inline int watch_refresh(struct watch_input *w){
 union {struct inotify_event align;char bytes[4096];} buffer;int changed=0;
 for(;;){
  ssize_t n=read(w->fd,buffer.bytes,sizeof(buffer.bytes));
  if(n<0&&errno==EINTR)continue;
  if(n<0&&(errno==EAGAIN||errno==EWOULDBLOCK))break;
  if(n<=0)return -1;
  for(size_t off=0;off<(size_t)n;){
   struct inotify_event *e=(void*)(buffer.bytes+off);
   if(e->mask&(IN_Q_OVERFLOW|IN_DELETE_SELF|IN_MOVE_SELF|IN_IGNORED))return -1;
   if(e->len&&!strcmp(e->name,w->name))changed=1;
   off+=sizeof(*e)+e->len;
  }
 }
 return changed?watch_load(w):0;
}
#endif
