# Prebuilt release packaging

The v0.4.0 archive ships `install-daemon.sh`, which invokes Cargo and expects
source files absent from the archive. Meanwhile the README directs binary users
to `install-prebuilt.sh`, which exists in the repository but is not packaged.

Package the prebuilt installer instead and point generated release notes at it.
Validate the actual archive contents and installer syntax before publishing.
The check must not install anything or connect to hardware.

Validation: the check rejects the published v0.4.0 archive and accepts a package
containing the prebuilt installer and its runtime dependencies. Bash syntax
checks pass. Rust code is unchanged.
