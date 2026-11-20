/* RAM-only guarded replacement of pinned AD9361. Included after static helpers.
 * One worker owns all held locks. No lock is acquired by one userspace task and
 * released by another. All hardware callbacks use the rwsem gate, while the
 * complete physical SPI bus is locked from handoff until cache restoration.
 */
#include <linux/kthread.h>
#include <linux/miscdevice.h>
#include <linux/dma-mapping.h>
#include <linux/platform_device.h>
#include <linux/gpio/driver.h>
#include <linux/gpio/machine.h>
#include <linux/iopoll.h>
#include <linux/fs.h>
#include "../obs-dma/obs_cap2.h"
static DECLARE_RWSEM(obs_gate);
static struct task_struct *obs_worker;
static atomic_t obs_denied=ATOMIC_INIT(0);
static bool obs_block,obs_bus_locked,obs_held;
static struct ad9361_rf_phy *obs_phy;
static void __iomem *obs_csr;
static struct device *obs_dma_dev;
static void *obs_mem[3];
static size_t obs_alloc_bytes;
static dma_addr_t obs_dma[3];
static struct gpio_desc *obs_gpio[3];
static DECLARE_WAIT_QUEUE_HEAD(obs_wait);
static DECLARE_COMPLETION(obs_done);
static DEFINE_MUTEX(obs_command_lock);
static int obs_command,obs_error,obs_state;
/* The persistent FD used by the RF-enabled ARM owner renews a short lease.
 * The kthread retains the SPI lock and is the only task that may hand it back. */
static struct file *obs_rf_owner;
static bool obs_rf_closed;
static unsigned long obs_rf_deadline;
static u32 obs_readback_status,obs_readback_pos,obs_readback_version;
static u8 obs_readback[128],obs_tx_readback[128];
static u32 obs_tx_pd,obs_tx_atten;
struct obs_setup {u32 magic,catalog_count,map_count,next_count,version,next_version,gpio_base,reserved;u64 restore_lo;u8 catalog[4096],map[4096],next[4096];u32 tx_enable,tx_rf_on,tx_atten_mdb,tx_start_slot;u64 restore_tx_lo;u8 tx_profiles[128];u32 dwell,extra_reserved;};
/* setup.extra_reserved: bit0 requests per-visit gate, bits31:16 are the
 * post-transition settling interval in 62.5 MHz clocks. Old drivers reject
 * this field, including maps where every visit requests TX off. */
