# Isolate filesystem roots

YAASM places targets, local skill paths, and its internal repository and snapshot roots into one comparison set, rejecting duplicates and every ancestor-descendant overlap before deployment. Existing paths are canonicalized so symlink aliases cannot hide overlap; missing targets use normalized absolute paths and `..` is forbidden rather than implementing weak canonicalization. This deliberately forbids some technically possible layouts in exchange for preventing ownership ambiguity, recursive scans, and link cycles.
