/* Bounded real8 single-field port of the handoff search/64-line fit/pixel path.
 * Experimental native port: parity/field-length and every copied row are checked.
 * No previous frame, missing-row interpolation or sync-only success is allowed.
 * No claim of bit-identical pixels to the NumPy reference (float ordering differs).
 */
#include <math.h>
#include <string.h>
#define FCAP 2097152
typedef struct {
 float *d,*y,*s; size_t n,ny,ns,acq; int locked,pol,ntsc,k,vs,par,last_broad,rows,ready,bad;
 double T,p0,tip,blank,thr,begin,end; unsigned char pixels[720*288],covered[288];
} Field;
static int cmpf(const void*a,const void*b){float x=*(const float*)a,y=*(const float*)b;return(x>y)-(x<y);}
static double clampd(double x,double lo,double hi){return fmax(lo,fmin(hi,x));}
static int field_init(Field*f){memset(f,0,sizeof(*f));f->d=calloc(FCAP,sizeof(float));f->y=calloc(FCAP,sizeof(float));f->s=calloc(FCAP/6,sizeof(float));f->vs=-1;f->last_broad=-10000;return f->d&&f->y&&f->s;}
static void field_free(Field*f){free(f->d);free(f->y);free(f->s);}
static double sample_s(Field*f,double e,double offset){int j=(int)floor((e-2.5)/6+offset*15360000/6);j=j<0?0:j>=(int)f->ns?(int)f->ns-1:j;return f->pol*f->s[j];}
static int acquire_field(Field*f){
 const int N=30720; if(f->ns-f->acq<N)return 0;f->acq=f->ns;int start=f->ns-N;
 float*q=malloc(N*sizeof(float));if(!q)return 0;memcpy(q,f->s+start,N*sizeof(float));qsort(q,N,sizeof(float),cmpf);
 double thresholds[2]={(q[(int)round(.01*(N-1))]+q[(int)round(.2*(N-1))])*.5,(-q[(int)round(.99*(N-1))]-q[(int)round(.8*(N-1))])*.5};free(q);
 double best[1024],edges[1024],diff[1024];int nb=0,bscore=0,pol=1;double T0=0;
 for(int side=0;side<2;side++){
  int p=side?-1:1,fall=-1,ne=0,nd=0;double thr=thresholds[side];
  for(int j=1;j<N;j++){double a=p*f->s[start+j-1],b=p*f->s[start+j];
   if(a>=thr&&b<thr)fall=j;
   if(a<thr&&b>=thr&&fall>=1){double width=(j-fall)*6/15360000.;if(width>=3e-6&&width<=6.5e-6&&ne<1024){double u=p*f->s[start+fall-1],v=p*f->s[start+fall];edges[ne++]=fall-1+(u-thr)/fmax(u-v,1e-12);}fall=-1;}}
  for(int j=1;j<ne;j++){double x=edges[j]-edges[j-1];if(x>153.6&&x<174.08)diff[nd++]=x;}
  if(nd>bscore){bscore=nd;nb=ne;pol=p;memcpy(best,edges,ne*sizeof(double));for(int j=0;j<nd;j++)edges[j]=diff[j];for(int j=1;j<nd;j++){double x=edges[j];int z=j;while(z&&edges[z-1]>x){edges[z]=edges[z-1];z--;}edges[z]=x;}T0=edges[nd/2];}}
 if(bscore<94||nb<8)return 0;
 double slope=T0,intercept=best[0],lim=1e20;
 for(int pass=0;pass<2;pass++){double sx=0,sy=0,sxx=0,sxy=0;int n=0;
  for(int j=0;j<nb;j++){double x=round((best[j]-best[0])/T0);if(fabs(best[j]-slope*x-intercept)>=lim)continue;sx+=x;sy+=best[j];sxx+=x*x;sxy+=x*best[j];n++;}
  if(n<8||n*sxx-sx*sx<=0)return 0;slope=(n*sxy-sx*sy)/(n*sxx-sx*sx);intercept=(sy-slope*sx)/n;
  double var=0;for(int j=0;j<nb;j++){double x=round((best[j]-best[0])/T0),r=best[j]-slope*x-intercept;var+=r*r;}lim=fmax(3*sqrt(var/nb),.02*T0);}
 double pal=983.04,nt=15360000./15734.264;f->ntsc=fabs(slope*6-nt)<fabs(slope*6-pal);if(fabs(slope*6/(f->ntsc?nt:pal)-1)>.02)return 0;
 f->pol=pol;double tip=0,blank=0;int n=0;double porch[3]={f->ntsc?5.6e-6:5.8e-6,f->ntsc?6.8e-6:7.2e-6,f->ntsc?8e-6:8.6e-6};
 for(int j=0;j<nb;j++){double x=round((best[j]-best[0])/T0);if(fabs(best[j]-slope*x-intercept)>=lim)continue;double e=(best[j]+start)*6+2.5;tip+=sample_s(f,e,1e-6)+sample_s(f,e,1.8e-6);for(int z=0;z<3;z++)blank+=sample_s(f,e,porch[z]);n++;}
 if(n<8)return 0;f->tip=tip/(2*n);f->blank=blank/(3*n);if(f->blank<=f->tip)return 0;f->thr=(f->tip+f->blank)*.5;
 f->T=slope*6;double kk=floor((N-2.5*slope-intercept)/slope);f->p0=(slope*kk+intercept+start)*6+2.5;f->locked=1;f->k=0;f->vs=-1;f->bad=0;return 1;
}
static void field_pixels(Field*f,double e,int line){
 int need=f->ntsc?240:288,off=(f->ntsc?18:22)+f->par,row=line-f->vs-off;if(f->vs<0||row<0||row>=need)return;
 double start=(f->ntsc?122:132)*15360000./13500000.,step=15360000./13500000.;
 if(e+start<0||e+start+719*step+2>=f->ny){f->bad=1;return;}
 double span=(f->ntsc?2.5:7./3)*(f->blank-f->tip);if(span<=0){f->bad=1;return;}float A=f->pol*255/span,B=-f->blank*255/span+.5;
 for(int j=0;j<720;j++){double x=e+start+j*step;int a=(int)x;float frac=x-a,v0=clampd(A*f->y[a]+B,0,255.999),v1=clampd(A*f->y[a+1]+B,0,255.999);f->pixels[row*720+j]=(unsigned char)(v0+frac*(v1-v0));}
 if(!f->covered[row]){f->covered[row]=1;f->rows++;}
}
static int field_feed(Field*f,const int8_t*raw,size_t n){
 if(n>FCAP-f->n)return -1;size_t old=f->n;
 for(size_t i=0;i<n;i++){float v=raw[i];if(v==-128)v=(old+i)?f->d[old+i-1]:0;f->d[old+i]=v;}f->n+=n;
 const float h0=-.01105553843081f,h1=.16332732141018f,h2=.69545644521713f;
 for(size_t i=f->ny>2?f->ny:2;i+2<f->n;i++)f->y[i]=h2*f->d[i]+h1*(f->d[i-1]+f->d[i+1])+h0*(f->d[i-2]+f->d[i+2]);f->ny=f->n-2;
 while((f->ns+1)*6<=f->n){float v=0;for(int j=0;j<6;j++)v+=f->d[f->ns*6+j];f->s[f->ns++]=v/6;}
 if(!f->locked&&!acquire_field(f))return 0;
 while(f->p0+64*f->T+8<f->ny&&!f->ready){
  double residual[64],fit[64],c=0,mr=0,mk=0;int found[64],nf=0;
  for(int i=0;i<64;i++){double p=f->p0+i*f->T,g=(p-2.5)/6,best=1e30,r=0;found[i]=0;int mid=(int)floor(g);
   for(int j=mid-3;j<=mid+3;j++){if(j<0||j+1>=(int)f->ns)continue;double a=f->pol*f->s[j],b=f->pol*f->s[j+1];if(a>=f->thr&&b<f->thr){double e=(j+(a-f->thr)/fmax(a-b,1e-12))*6+2.5;if(fabs(e-p)<best){best=fabs(e-p);r=e-p;found[i]=1;}}}
   residual[i]=r;nf+=found[i];}
  if(nf<26){f->locked=0;f->bad=1;return 0;}
  for(int pass=0;pass<2;pass++){double sx=0,sy=0,sxx=0,sxy=0;int m=0;for(int i=0;i<64;i++)if(found[i]){sx+=i;sy+=residual[i];sxx+=i*i;sxy+=i*residual[i];m++;}if(m<3){f->locked=0;f->bad=1;return 0;}mk=sx/m;mr=sy/m;c=(sxy-sx*sy/m)/(sxx-sx*sx/m);for(int i=0;i<64;i++)if(fabs(residual[i]-mr-c*(i-mk))>4.608)found[i]=0;}
  c=clampd(c,-.02*f->T,.02*f->T);for(int i=0;i<64;i++)fit[i]=f->p0+i*f->T+mr+c*(i-mk);
  double h1s[64],h2s[64],tip=0,blank=0;int normal=0;double ratio=(f->ntsc?1./15734.264:64e-6)/64e-6;
  for(int i=0;i<64;i++){int a=0,b=0;for(int j=0;j<8;j++){a+=sample_s(f,fit[i],(6+24*j/7.)*1e-6)<f->thr;b+=sample_s(f,fit[i],(38+24*j/7.)*1e-6*ratio)<f->thr;}h1s[i]=a/8.;h2s[i]=b/8.;
   if(a<5&&b<5&&found[i]){tip+=sample_s(f,fit[i],1e-6)+sample_s(f,fit[i],1.8e-6);blank+=sample_s(f,fit[i],f->ntsc?5.6e-6:5.8e-6)+sample_s(f,fit[i],f->ntsc?6.8e-6:7.2e-6)+sample_s(f,fit[i],f->ntsc?8e-6:8.6e-6);normal++;}}
  if(normal>=2){f->tip+=.15*(tip/(2*normal)-f->tip);f->blank+=.15*(blank/(3*normal)-f->blank);f->thr=(f->tip+f->blank)*.5;}
  for(int i=0;i<64;i++){int line=f->k+i;
   if(h1s[i]>.6||h2s[i]>.6){int prev=f->last_broad;f->last_broad=line;if(line-prev>12){int par=h1s[i]>.6?0:1,dist=line-f->vs,need=f->ntsc?240:288,ab=f->ntsc?262:312;
     int length_ok=f->par!=par?dist==ab+f->par:(dist==ab||dist==ab+1);
     if(f->vs>=0&&length_ok&&!f->bad&&f->rows==need&&f->blank>f->tip){f->ready=1;f->end=fit[i];return 1;}
     f->vs=line;f->par=par;f->begin=fit[i];f->bad=0;f->rows=0;memset(f->covered,0,sizeof(f->covered));}}
   field_pixels(f,fit[i],line);}
  f->p0+=64*f->T+mr+c*(64-mk);f->T=clampd(f->T+.3*c,(f->ntsc?15360000./15734.264:983.04)*.98,(f->ntsc?15360000./15734.264:983.04)*1.02);f->k+=64;
 }
 return f->ready;
}