#define OBS_GATE_ID_REG 0x328
#define OBS_GATE_ID 0x31475458
#define OBS_GATE_SEEDS 0x32c
#define OBS_GATE_WAIT 0x330
#define OBS_GATE_STATUS 0x334
#define OBS_GATE_DURATION 0x338
#define OBS_GATE_OPT BIT(0)
static struct obs_setup *obs_cfg;
bool obs_gate_enter(void){
 if(current==READ_ONCE(obs_worker))return true;
 if(READ_ONCE(obs_block)||!down_read_trylock(&obs_gate)){atomic_inc(&obs_denied);return false;}
 return true;
}
void obs_gate_exit(void){if(current!=READ_ONCE(obs_worker))up_read(&obs_gate);}
int obs_spi_write_then_read(struct spi_device *spi,const void *tx,unsigned nt,void *rx,unsigned nr){
 struct spi_message m;struct spi_transfer t[2]={};u8 *buf;int ret;
 if(!READ_ONCE(obs_bus_locked))return spi_write_then_read(spi,tx,nt,rx,nr);
 if(current!=READ_ONCE(obs_worker))return -EBUSY;
 buf=kmalloc(nt+nr,GFP_KERNEL);if(!buf)return -ENOMEM;memcpy(buf,tx,nt);spi_message_init(&m);
 if(nt){t[0].tx_buf=buf;t[0].len=nt;spi_message_add_tail(&t[0],&m);}
 if(nr){t[1].rx_buf=buf+nt;t[1].len=nr;spi_message_add_tail(&t[1],&m);}
 ret=spi_sync_locked(spi,&m);if(!ret&&nr)memcpy(rx,buf+nt,nr);kfree(buf);return ret;
}
static u32 obs_r(unsigned o){return readl(obs_csr+o);}
static void obs_w(unsigned o,u32 v){writel(v,obs_csr+o);}
static void obs_pins(unsigned slot){int i;for(i=0;i<3;i++)gpiod_set_value_cansleep(obs_gpio[i],(slot>>i)&1);}
static int obs_tx_off(void){
 int pd;
 int r=ad9361_synth_lo_powerdown(obs_phy,LO_DONTCARE,LO_OFF);
 r|=ad9361_set_tx_atten(obs_phy,89750,true,true,true);
 pd=ad9361_spi_read(obs_phy->spi,REG_TX_SYNTH_POWER_DOWN_OVERRIDE);
 if(r||pd<0||!(pd&TX_LO_POWER_DOWN)||ad9361_get_tx_atten(obs_phy,1)!=89750)return -EIO;
 return 0;
}
static int obs_canonical(void){int i,r;u8 check[16];
 for(i=0;i<8;i++){
  r=ad9361_fastlock_load(obs_phy,false,i,obs_cfg->catalog+32*i+8);if(r)return r;
  ad9361_fastlock_save(obs_phy,false,i,check);
  if(memcmp(check,obs_cfg->catalog+32*i+8,16))return -EIO;
 }return 0;
}
static int obs_tx_canonical(void){int i,r;u8 b[16];
 if(!obs_cfg->tx_enable)return 0;
 for(i=0;i<8;i++){r=ad9361_fastlock_load(obs_phy,true,i,obs_cfg->tx_profiles+16*i);if(r)return r;
  r=ad9361_fastlock_save(obs_phy,true,i,b);if(r||memcmp(b,obs_cfg->tx_profiles+16*i,16))return -EIO;}
 return 0;
}
static int obs_take(void){int i,r;u32 v;struct gpio_chip *chip;
 if(!obs_cfg||obs_held||obs_r(0x1e0)||obs_r(0x228)&7||obs_r(0x64)!=16)return -EINVAL;
 if(obs_cfg->tx_rf_on && !READ_ONCE(obs_rf_owner))return -EPERM;
 WRITE_ONCE(obs_block,true);down_write(&obs_gate);obs_held=true;obs_state=1;
 /* All callback families are quiesced. A callback that queued calibration has
  * returned before down_write; flush drains its physical SPI before bus lock. */
 flush_work(&obs_phy->work);mutex_lock(&obs_phy->lock);
 chip=gpiod_to_chip(gpio_to_desc(obs_cfg->gpio_base+63));
 if(!chip||strcmp(chip->label,"zynq_gpio")||chip->base!=obs_cfg->gpio_base||chip->ngpio<66)return -ENODEV;
 for(i=0;i<3;i++){
  obs_gpio[i]=gpiochip_request_own_desc(chip,63+i,"obs006d-guard",GPIO_LOOKUP_FLAGS_DEFAULT,GPIOD_OUT_LOW);
  if(IS_ERR(obs_gpio[i])){r=PTR_ERR(obs_gpio[i]);obs_gpio[i]=NULL;return r;}
 }
 obs_pins(0);r=obs_tx_off();if(r)return r;
 ad9361_fastlock_prepare(obs_phy,false,0,false);ad9361_fastlock_prepare(obs_phy,true,0,false);
 memset(&obs_phy->state->fastlock,0,sizeof(obs_phy->state->fastlock));
 obs_phy->pdata->trx_fastlock_pinctrl_en[0]=true;obs_phy->pdata->trx_fastlock_pinctrl_en[1]=false;
 r=obs_canonical();if(r)return r;r=obs_tx_canonical();if(r)return r;r=ad9361_fastlock_recall(obs_phy,false,0);if(r)return r;
 if(ad9361_spi_read(obs_phy->spi,REG_RX_FAST_LOCK_SETUP)!=3||ad9361_spi_read(obs_phy->spi,REG_TX_FAST_LOCK_SETUP)&2)return -EIO;
 if(obs_cfg->tx_enable){
  u8 orig[8],written[8];
  r=ad9361_fastlock_recall(obs_phy,true,obs_cfg->tx_start_slot);if(r)return r;
  if(ad9361_spi_read(obs_phy->spi,REG_TX_FAST_LOCK_SETUP)!=((obs_cfg->tx_start_slot<<5)|1))return -EIO;
  for(i=0;i<8;i++){orig[i]=obs_phy->state->fastlock.entry[1][i].alc_orig;written[i]=obs_phy->state->fastlock.entry[1][i].alc_written;}
  obs_w(0x208,get_unaligned_le32(orig));obs_w(0x20c,get_unaligned_le32(orig+4));
  obs_w(0x210,get_unaligned_le32(written));obs_w(0x214,get_unaligned_le32(written+4));
  /* RF power and digital executor are deliberately independent. */
  if(obs_cfg->tx_rf_on && !(obs_cfg->extra_reserved & OBS_GATE_OPT)){r=ad9361_set_tx_atten(obs_phy,obs_cfg->tx_atten_mdb,true,true,true);if(r)return r;
   r=ad9361_synth_lo_powerdown(obs_phy,LO_DONTCARE,LO_ON);if(r)return r;}
 }
 obs_tx_pd=!!(ad9361_spi_read(obs_phy->spi,REG_TX_SYNTH_POWER_DOWN_OVERRIDE)&TX_LO_POWER_DOWN);
 obs_tx_atten=ad9361_get_tx_atten(obs_phy,1);
 if(obs_cfg->extra_reserved & OBS_GATE_OPT){
  int pd=ad9361_spi_read(obs_phy->spi,REG_TX_SYNTH_POWER_DOWN_OVERRIDE);
  int tpc=ad9361_spi_read(obs_phy->spi,REG_TX2_DIG_ATTEN);
  if(pd<0 || tpc<0 || !(pd&TX_LO_POWER_DOWN) || obs_tx_atten!=89750)return -EIO;
  obs_w(OBS_GATE_SEEDS,pd|(tpc<<8)|((obs_cfg->tx_atten_mdb/250)<<16));
  obs_w(OBS_GATE_WAIT,obs_cfg->extra_reserved>>16);
 }else if(obs_tx_pd!=!obs_cfg->tx_rf_on||obs_tx_atten!=(obs_cfg->tx_rf_on?obs_cfg->tx_atten_mdb:89750))return -EIO;
 r=spi_bus_lock(obs_phy->spi->controller);if(r)return r;WRITE_ONCE(obs_bus_locked,true);
 /* This synchronous locked transaction is behind any already-queued PS work. */
 if(ad9361_spi_read(obs_phy->spi,REG_PRODUCT_ID)<0)return -EIO;
 dma_wmb();obs_w(0x1e4,obs_dma[1]);obs_w(0x1e8,obs_cfg->map_count);obs_w(0x1ec,obs_cfg->version);
 obs_w(0x1f0,obs_dma[0]);obs_w(0x1f4,obs_cfg->catalog_count);obs_w(0x204,obs_phy->state->fastlock.entry[0][0].alc_written|(obs_cfg->tx_start_slot<<8));
 obs_w(0x21c,obs_cfg->dwell);obs_w(0x1e0,2);
 r=readl_poll_timeout(obs_csr+0x228,v,v&2,10,1000000);if(r)return r;
 if(obs_cfg->extra_reserved & OBS_GATE_OPT){
  obs_w(0x1e0,31);
  obs_w(0x220,4);
 }else obs_w(0x1e0,3|(obs_cfg->tx_enable?4:0));
 obs_state=2;return 0;
}
static int obs_return(void){int i,r;u32 v;unsigned long hz;
 if(!obs_held||!obs_bus_locked)return -EINVAL;
 if(obs_cfg->extra_reserved & OBS_GATE_OPT)
  obs_w(0x1e0,15); /* Revoke before attempting STOP/drain or taking PS SPI. */
 obs_w(0x54,4);r=readl_poll_timeout(obs_csr+0x64,v,(v&19)==16,10,2000000);if(r)return r;
 v=obs_r(0x228);obs_readback_status=v;obs_readback_pos=obs_r(0x234);obs_readback_version=obs_r(0x22c);
 obs_pins((v>>16)&7);obs_w(0x1e0,0);
 r=readl_poll_timeout(obs_csr+0x228,v,!(v&2),10,2000000);if(r)return r;
 for(i=0;i<8;i++){ad9361_fastlock_save(obs_phy,false,i,obs_readback+16*i);if(obs_cfg->tx_enable)ad9361_fastlock_save(obs_phy,true,i,obs_tx_readback+16*i);}
 r=obs_tx_off();if(r)return r;
 ad9361_fastlock_prepare(obs_phy,false,0,false);ad9361_fastlock_prepare(obs_phy,true,0,false);
 memset(&obs_phy->state->fastlock,0,sizeof(obs_phy->state->fastlock));
 r=obs_canonical();if(r)return r;r=obs_tx_canonical();if(r)return r;
 obs_phy->pdata->trx_fastlock_pinctrl_en[0]=false;obs_phy->pdata->trx_fastlock_pinctrl_en[1]=false;obs_pins(0);
 /* Force a genuine normal-mode PLL set even if CCF cached the desired rate. */
 hz=ad9361_to_clk(obs_cfg->restore_lo);
 r=clk_set_rate(obs_phy->clks[RX_RFPLL],hz+500000);if(r)return r;
 r=clk_set_rate(obs_phy->clks[RX_RFPLL],hz);if(r)return r;
 if(abs((long)clk_get_rate(obs_phy->clks[RX_RFPLL])-(long)hz)>10)return -EIO;
 if(obs_cfg->tx_enable){hz=ad9361_to_clk(obs_cfg->restore_tx_lo);
  r=clk_set_rate(obs_phy->clks[TX_RFPLL],hz+500000);if(r)return r;
  r=clk_set_rate(obs_phy->clks[TX_RFPLL],hz);if(r)return r;
  if(abs((long)clk_get_rate(obs_phy->clks[TX_RFPLL])-(long)hz)>10)return -EIO;}
 r=obs_tx_off();if(r)return r;
 obs_tx_pd=!!(ad9361_spi_read(obs_phy->spi,REG_TX_SYNTH_POWER_DOWN_OVERRIDE)&TX_LO_POWER_DOWN);obs_tx_atten=ad9361_get_tx_atten(obs_phy,1);
 WRITE_ONCE(obs_bus_locked,false);spi_bus_unlock(obs_phy->spi->controller);
 for(i=0;i<3;i++){gpiochip_free_own_desc(obs_gpio[i]);obs_gpio[i]=NULL;}
 mutex_unlock(&obs_phy->lock);obs_held=false;obs_state=3;up_write(&obs_gate);WRITE_ONCE(obs_block,false);return 0;
}
static int obs_thread(void *arg){int cmd,r;
 for(;;){
  if(READ_ONCE(obs_rf_owner) && READ_ONCE(obs_state)==2){
   wait_event_timeout(obs_wait,READ_ONCE(obs_command)!=0,msecs_to_jiffies(100));
   if(!READ_ONCE(obs_command) && time_after_eq(jiffies,READ_ONCE(obs_rf_deadline))){
    if(mutex_trylock(&obs_command_lock)){
     if(!obs_command && time_after_eq(jiffies,obs_rf_deadline)){
      obs_error=obs_return();if(obs_error)obs_state=4;
      WRITE_ONCE(obs_rf_owner,NULL);
     }
     mutex_unlock(&obs_command_lock);
    }
   }
  }else wait_event(obs_wait,READ_ONCE(obs_command)!=0);
  cmd=READ_ONCE(obs_command);if(!cmd)continue;r=0;
  if(cmd==2){
   r=obs_take();
   if(r && obs_rf_owner && obs_held){
    /* obs_take can fail while PS owns SPI or while waiting for mux ACK. */
    if(obs_bus_locked)obs_return();else obs_tx_off();
   }
  }
  else if(cmd==3){if(obs_state!=2||obs_r(0x228)&16)r=-EBUSY;else{dma_wmb();obs_w(0x1f8,obs_dma[2]);obs_w(0x1fc,obs_cfg->next_count);obs_w(0x200,obs_cfg->next_version);obs_w(0x220,1);}}
  else if(cmd==4)r=obs_return();else r=-EINVAL;
  obs_error=r;if(r)obs_state=4;WRITE_ONCE(obs_command,0);complete(&obs_done);
 }return 0;
}
static int obs_validate(struct obs_setup *c,u8 *tables,size_t stride){u32 i,j;
 bool cap2 = obs_r(OBS_CAP2_ID_REG) == OBS_CAP2_ID;
 if(c->catalog_count<8||c->catalog_count>stride/32||!c->map_count||c->map_count>stride/32||!c->next_count||c->next_count>stride/32||!c->version||c->next_version==c->version||!c->next_version||c->reserved||c->restore_lo<70000000ULL||c->restore_lo>6000000000ULL||c->gpio_base!=512)return -EINVAL;
 if(c->tx_enable>1||c->tx_rf_on>1||c->tx_rf_on>c->tx_enable||c->tx_start_slot>7||c->tx_atten_mdb>89750||(c->extra_reserved && !(c->extra_reserved & OBS_GATE_OPT)))return -EINVAL;
 if(c->extra_reserved & OBS_GATE_OPT){
  if(obs_r(OBS_GATE_ID_REG)!=OBS_GATE_ID)return -EOPNOTSUPP;
  if((c->extra_reserved & 0xfffe) || !c->tx_rf_on || c->tx_atten_mdb%250)return -EINVAL;
 }
 if(c->tx_enable&&(c->restore_tx_lo<70000000ULL||c->restore_tx_lo>6000000000ULL))return -EINVAL;
 for(i=0;i<c->catalog_count;i++)for(j=24;j<32;j++)if(tables[i*32+j])return -EINVAL;
 for(j=0;j<2;j++){u8 *p=tables+(1+j)*stride;u32 n=j?c->next_count:c->map_count;
  for(i=0;i<n;i++){u32 idx=get_unaligned_le32(p+i*32),flags=get_unaligned_le32(p+i*32+4);
   u32 job = get_unaligned_le32(p+i*32+12);
   bool assigned = flags & BIT(3);
   if(idx>=c->catalog_count||(flags & ~(0x00ff070fU | ((c->extra_reserved & OBS_GATE_OPT)?BIT(4):0)))||assigned!=!!job)return -EINVAL;
   if(assigned&&!cap2)return -EOPNOTSUPP;
   /* Segment slots require a prepared TX bank; RF may still be off. */
   if((flags&0x704)&&!c->tx_enable)return -EINVAL;
  }
 }return 0;
}
static int obs_install(void __user *arg,bool extended){
 struct obs_setup *cfg;
 void *fresh[3]={};
 dma_addr_t addr[3];
 u8 *tables;
 size_t stride=4096;
 u32 entries;
 int i,r;
 cfg=kzalloc(sizeof(*cfg),GFP_KERNEL);
 if(!cfg)return -ENOMEM;
 if(copy_from_user(cfg,arg,offsetof(struct obs_setup,catalog))){r=-EFAULT;goto free_cfg;}
 if(cfg->magic!=(extended?0x3847534f:0x3747534f)){r=-EINVAL;goto free_cfg;}
 if(extended){
  entries=max(cfg->catalog_count,max(cfg->map_count,cfg->next_count));
  if(entries<8||entries>65535){r=-EINVAL;goto free_cfg;}
  stride=32*max(entries,128U);
 }
 tables=vmemdup_user((u8 __user *)arg+40,stride*3);
 if(IS_ERR(tables)){r=PTR_ERR(tables);goto free_cfg;}
 if(copy_from_user((u8 *)cfg+offsetof(struct obs_setup,tx_enable),(u8 __user *)arg+40+3*stride,
     sizeof(*cfg)-offsetof(struct obs_setup,tx_enable))){r=-EFAULT;goto free_tables;}
 r=obs_validate(cfg,tables,stride);if(r)goto free_tables;
 memcpy(cfg->catalog,tables,8*32);
 if(stride>obs_alloc_bytes){
  for(i=0;i<3;i++){
   fresh[i]=dma_alloc_coherent(obs_dma_dev,stride,&addr[i],GFP_KERNEL);
   if(!fresh[i] || addr[i]%32 || (u64)addr[i]+stride>0x20000000ULL){r=-ENOMEM;goto free_fresh;}
  }
  for(i=0;i<3;i++){
   if(obs_mem[i])dma_free_coherent(obs_dma_dev,obs_alloc_bytes,obs_mem[i],obs_dma[i]);
   obs_mem[i]=fresh[i];obs_dma[i]=addr[i];
  }
  obs_alloc_bytes=stride;
 }
 for(i=0;i<3;i++)memcpy(obs_mem[i],tables+i*stride,stride);
 dma_wmb();kfree(obs_cfg);obs_cfg=cfg;obs_state=0;obs_error=0;kvfree(tables);return 0;
free_fresh:
 for(i=0;i<3;i++)if(fresh[i])dma_free_coherent(obs_dma_dev,stride,fresh[i],addr[i]);
free_tables:
 kvfree(tables);
free_cfg:
 kfree(cfg);return r;
}
/* A bound tuple belongs to one physical RX entry. A transition can start
 * between MMIO reads; drop the binding on either edge of that interval. */
