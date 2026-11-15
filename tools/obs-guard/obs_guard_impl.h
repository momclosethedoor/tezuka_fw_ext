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
static DECLARE_RWSEM(obs_gate);
static struct task_struct *obs_worker;
static atomic_t obs_denied=ATOMIC_INIT(0);
static bool obs_block,obs_bus_locked,obs_held;
static struct ad9361_rf_phy *obs_phy;
static void __iomem *obs_csr;
static struct device *obs_dma_dev;
static void *obs_mem[3];
static dma_addr_t obs_dma[3];
static struct gpio_desc *obs_gpio[3];
static DECLARE_WAIT_QUEUE_HEAD(obs_wait);
static DECLARE_COMPLETION(obs_done);
static DEFINE_MUTEX(obs_command_lock);
static int obs_command,obs_error,obs_state;
static u32 obs_readback_status,obs_readback_pos,obs_readback_version;
static u8 obs_readback[128],obs_tx_readback[128];
static u32 obs_tx_pd,obs_tx_atten;
struct obs_setup {u32 magic,catalog_count,map_count,next_count,version,next_version,gpio_base,reserved;u64 restore_lo;u8 catalog[4096],map[4096],next[4096];u32 tx_enable,tx_rf_on,tx_atten_mdb,tx_start_slot;u64 restore_tx_lo;u8 tx_profiles[128];u32 dwell,extra_reserved;};
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
 int r=ad9361_synth_lo_powerdown(obs_phy,LO_DONTCARE,LO_OFF);
 r|=ad9361_set_tx_atten(obs_phy,89750,true,true,true);
 if(r||!(ad9361_spi_read(obs_phy->spi,REG_TX_SYNTH_POWER_DOWN_OVERRIDE)&TX_LO_POWER_DOWN)||ad9361_get_tx_atten(obs_phy,1)!=89750)return -EIO;
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
  if(obs_cfg->tx_rf_on){r=ad9361_set_tx_atten(obs_phy,obs_cfg->tx_atten_mdb,true,true,true);if(r)return r;
   r=ad9361_synth_lo_powerdown(obs_phy,LO_DONTCARE,LO_ON);if(r)return r;}
 }
 obs_tx_pd=!!(ad9361_spi_read(obs_phy->spi,REG_TX_SYNTH_POWER_DOWN_OVERRIDE)&TX_LO_POWER_DOWN);
 obs_tx_atten=ad9361_get_tx_atten(obs_phy,1);
 if(obs_tx_pd!=!obs_cfg->tx_rf_on||obs_tx_atten!=(obs_cfg->tx_rf_on?obs_cfg->tx_atten_mdb:89750))return -EIO;
 r=spi_bus_lock(obs_phy->spi->controller);if(r)return r;WRITE_ONCE(obs_bus_locked,true);
 /* This synchronous locked transaction is behind any already-queued PS work. */
 if(ad9361_spi_read(obs_phy->spi,REG_PRODUCT_ID)<0)return -EIO;
 dma_wmb();obs_w(0x1e4,obs_dma[1]);obs_w(0x1e8,obs_cfg->map_count);obs_w(0x1ec,obs_cfg->version);
 obs_w(0x1f0,obs_dma[0]);obs_w(0x1f4,obs_cfg->catalog_count);obs_w(0x204,obs_phy->state->fastlock.entry[0][0].alc_written|(obs_cfg->tx_start_slot<<8));
 obs_w(0x21c,obs_cfg->dwell);obs_w(0x1e0,2);
 r=readl_poll_timeout(obs_csr+0x228,v,v&2,10,1000000);if(r)return r;
 obs_w(0x1e0,3|(obs_cfg->tx_enable?4:0));obs_state=2;return 0;
}
static int obs_return(void){int i,r;u32 v;unsigned long hz;
 if(!obs_held||!obs_bus_locked)return -EINVAL;
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
 for(;;){wait_event(obs_wait,READ_ONCE(obs_command)!=0);cmd=READ_ONCE(obs_command);r=0;
  if(cmd==2)r=obs_take();
  else if(cmd==3){if(obs_state!=2||obs_r(0x228)&16)r=-EBUSY;else{dma_wmb();obs_w(0x1f8,obs_dma[2]);obs_w(0x1fc,obs_cfg->next_count);obs_w(0x200,obs_cfg->next_version);obs_w(0x220,1);}}
  else if(cmd==4)r=obs_return();else r=-EINVAL;
  obs_error=r;if(r)obs_state=4;WRITE_ONCE(obs_command,0);complete(&obs_done);
 }return 0;
}
static int obs_validate(struct obs_setup *c){u32 i,j;
 if(c->magic!=0x3747534f||c->catalog_count<8||c->catalog_count>128||!c->map_count||c->map_count>128||!c->next_count||c->next_count>128||!c->version||c->next_version==c->version||!c->next_version||c->reserved||c->restore_lo<70000000ULL||c->restore_lo>6000000000ULL)return -EINVAL;
 if(c->tx_enable>1||c->tx_rf_on>1||c->tx_rf_on>c->tx_enable||c->tx_start_slot>7||c->tx_atten_mdb>89750||c->extra_reserved)return -EINVAL;
 if(c->tx_enable&&(c->restore_tx_lo<70000000ULL||c->restore_tx_lo>6000000000ULL))return -EINVAL;
 for(i=0;i<c->catalog_count;i++)for(j=24;j<32;j++)if(c->catalog[i*32+j])return -EINVAL;
 for(j=0;j<2;j++){u8 *p=j?c->next:c->map;u32 n=j?c->next_count:c->map_count;
  for(i=0;i<n;i++){u32 idx=get_unaligned_le32(p+i*32),flags=get_unaligned_le32(p+i*32+4);
   if(idx>=c->catalog_count||(flags&~0x00ff0707)||get_unaligned_le32(p+i*32+12))return -EINVAL;
   /* Segment slots require a prepared TX bank; RF may still be off. */
   if((flags&0x704)&&!c->tx_enable)return -EINVAL;
  }
 }return 0;
}
static long obs_ioctl(struct file *f,unsigned cmd,unsigned long arg){int r=0,i;u32 words[128]={};struct obs_setup *cfg;
 if(!capable(CAP_SYS_RAWIO))return -EPERM;
 if(cmd==0x4605){
  words[0]=2;words[1]=READ_ONCE(obs_state);words[2]=READ_ONCE(obs_error);words[3]=atomic_read(&obs_denied);
  for(i=0;i<3;i++)words[4+i]=obs_dma[i];words[7]=4096;
  words[8]=obs_r(0x64);words[9]=obs_r(0x1e0);
  for(i=0;i<18;i++)words[16+i]=obs_r(0x224+4*i);
  words[40]=obs_readback_status;words[41]=obs_readback_pos;words[42]=obs_readback_version;memcpy(words+48,obs_readback,128);memcpy(words+80,obs_tx_readback,128);words[112]=obs_tx_pd;words[113]=obs_tx_atten;
  return copy_to_user((void __user *)arg,words,sizeof(words))?-EFAULT:0;
 }
 if(mutex_lock_interruptible(&obs_command_lock))return -EINTR;
 if(cmd==0x4601){
  if(obs_held||obs_r(0x1e0)||obs_r(0x228)&7||obs_r(0x64)!=16){r=-EBUSY;goto out;}
  cfg=memdup_user((void __user *)arg,sizeof(*cfg));if(IS_ERR(cfg)){r=PTR_ERR(cfg);goto out;}
  r=obs_validate(cfg);if(r){kfree(cfg);goto out;}
  for(i=0;i<3;i++){if(obs_mem[i])continue;obs_mem[i]=dma_alloc_coherent(obs_dma_dev,4096,&obs_dma[i],GFP_KERNEL);if(!obs_mem[i]||obs_dma[i]%32||obs_dma[i]+4096>0x20000000ULL){r=-ENOMEM;break;}}
  if(r){for(i=0;i<3;i++)if(obs_mem[i]){dma_free_coherent(obs_dma_dev,4096,obs_mem[i],obs_dma[i]);obs_mem[i]=NULL;obs_dma[i]=0;}kfree(cfg);goto out;}
  memcpy(obs_mem[0],cfg->catalog,4096);memcpy(obs_mem[1],cfg->map,4096);memcpy(obs_mem[2],cfg->next,4096);dma_wmb();kfree(obs_cfg);obs_cfg=cfg;obs_state=0;obs_error=0;
 }else if(cmd>=0x4602&&cmd<=0x4604){
  reinit_completion(&obs_done);WRITE_ONCE(obs_command,cmd-0x4600);wake_up(&obs_wait);
  if(!wait_for_completion_timeout(&obs_done,msecs_to_jiffies(10000)))r=-ETIMEDOUT;else r=obs_error;
 }else r=-ENOTTY;
out:mutex_unlock(&obs_command_lock);return r;
}
static const struct file_operations obs_fops={.owner=THIS_MODULE,.unlocked_ioctl=obs_ioctl};
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
