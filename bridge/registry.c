#include "postgres.h"

#include "nodes/pg_list.h"
#include "utils/memutils.h"

#include "internal.h"

/*
 * The lists behind the node, source and function registries: each entry
 * is a description its provider owns and keeps valid until removal; only
 * the list cells, in TopMemoryContext, belong to the bridge.
 */

const void *
tess_registry_find(List *entries, TessRegistryMatch match, const void *key)
{
	ListCell   *cell;

	foreach(cell, entries)
	{
		if (match(lfirst(cell), key))
			return lfirst(cell);
	}
	return NULL;
}

bool
tess_registry_add(List **entries, const void *entry, TessRegistryMatch match,
				  const void *key)
{
	const void *existing = tess_registry_find(*entries, match, key);
	MemoryContext oldcontext;

	if (existing != NULL)
		return existing == entry;
	oldcontext = MemoryContextSwitchTo(TopMemoryContext);
	*entries = lappend(*entries, (void *) entry);
	MemoryContextSwitchTo(oldcontext);
	return true;
}

void
tess_registry_remove(List **entries, const void *entry)
{
	ListCell   *cell;

	if (entry == NULL)
		return;
	foreach(cell, *entries)
	{
		if (lfirst(cell) == entry)
		{
			*entries = foreach_delete_current(*entries, cell);
			return;
		}
	}
}