static void obs_gate_snapshot(u32 *words)
{
 u32 before = obs_r(OBS_GATE_STATUS);
 u32 epoch = obs_r(0x180);
 words[119]=obs_r(OBS_GATE_DURATION);
 words[120]=obs_r(0x22c); /* map version */
 words[121]=obs_r(0x234); /* visit position */
 words[122]=obs_r(0x230); /* pass */
 words[123]=epoch;
 words[124]=obs_r(0x184);
 words[118]=obs_r(OBS_GATE_STATUS);
 if(before!=words[118] || epoch!=obs_r(0x180) || (before & BIT(7)))
  words[118]&=~BIT(6);
}
static long obs_ioctl(struct file *f,unsigned cmd,unsigned long arg){int r=0,i;u32 words[128]={};
 if(!capable(CAP_SYS_RAWIO))return -EPERM;
 if(cmd==0x4605){
  words[0]=2;words[1]=READ_ONCE(obs_state);words[2]=READ_ONCE(obs_error);words[3]=atomic_read(&obs_denied);
  for(i=0;i<3;i++)words[4+i]=obs_dma[i];words[7]=obs_alloc_bytes?:4096;words[115]=65535;words[116]=1;
  words[117]=obs_r(OBS_GATE_ID_REG);
  if(words[117]==OBS_GATE_ID)obs_gate_snapshot(words);
  words[8]=obs_r(0x64);words[9]=obs_r(0x1e0);words[114]=1;
  for(i=0;i<18;i++)words[16+i]=obs_r(0x224+4*i);
  words[40]=obs_readback_status;words[41]=obs_readback_pos;words[42]=obs_readback_version;memcpy(words+48,obs_readback,128);memcpy(words+80,obs_tx_readback,128);words[112]=obs_tx_pd;words[113]=obs_tx_atten;
  return copy_to_user((void __user *)arg,words,sizeof(words))?-EFAULT:0;
 }
 if(mutex_lock_interruptible(&obs_command_lock))return -EINTR;
 if(cmd==0x4606){
  if(obs_rf_closed || obs_rf_owner!=f || obs_state!=2){r=-ESTALE;goto out;}
  if(time_after_eq(jiffies,obs_rf_deadline)){r=-ETIMEDOUT;goto out;}
  if(obs_cfg->extra_reserved & OBS_GATE_OPT){
   if(obs_r(OBS_GATE_STATUS)&BIT(8)){r=-ESTALE;goto out;}
   obs_w(0x220,4);
  }
  WRITE_ONCE(obs_rf_deadline,jiffies+msecs_to_jiffies(750));goto out;
 }
 if(cmd==0x4607){
  if(obs_rf_owner || !obs_cfg || !obs_cfg->tx_rf_on || obs_held){r=-EBUSY;goto out;}
  obs_rf_closed=false;WRITE_ONCE(obs_rf_owner,f);
  WRITE_ONCE(obs_rf_deadline,jiffies+msecs_to_jiffies(750));cmd=0x4602;
 }else if(obs_rf_owner && obs_rf_owner!=f){r=-EBUSY;goto out;}
 else if(cmd==0x4602 && obs_cfg && obs_cfg->tx_rf_on){r=-EPERM;goto out;}
 if(cmd==0x4601||cmd==0x4608){
  if(obs_held||obs_r(0x1e0)||obs_r(0x228)&7||obs_r(0x64)!=16){r=-EBUSY;goto out;}
  r=obs_install((void __user *)arg,cmd==0x4608);
 }else if(cmd>=0x4602&&cmd<=0x4604){
  if(cmd==0x4604 && obs_r(OBS_CAP2_ID_REG)==OBS_CAP2_ID){
   u32 state=obs_r(OBS_CAP2_STATUS_REG)&15;
   if(state && state!=5 && state!=6){
    if(obs_held && obs_cfg && (obs_cfg->extra_reserved & OBS_GATE_OPT)){
     obs_w(0x1e0,15);
     obs_w(0x54,4);
    }
    r=-EBUSY;goto out;
   }
  }
  reinit_completion(&obs_done);WRITE_ONCE(obs_command,cmd-0x4600);wake_up(&obs_wait);
  if(!wait_for_completion_timeout(&obs_done,msecs_to_jiffies(10000)))r=-ETIMEDOUT;else r=obs_error;
  if(cmd==0x4604 && !r)WRITE_ONCE(obs_rf_owner,NULL);
  if(cmd==0x4602 && obs_rf_owner==f && !r)WRITE_ONCE(obs_rf_deadline,jiffies+msecs_to_jiffies(750));
 }else r=-ENOTTY;
out:mutex_unlock(&obs_command_lock);return r;
}
static int obs_file_release(struct inode *inode,struct file *f){
 mutex_lock(&obs_command_lock);
 if(obs_rf_owner==f){
  if(obs_held && obs_cfg && (obs_cfg->extra_reserved & OBS_GATE_OPT))obs_w(0x1e0,15);
  obs_rf_closed=true;WRITE_ONCE(obs_rf_deadline,jiffies);
  if(!obs_held)WRITE_ONCE(obs_rf_owner,NULL);
 }
 mutex_unlock(&obs_command_lock);return 0;
}
static const struct file_operations obs_fops={.owner=THIS_MODULE,.unlocked_ioctl=obs_ioctl,.release=obs_file_release};
static struct miscdevice obs_misc={.minor=MISC_DYNAMIC_MINOR,.name="obs_fl_guard",.fops=&obs_fops,.mode=0600};
static int obs_guard_probe(struct ad9361_rf_phy *phy){int r;
 if(obs_phy)return -EBUSY;obs_csr=ioremap(0x7c460000,4096);if(!obs_csr)return -ENOMEM;
 if(obs_r(0x224)!=0x31444c46||obs_r(0x1e0)||obs_r(0x228)&7)return -EINVAL;
 obs_dma_dev=bus_find_device_by_name(&platform_bus_type,NULL,"7c460000.maia-sdr");if(!obs_dma_dev)return -ENODEV;
 r=dma_set_mask_and_coherent(obs_dma_dev,DMA_BIT_MASK(32));if(r)return r;
 obs_phy=phy;obs_worker=kthread_run(obs_thread,NULL,"obs_fl_guard");if(IS_ERR(obs_worker))return PTR_ERR(obs_worker);
 r=misc_register(&obs_misc);if(r)return r;
 __module_get(THIS_MODULE);pr_info("obs006d: guarded driver ready, DMA lifetime pinned until reboot\n");return 0;
}
