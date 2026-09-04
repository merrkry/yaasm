# Trust configured sources

YAASM treats every configured source as user-trusted content. It prevents its own materialization process from writing outside staging, but does not audit symlinks or other content inside a selected skill; network submodules are unsupported and cause the affected skill or source update to fail rather than being fetched or silently omitted.
