# Building an allowlist

Guessing which hosts a toolchain contacts is tedious. Let Claustrum record them instead:

1. Run a session with `mode = "audit"`: everything is reachable, and everything the
   allowlist would refuse is logged.

   ```toml
   [network]
   mode = "audit"
   ```

2. Let Claude do the usual work: install dependencies, build, run the tests.

3. Summarise the log:

   ```sh
   claustrum network report
   ```

   It groups the log by destination and prints ready-to-paste `allow` entries. `--all`
   includes the destinations that were allowed as well, `--workspace DIR` reads the log of
   another workspace.

4. Review the entries before adding them, the report cannot tell a needed download from an
   unwanted one. Then switch to `mode = "allowlist"` with the entries you kept.

The log is JSON lines in the user state directory, one file per workspace, or wherever
[`[network] log`](../configuration/reference.md#network) points.
