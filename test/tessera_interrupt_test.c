#include "postgres.h"

#include "fmgr.h"
#include "miscadmin.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_interrupt_after);
PG_FUNCTION_INFO_V1(tessera_test_interrupt_probe);
PG_FUNCTION_INFO_V1(tessera_test_interrupt_calls);

/* The probe's call that asks to cancel, 0 for none, and its calls so far. */
static int32 interrupt_at = 0;
static int32 calls = 0;

/* Arm the probe: its n-th call from now on cancels the statement; 0 disarms it. */
Datum
tessera_test_interrupt_after(PG_FUNCTION_ARGS)
{
	interrupt_at = PG_GETARG_INT32(0);
	calls = 0;
	PG_RETURN_VOID();
}

/*
 * A condition never true that, on its armed call, does what the SIGINT
 * handler does: marks a query cancel pending, which the next
 * CHECK_FOR_INTERRUPTS acts on. Neither ExecQual nor a function call
 * checks for interrupts, so only a node's own check ends the statement
 * before its loop returns to the executor.
 */
Datum
tessera_test_interrupt_probe(PG_FUNCTION_ARGS)
{
	if (++calls == interrupt_at)
	{
		QueryCancelPending = true;
		InterruptPending = true;
	}
	PG_RETURN_BOOL(false);
}

/* The probe's calls since it was armed. */
Datum
tessera_test_interrupt_calls(PG_FUNCTION_ARGS)
{
	PG_RETURN_INT32(calls);
}
