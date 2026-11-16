#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/file.h>
#include <sys/ioctl.h>
#include <string.h>
#include <time.h>
#include <errno.h>
static unsigned readnum(const char *p){FILE*f=fopen(p,"r");unsigned n=0;if(!f||fscanf(f,"%i",&n)!=1)exit(2);fclose(f);return n;}
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
int main(int argc,char**argv){
 int guarded=argc==3&&!strcmp(argv[2],"--guarded-scan");
 if(argc!=2&&!guarded)return 2;
 int lock=open("/run/obs-recording.lock",O_CREAT|O_RDWR,0600);if(lock<0||flock(lock,LOCK_EX|LOCK_NB))return 2;
 unsigned base=readnum("/sys/devices/soc0/maia-sdr-recording/recording_base_address"),size=readnum("/sys/devices/soc0/maia-sdr-recording/recording_size");
 if(base!=0x06000000||size!=0x08000000)return 3;
 if(guarded){int fd=open("/dev/obs_fl_guard",O_RDONLY);uint32_t s[128]={0};if(fd<0||ioctl(fd,0x4605,s)||s[0]!=2||s[1]!=2||s[2]||s[112]!=1)return 3;close(fd);}
 else if(readnum("/sys/bus/iio/devices/iio:device0/out_altvoltage1_TX_LO_powerdown")!=1||readnum("/sys/bus/iio/devices/iio:device0/in_voltage_sampling_frequency")!=61440000)return 3;
 int fd=open("/dev/mem",O_RDWR|O_SYNC);volatile uint32_t*r=mmap(NULL,4096,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0x7c460000);if(r==MAP_FAILED)return 4;
 if(r[0x50/4]!=0x324e424f||r[0x224/4]!=0x31444c46||r[0x1e0/4]!=(guarded?3u:0u)||r[0x20/4]&1)return 5;
 /* Guarded mode uses the pre-take Fs readback in the series receipt. PHY
    callbacks are blocked for the entire lease; do not call SPI from here.
    No sample-zero timestamp is invented: this recorder has no such latch. */
 (void)r[0xc/4];double start=now();r[0x10/4]=1;__sync_synchronize();usleep(200000);r[0x10/4]=2;__sync_synchronize();
 unsigned irq=0;while(now()-start<2){irq=r[0xc/4];if(irq&2)break;usleep(100);}
 if(!(irq&2)){fprintf(stderr,"recorder FINISHED timeout; allocation remains reserved\n");return 6;}
 unsigned end=r[0x14/4],status=r[0x10/4];double dt=now()-start;
 if(end<=base||end>base+size||status&16){fprintf(stderr,"bad recorder end/status %x %x\n",end,status);return 7;}
 size_t n=end-base,len=(n+4095)&~4095u;int rec=open("/dev/maia-sdr-recording",O_RDONLY);void*p=mmap(NULL,len,PROT_READ,MAP_SHARED,rec,0);if(p==MAP_FAILED){perror("recording mmap");return 8;}
 int out=open(argv[1],O_WRONLY|O_CREAT|O_EXCL,0600);if(out<0)return 9;size_t written=0;while(written<n){ssize_t k=write(out,(char*)p+written,n-written);if(k<=0)return 10;written+=k;}close(out);munmap(p,len);close(rec);
 printf("{\"base\":%u,\"reserve_bytes\":%u,\"end\":%u,\"bytes\":%zu,\"samples\":%zu,\"seconds_until_finished\":%.9f,\"recorder_status\":%u,\"recorder_dropped_samples\":false,\"Fs\":61440000,\"format\":\"ci16_le raw ADC, independent stock reserved recorder\"}\n",base,size,end,n,n/4,dt,status);
 munmap((void*)r,4096);close(fd);close(lock);return 0;
}
