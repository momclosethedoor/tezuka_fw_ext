#ifndef OBS_STREAM_002_H
#define OBS_STREAM_002_H
#include <stdint.h>
/* Offsets from the Maia CSR base. Check FFM1 independently of spectrum ABI. */
#define OBS_FFM1 UINT32_C(0x314d4646)
#define OBS_FLD2 UINT32_C(0x32444c46)
#define OBS_FFM1_ID_REG 0x350u
#define OBS_FFM1_SESSION 0x354u
#define OBS_FFM1_POP 0x358u
#define OBS_FFM1_LEVEL 0x35cu
#define OBS_FFM1_ASSIGNED 0x360u
#define OBS_FFM1_STARTED 0x364u
#define OBS_FFM1_COMPLETED 0x368u
#define OBS_FFM1_LOST 0x36cu
#define OBS_FFM1_HEAD 0x370u
#define OBS_FFM1_HEAD_VALID (1u<<16)
#define OBS_FFM1_MARK (1u<<5)
#define OBS_FFM1_RECEIPTS 256u
#define OBS_OBN3 UINT32_C(0x334e424f)
#define OBS_SPECTRUM_ID_REG 0x050u
#define OBS_SPECTRUM_FEATURES 0x0e0u
#define OBS_SPECTRUM_REVISION 0x0e4u
#define OBS_OBN3_CONFIG 0x348u
#define OBS_OBN3_ACTIVE 0x34cu
#define OBS_OBN3_RAW8 1u
#define OBS_OBN3_PACKED 2u
#define OBS_OBN3_FULL_STRIDE 32768u
#define OBS_OBN3_VIEW_STRIDE 2048u
#define OBS_OBN3_SLOTS 1024u
#define OBS_OBN3_PACKED_BYTES 24704u
#define OBS_OBN3_PACKED_DATA_OFFSET 128u
/* The 48 bits are little endian; byte 5 has three exponent bits at [7:5].
 * E=0..4, P=M*4^E. Return an error for an impossible exponent or mantissa. */
static inline int obs_obn3_power48(const uint8_t *bin,unsigned k,int peak,uint64_t *out)
{
    if(!bin || !out || !k || k>1023) return -1;
    uint64_t m=0;
    for(unsigned i=0;i<5;++i) m|=(uint64_t)bin[i]<<(8*i);
    m|=(uint64_t)(bin[5]&31u)<<40;
    unsigned e=bin[5]>>5;
    if(e>4 || m>((uint64_t)(peak?1:k)<<35)) return -1;
    *out=m<<(2*e);
    return 0;
}
#endif
