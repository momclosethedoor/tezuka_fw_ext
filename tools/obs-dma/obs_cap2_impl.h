/* CAP2 coherent arena. The kernel is the sole writer of its descriptor CSRs. */
static struct region cap = { .len = OBS_CAP2_LENGTH };
static DEFINE_MUTEX(cap_lock);
static atomic_t cap_opened = ATOMIC_INIT(0);
static atomic_t cap_mapped = ATOMIC_INIT(0);
static u32 cap_last, cap_job;
static bool cap_cpu, cap_poisoned;

enum { CAP_COMMAND = 0x2e4, CAP_KEY = 0x2e8, CAP_JOB = 0x2ec,
       CAP_RESULT = 0x308, CAP_CANCEL = 8 };

static bool cap_terminal(u32 status)
{
	return (status & 15) == 5 || (status & 15) == 6;
}

static bool cap_present(void)
{
	return readl(csr + OBS_CAP2_ID_REG) == OBS_CAP2_ID;
}

static int cap_open(struct inode *inode, struct file *file)
{
	if (!capable(CAP_SYS_RAWIO))
		return -EPERM;
	return atomic_cmpxchg(&cap_opened, 0, 1) ? -EBUSY : 0;
}

static int cap_close(struct inode *inode, struct file *file)
{
	u32 status;

	mutex_lock(&cap_lock);
	if (cap_job && !cap_poisoned) {
		if (!cap_present()) {
			cap_poisoned = true;
		} else {
			if (!cap_terminal(readl(csr + OBS_CAP2_STATUS_REG))) {
				writel(cap_job, csr + CAP_KEY);
				writel(CAP_CANCEL, csr + CAP_COMMAND);
			}
			if (readl_poll_timeout(csr + OBS_CAP2_STATUS_REG, status,
					       cap_terminal(status), 20, 100000) ||
			    readl(csr + CAP_RESULT) != cap_job)
				cap_poisoned = true;
		}
	}
	if (cap_poisoned)
		pr_err("obs_cap2: ownership uncertain; arena quarantined until reboot\n");
	cap_job = 0;
	cap_cpu = false;
	atomic_set(&cap_opened, 0);
	mutex_unlock(&cap_lock);
	return 0;
}

static int cap_submit(const struct obs_cap2_submit *s)
{
	u32 result, status;

	if (!obs_cap2_valid_window(cap.dma, s))
		return -EINVAL;
	if (cap_job)
		return -EBUSY;
	if (s->job <= cap_last)
		return -ESTALE;
	status = readl(csr + OBS_CAP2_STATUS_REG);
	if ((status & 15) && !cap_terminal(status))
		return -EBUSY;

	memset(cap.cpu, OBS_CAP2_SENTINEL, cap.len);
	dma_wmb();
	writel(s->job, csr + CAP_KEY);
	writel(s->job, csr + CAP_JOB);
	writel(s->samples, csr + CAP_JOB + 4);
	writel(s->lease, csr + CAP_JOB + 8);
	writel(lower_32_bits(cap.dma + s->offset), csr + CAP_JOB + 12);
	writel(s->blocks, csr + CAP_JOB + 16);
	writel(s->cookie, csr + CAP_JOB + 20);
	cap_job = cap_last = s->job;
	writel(1, csr + CAP_COMMAND);
	if (readl_poll_timeout(csr + CAP_RESULT, result, result == s->job, 1, 1000)) {
		cap_poisoned = true;
		return -ETIMEDOUT;
	}
	return 0;
}

