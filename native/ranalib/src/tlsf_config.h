#ifndef RANALIB_TLSF_CONFIG_H
#define RANALIB_TLSF_CONFIG_H

_Noreturn void mrt_abort(void);

#define tlsf_assert(expression) ((expression) ? (void)0 : mrt_abort())

#endif
