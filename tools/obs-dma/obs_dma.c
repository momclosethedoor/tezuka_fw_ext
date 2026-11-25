// SPDX-License-Identifier: GPL-2.0
/* Temporary r3 DMA owner. Allocations are pinned until reboot, never file close.
 * Load only after the RAM loader restored the real Maia platform device.
 * ABI 1: INFO 0x4f01, SYNC 0x4f02. mmap selectors: offset0=FULL, offset16MiB=VIEW.
 * CPU mappings are read-only. The single reader syncs after commit and before
 * cumulative release; mmap/close/errors never give allocation back to Linux. */
#include <linux/module.h>
#include <linux/platform_device.h>
#include <linux/dma-mapping.h>
#include <linux/miscdevice.h>
#include <linux/fs.h>
#include <linux/mm.h>
#include <linux/uaccess.h>
#include <linux/io.h>
#include <linux/iopoll.h>
#include <linux/mutex.h>
#include "obs_cap2.h"

static bool cached = true;
module_param(cached, bool, 0444);
struct region { void *cpu; struct page *page; dma_addr_t dma; size_t len; };
static struct region regions[2] = { {.len=0x2000000}, {.len=0x200000} };
static struct device *dev;
static void __iomem *csr;
static atomic_t opened=ATOMIC_INIT(0);
static bool quarantine;
static u32 view_stride, spectrum_id, spectrum_revision;
static u32 spectrum_view_stride(void)
{
 u32 id=readl(csr+0x50), revision=readl(csr+0xe4);
 if (id==0x354e424f && (revision==1 || revision==2) && (readl(csr+0xe0)&0x1ff)==0x1ff)
  return 4096;
 if ((id==0x324e424f && revision==3) ||
     (id==0x334e424f && revision==4 && (readl(csr+0xe0)&0x60)==0x60))
  return 2048;
 return 0;
}
static bool supported_spectrum(void)
{
 return view_stride && readl(csr+0x50)==spectrum_id && readl(csr+0xe4)==spectrum_revision &&
        spectrum_view_stride()==view_stride &&
        readl(csr+0x1c0)==1024 && readl(csr+0x1c4)==32768 &&
        readl(csr+0x1c8)==view_stride;
}
static bool engine_drained(u32 status)
{
 /* running, DMA busy and DRAINED are live in both supported revisions. */
 return (status & 0x13)==0x10;
}
struct info { u64 full, view; u32 full_len, view_len, cached, cache_line, abi, reserved; };
struct sync_range { u32 region, offset, len, for_cpu; };
static int open_owner(struct inode *i, struct file *f)
{
 if (!capable(CAP_SYS_RAWIO)) return -EPERM;
 if (atomic_cmpxchg(&opened,0,1)) return -EBUSY;
 if (READ_ONCE(quarantine)) { atomic_set(&opened,0); return -EIO; }
 if (!supported_spectrum()) {
  WRITE_ONCE(quarantine,true);
  atomic_set(&opened,0);
  return -ENODEV;
 }
 return 0;
}
static int close_owner(struct inode *i, struct file *f)
{
 u32 status;
 if (supported_spectrum()) {
  writel(4,csr+0x54);
  if (readl_poll_timeout(csr+0x64,status,engine_drained(status),100,500000))
   WRITE_ONCE(quarantine,true);
 } else {
  WRITE_ONCE(quarantine,true);
 }
 if (READ_ONCE(quarantine)) pr_err("obs_dma: unconfirmed STOP/ABI; allocations pinned, reboot required\n");
 atomic_set(&opened,0);
 return 0;
}
static long control(struct file *f, unsigned int cmd, unsigned long arg)
{
 if (cmd==0x4f01) {
  struct info v={regions[0].dma,regions[1].dma,regions[0].len,regions[1].len,cached,dma_get_cache_alignment(),1,0};
  return copy_to_user((void __user *)arg,&v,sizeof(v)) ? -EFAULT : 0;
 }
 if (cmd==0x4f02) {
  struct sync_range s; struct region *r; unsigned int line=dma_get_cache_alignment();
  if (copy_from_user(&s,(void __user *)arg,sizeof(s))) return -EFAULT;
  if (s.region>1 || s.for_cpu>1 || !s.len || s.offset%line || s.len%line) return -EINVAL;
  r=&regions[s.region]; if (s.offset>r->len || s.len>r->len-s.offset) return -EINVAL;
  if (cached) {
   if (s.for_cpu) dma_sync_single_for_cpu(dev,r->dma+s.offset,s.len,DMA_FROM_DEVICE);
   else dma_sync_single_for_device(dev,r->dma+s.offset,s.len,DMA_FROM_DEVICE);
  }
  if (s.for_cpu) dma_rmb(); else dma_wmb();
  return 0;
 }
 return -ENOTTY;
}
static int map_region(struct file *f, struct vm_area_struct *vma)
{
 unsigned long off=vma->vm_pgoff<<PAGE_SHIFT; struct region *r;
 if (off!=0 && off!=0x1000000) return -EINVAL;
 r=&regions[off!=0];
 if (vma->vm_end-vma->vm_start!=r->len || vma->vm_flags&VM_WRITE) return -EINVAL;
 vm_flags_clear(vma,VM_MAYWRITE); vm_flags_set(vma,VM_DONTCOPY|VM_DONTDUMP);
 vma->vm_pgoff=0;
 return cached ? dma_mmap_pages(dev,vma,r->len,r->page) : dma_mmap_coherent(dev,vma,r->cpu,r->dma,r->len);
}
static const struct file_operations fops={.owner=THIS_MODULE,.open=open_owner,.release=close_owner,.unlocked_ioctl=control,.mmap=map_region};
static struct miscdevice misc={.minor=MISC_DYNAMIC_MINOR,.name="obs_dma",.fops=&fops,.mode=0600};
#include "obs_cap2_impl.h"
static void free_regions(void)
{
 int i; for(i=0;i<2;i++) {
  struct region *r=&regions[i];
  if (r->page) dma_free_pages(dev,r->len,r->page,r->dma,DMA_FROM_DEVICE);
  if (r->cpu) dma_free_coherent(dev,r->len,r->cpu,r->dma);
 }
}
static int __init begin(void)
{
 int i,err=-ENODEV;
 csr=ioremap(0x7c460000,0x1000); if(!csr) return -ENOMEM;
 spectrum_id=readl(csr+0x50);
 spectrum_revision=readl(csr+0xe4);
 view_stride=spectrum_view_stride();
 if(!supported_spectrum() || !engine_drained(readl(csr+0x64))) goto fail;
 regions[1].len=1024*view_stride;
 dev=bus_find_device_by_name(&platform_bus_type,NULL,"7c460000.maia-sdr"); if(!dev) goto fail;
 err=dma_set_mask_and_coherent(dev,DMA_BIT_MASK(32)); if(err) goto fail;
 for(i=0;i<2;i++) {
  struct region *r=&regions[i];
  if(cached) r->page=dma_alloc_pages(dev,r->len,&r->dma,DMA_FROM_DEVICE,GFP_KERNEL|__GFP_ZERO);
  else r->cpu=dma_alloc_coherent(dev,r->len,&r->dma,GFP_KERNEL);
  if(!r->page && !r->cpu) { err=-ENOMEM; goto fail; }
  if(r->dma%(i?view_stride:32768) || (u64)r->dma+r->len>0xffffffffULL) { err=-EINVAL; goto fail; }
  if(cached) dma_sync_single_for_device(dev,r->dma,r->len,DMA_FROM_DEVICE);
  pr_info("obs_dma: region%d DMA=%pad len=%zu cached=%d\n",i,&r->dma,r->len,cached);
 }
 err=misc_register(&misc); if(err) goto fail;
 if (readl(csr + OBS_CAP2_ID_REG) == OBS_CAP2_ID) {
  err = cap_register();
  if (err) { misc_deregister(&misc); goto fail; }
 }
 __module_get(THIS_MODULE); /* intentional: only reboot relinquishes memory */
 pr_info("obs_dma: ownership PINNED until reboot; close only stops writer\n");
 return 0;
fail:
 if(dev) { free_regions(); put_device(dev); }
 iounmap(csr); return err;
}
static void __exit end(void) { /* ordinary unload prevented by permanent pin */ }
module_init(begin); module_exit(end);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("OBS r3 owned DMA buffers, read-only mmap, reboot lifetime");