static long cap_ioctl(struct file *file, unsigned int cmd, unsigned long arg)
{
	void __user *user = (void __user *)arg;
	struct obs_cap2_receipt receipt;
	struct obs_cap2_submit submit;
	u32 status, key;
	long ret = 0;

	mutex_lock(&cap_lock);
	if (!cap_present() || cap_poisoned) {
		cap_poisoned = true;
		ret = -EIO;
		goto out;
	}
	status = readl(csr + OBS_CAP2_STATUS_REG);
	if (cap_job && !cap_cpu &&
	    (!(status & 15) || readl(csr + CAP_RESULT) != cap_job)) {
		cap_poisoned = true;
		ret = -EIO;
		goto out;
	}
	switch (cmd) {
	case OBS_CAP2_INFO: {
		struct obs_cap2_info info = {
			cap.dma, regions[0].dma, regions[1].dma, cap.len,
			OBS_CAP2_GUARD, OBS_CAP2_PAYLOAD, cap_last
		};
		ret = copy_to_user(user, &info, sizeof(info)) ? -EFAULT : 0;
		break;
	}
	case OBS_CAP2_SUBMIT:
		ret = copy_from_user(&submit, user, sizeof(submit)) ? -EFAULT : cap_submit(&submit);
		break;
	case OBS_CAP2_STATUS:
	case OBS_CAP2_ACQUIRE:
		if (cmd == OBS_CAP2_ACQUIRE && (!cap_job || !cap_terminal(status))) {
			ret = -EAGAIN;
			break;
		}
		receipt = (struct obs_cap2_receipt) {
			status, readl(csr + CAP_RESULT), readl(csr + CAP_RESULT + 4),
			readl(csr + CAP_RESULT + 8), readl(csr + CAP_RESULT + 12),
			readl(csr + CAP_RESULT + 16), readl(csr + CAP_RESULT + 20),
			readl(csr + CAP_RESULT + 24), readl(csr + CAP_RESULT + 28)
		};
		if (cmd == OBS_CAP2_ACQUIRE)
			dma_rmb();
		ret = copy_to_user(user, &receipt, sizeof(receipt)) ? -EFAULT : 0;
		if (!ret && cmd == OBS_CAP2_ACQUIRE)
			cap_cpu = true;
		break;
	case OBS_CAP2_CANCEL:
		if (copy_from_user(&key, user, sizeof(key))) {
			ret = -EFAULT;
		} else if (!cap_job || key != cap_job) {
			ret = -EPERM;
		} else if (cap_terminal(status)) {
			ret = -EALREADY;
		} else {
			writel(key, csr + CAP_KEY);
			writel(CAP_CANCEL, csr + CAP_COMMAND);
		}
		break;
	case OBS_CAP2_RELEASE:
		if (!cap_cpu || atomic_read(&cap_mapped))
			ret = -EBUSY;
		else {
			dma_wmb();
			cap_cpu = false;
			cap_job = 0;
		}
		break;
	default:
		ret = -ENOTTY;
	}
out:
	mutex_unlock(&cap_lock);
	return ret;
}

static void cap_vma_open(struct vm_area_struct *vma)
{
	atomic_inc(&cap_mapped);
}

static void cap_vma_close(struct vm_area_struct *vma)
{
	atomic_dec(&cap_mapped);
}

static const struct vm_operations_struct cap_vm_ops = {
	.open = cap_vma_open, .close = cap_vma_close
};

static int cap_mmap(struct file *file, struct vm_area_struct *vma)
{
	int ret = -EBUSY;

	mutex_lock(&cap_lock);
	if (cap_cpu && !cap_poisoned) {
		if (vma->vm_pgoff || vma->vm_end - vma->vm_start != cap.len ||
		    !(vma->vm_flags & VM_SHARED) || vma->vm_flags & VM_WRITE) {
			ret = -EINVAL;
		} else {
			vm_flags_clear(vma, VM_MAYWRITE);
			vm_flags_set(vma, VM_DONTCOPY | VM_DONTDUMP | VM_DONTEXPAND);
			ret = dma_mmap_coherent(dev, vma, cap.cpu, cap.dma, cap.len);
			if (!ret) {
				vma->vm_ops = &cap_vm_ops;
				cap_vma_open(vma);
			}
		}
	}
	mutex_unlock(&cap_lock);
	return ret;
}

static const struct file_operations cap_fops = {
	.owner = THIS_MODULE, .open = cap_open, .release = cap_close,
	.unlocked_ioctl = cap_ioctl, .mmap = cap_mmap
};
static struct miscdevice cap_misc = {
	.minor = MISC_DYNAMIC_MINOR, .name = "obs_cap2", .fops = &cap_fops, .mode = 0600
};

static bool cap_overlaps(u64 start, u64 end)
{
	return cap.dma < end && start < (u64)cap.dma + cap.len;
}

static int cap_register(void)
{
	int ret = -ERANGE;
	int i;

	cap.cpu = dma_alloc_coherent(dev, cap.len, &cap.dma, GFP_KERNEL);
	if (!cap.cpu)
		return -ENOMEM;
	if (!cap.dma || cap.dma & 127 || (u64)cap.dma + cap.len > 0x20000000ULL ||
	    cap_overlaps(0x06000000, 0x0e000000) || cap_overlaps(0x16000000, 0x16040000))
		goto fail;
	for (i = 0; i < ARRAY_SIZE(regions); i++)
		if (cap_overlaps(regions[i].dma, (u64)regions[i].dma + regions[i].len))
			goto fail;
	ret = misc_register(&cap_misc);
	if (ret)
		goto fail;
	pr_info("obs_cap2: coherent arena DMA=%pad len=%zu pinned until reboot\n", &cap.dma, cap.len);
	return 0;
fail:
	dma_free_coherent(dev, cap.len, cap.cpu, cap.dma);
	return ret;
}
