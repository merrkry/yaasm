# Let the lock commit complete an update

A network source resolution is complete when its validated revision is committed to the lockfile, whether resolution was explicitly requested by `update` or required to fill or reconcile the lock during `deploy`. Deployment is a subsequent independent reconciliation phase: failures are reported but do not roll back successful source resolutions. This keeps the lockfile authoritative even though filesystem links cannot participate in the same atomic commit.
