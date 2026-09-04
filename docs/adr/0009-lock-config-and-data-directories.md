# Lock configuration and data directories

Every command takes non-blocking exclusive OS file locks in both the configuration and data directories, ordered by normalized path. Rust's standard `File::try_lock` API is used with an MSRV of 1.89; persistent lock files carry no ownership state, and closing their file handles on exit or crash releases the kernel locks. This prevents races when separate invocations reuse either directory with different partners.
