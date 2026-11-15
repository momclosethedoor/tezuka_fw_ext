#ifndef OBS_GUARD_API_H
#define OBS_GUARD_API_H
#include <linux/rwsem.h>
#include <linux/sched.h>
bool obs_gate_enter(void);
void obs_gate_exit(void);
int obs_spi_write_then_read(struct spi_device *spi, const void *tx, unsigned n_tx, void *rx, unsigned n_rx);
#endif
