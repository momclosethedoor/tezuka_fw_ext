#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/ioctl.h>
int main(int argc,char **argv){int fd,cmd,rc;uint32_t status[128]={0};unsigned char blob[12488];
 if(argc<2)return 2;fd=open("/dev/obs_fl_guard",O_RDWR);if(fd<0){perror("guard open");return 1;}
 if(!strcmp(argv[1],"load")){FILE *f;if(argc!=3)return 2;f=fopen(argv[2],"rb");if(!f||fread(blob,1,sizeof(blob),f)!=sizeof(blob)||fgetc(f)!=EOF)return 2;fclose(f);rc=ioctl(fd,0x4601,blob);}
 else{cmd=!strcmp(argv[1],"take")?0x4602:!strcmp(argv[1],"next")?0x4603:!strcmp(argv[1],"return")?0x4604:!strcmp(argv[1],"status")?0x4605:0;if(!cmd)return 2;rc=ioctl(fd,cmd,status);}
 if(rc){perror(argv[1]);return 1;}if(ioctl(fd,0x4605,status)){perror("status");return 1;}
 printf("{\"abi\":%u,\"state\":%u,\"error\":%d,\"denied\":%u,\"catalog_dma\":%u,\"map_dma\":%u,\"next_dma\":%u,\"length_each\":%u,\"obn_status\":%u,\"fld_control\":%u,\"fld_regs_224\":[",status[0],status[1],(int)status[2],status[3],status[4],status[5],status[6],status[7],status[8],status[9]);
 for(int i=16;i<34;i++)printf("%s%u",i==16?"":",",status[i]);
 printf("],\"return_status\":%u,\"return_position\":%u,\"return_version\":%u,\"physical_profiles\":[",status[40],status[41],status[42]);
 unsigned char *p=(unsigned char *)(status+48);for(int i=0;i<128;i++)printf("%s%u",i?",":"",p[i]);printf("],\"physical_tx_profiles\":[");p=(unsigned char *)(status+80);for(int i=0;i<128;i++)printf("%s%u",i?",":"",p[i]);printf("],\"tx_powerdown_readback\":%u,\"tx_atten_mdb_readback\":%u}\n",status[112],status[113]);close(fd);return 0;
}
