# Use stable skill indirection

Each target links by absolute path to a stable, same-named entry under YAASM's data directory, which then links to either an immutable network-source snapshot or a live local directory. Any link in a configured target whose immediate destination is inside the current data directory is managed. This indirection lets updates switch a skill's backing content without rebuilding every target; Unix uses symbolic links and Windows uses directory junctions.
