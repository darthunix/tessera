#include "postgres.h"

#include "fmgr.h"
#include "miscadmin.h"
#include "utils/builtins.h"
#include "utils/injection_point.h"

#include "injection_library.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_interrupt_after);
PG_FUNCTION_INFO_V1(tessera_test_interrupt_probe);
PG_FUNCTION_INFO_V1(tessera_test_interrupt_calls);
PG_FUNCTION_INFO_V1(tessera_test_interrupt_loop);
PG_FUNCTION_INFO_V1(tessera_test_interrupt_turns);

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

/*
 * The loop of a spill whose turns the callback of the injection point
 * "tessera-loop" counts, in backend pid or, for 0, in every process, and
 * whether its first turn marks a cancel pending.
 */
typedef struct LoopWatch
{
	int			pid;
	bool		cancel;
	char		loop[32];
} LoopWatch;

/* The turns of the loop watched since the watch was set. */
static int32 turns = 0;

PGDLLEXPORT void tessera_test_interrupt_turn(const char *name, const void *private_data,
											 void *arg);

/*
 * The callback of "tessera-loop", given the loop's name: its first turn
 * does what the SIGINT handler does when the watch cancels, and a loop
 * that checks for a cancel each turn then takes no second one.
 */
void
tessera_test_interrupt_turn(const char *name, const void *private_data, void *arg)
{
	const LoopWatch *watch = private_data;

	if ((watch->pid != 0 && watch->pid != MyProcPid) || strcmp(arg, watch->loop) != 0)
		return;
	if (++turns == 1 && watch->cancel)
	{
		QueryCancelPending = true;
		InterruptPending = true;
	}
}

/*
 * Watch the loop named loop in this backend from now on, or in every
 * process with everywhere, cancelling the statement in its first turn
 * with cancel; with loop NULL, no longer. A process counts only its own
 * turns. The callback is loaded from module, this module's file without
 * its suffix.
 */
Datum
tessera_test_interrupt_loop(PG_FUNCTION_ARGS)
{
	LoopWatch	watch = {PG_GETARG_BOOL(3) ? 0 : MyProcPid, PG_GETARG_BOOL(2)};

	InjectionPointDetach("tessera-loop");
	turns = 0;
	if (PG_ARGISNULL(1))
		PG_RETURN_VOID();
	strlcpy(watch.loop, text_to_cstring(PG_GETARG_TEXT_PP(1)), sizeof(watch.loop));
	InjectionPointAttach("tessera-loop",
						 module_from_pkglib(text_to_cstring(PG_GETARG_TEXT_PP(0))),
						 "tessera_test_interrupt_turn", &watch, sizeof(watch));
	PG_RETURN_VOID();
}

/* The turns of the loop watched since the watch was set. */
Datum
tessera_test_interrupt_turns(PG_FUNCTION_ARGS)
{
	PG_RETURN_INT32(turns);
}
