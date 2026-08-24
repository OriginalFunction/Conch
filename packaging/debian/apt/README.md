# Debian installation

Conch v1 publishes standalone `amd64` and `arm64` Debian packages through the GitHub release. It does not publish or bootstrap an apt repository.

Use the wrapper to download the package and `SHA256SUMS` over HTTPS, verify both GitHub workflow attestations against `OriginalFunction/Conch`, verify the package checksum, and then invoke `apt-get`:

```bash
sudo -E scripts/install-debian.sh --version 1.0.1
```

For an offline or mirrored artifact, download and verify it through your trusted channel first, then run:

```bash
sudo scripts/install-debian.sh --deb conch_1.0.1_amd64.deb --sums SHA256SUMS
```

The local form treats the files as operator-supplied trust roots and verifies the checksum before installation. The wrapper never pipes downloaded code to a shell.
