# Clean only after successful reconciliation

YAASM cleans stale managed links in a target only after every configured placement for that target succeeds, because only then does it have a complete and valid deployment map. Data entries and snapshots are cleaned afterward under the same rule; a failed deployment may retain obsolete content, but it must not turn uncertainty into deletion. Targets removed from configuration are outside this process and must be cleaned manually.
