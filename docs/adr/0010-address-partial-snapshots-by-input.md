# Address partial snapshots by input

YAASM stores each source-level partial snapshot under a hash derived from the source fingerprint, locked revision, and normalized selected paths. The key is derived at runtime rather than stored in the lockfile. A source update or path-set change creates a new immutable directory, so YAASM can publish content before writing the lock and clean the old snapshot only after deployment succeeds.
