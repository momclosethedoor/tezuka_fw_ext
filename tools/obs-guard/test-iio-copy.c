#include "obs-iio-copy.h"
#include <assert.h>
#include <stdlib.h>
#include <stdio.h>
int main(void){
 size_t maximum=262144;uint8_t *source=malloc(maximum+32),*dest=malloc(maximum+32);assert(source&&dest);
 for(size_t i=0;i<maximum+32;i++)source[i]=(uint8_t)((i*37u)^(i>>8)^(i>>16));
 const unsigned widths[]={0,32,64,128};const size_t lengths[]={0,1,7,31,32,33,63,64,65,127,128,129,65535,65536,131072,262144};
 for(unsigned w=0;w<4;w++)for(unsigned a=0;a<8;a++)for(unsigned c=0;c<8;c++)for(unsigned j=0;j<sizeof(lengths)/sizeof(lengths[0]);j++){
  memset(dest,0xa5,maximum+32);obs_iio_copy(dest+8+a,source+8+c,lengths[j],widths[w]);
  for(unsigned i=0;i<8+a;i++)assert(dest[i]==0xa5);
  assert(!memcmp(dest+8+a,source+8+c,lengths[j]));
  for(size_t i=8+a+lengths[j];i<maximum+32;i++)assert(dest[i]==0xa5);
 }
 free(source);free(dest);puts("IIO copy: widths, exact bytes, alignments, tails and guards PASS");return 0;
}
