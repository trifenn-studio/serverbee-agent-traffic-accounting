# Agent interface traffic accounting

ServerBee originated in [ZingerLittleBee/ServerBee](https://github.com/ZingerLittleBee/ServerBee) and is distributed under the repository's [AGPL license](../LICENSE).

Interface accounting is opt-in on Linux. The Agent samples the selected interfaces in its own network namespace using `/proc/net/dev`, `/proc/sys/kernel/random/boot_id`, `/proc/self/ns/net`, and `/sys/class/net`. Omitting `[collector.traffic]` or setting `enabled = false` uses the existing sysinfo totals and speeds without opening a traffic ledger.

## Policy

Append the policy to the existing Agent TOML and restart the Agent to apply changes. Config precedence is `/etc/serverbee/agent.toml`, the working directory's `agent.toml`, then `SERVERBEE_` environment overrides. See [ENV.md](../ENV.md#interface-traffic-accounting-linux) for every override.

This example uses two placeholder interface names. On each sample it counts billing outbound as `P_TX + R_RX + 2 × R_TX` and billing inbound as zero.

```toml
[collector.traffic]
enabled = true
include = ["P", "R"]
exclude = ["lo"]
strict = true
report = "billing"
state_dir = "/var/lib/serverbee/traffic"
flush_interval_secs = 30

[collector.traffic.weights.P]
rx_to_in_ppm = 0

[collector.traffic.weights.R]
rx_to_in_ppm = 0
rx_to_out_ppm = 1000000
tx_to_out_ppm = 2000000
```

Names are exact and case-sensitive. Exclusion takes precedence over inclusion. An empty include list selects all observed interfaces after exclusion. Use an explicit include list to count only designated interfaces while Docker, veth, or tunnel interfaces change. In strict mode, any missing required interface invalidates the sample. With `strict = false`, interfaces present in a complete sample continue to accrue while an absent interface picks up a fresh baseline when it returns.

Each interface has four integer weights in parts per million, from `0` to `1000000000` (1000×). Omitted weights use RX→inbound and TX→outbound at 1× and both cross-directions at zero. Weights apply to counter increments. Fractional bytes carry forward across samples, checkpoints, and policy changes.

## Native reporting

| Field | Enabled behavior |
| --- | --- |
| `net_in_speed`, `net_out_speed` | Selected RX/TX deltas divided by the elapsed sample time in seconds, clamped to at least one second. Speeds are unweighted bytes per second. |
| `net_in_transfer`, `net_out_transfer` | The last durable pair in bytes. Use `report = "raw"` for selected RX/TX, or `report = "billing"` for the weighted pair. |

The sampler runs every three seconds across connection losses and reconnects. The Agent sends the existing `SystemReport` at the Server's requested interval, and interface inventory continues to use the existing sysinfo path. The Server applies its existing traffic aggregation to the reported pair. Cumulative values advance at each durable checkpoint, so a checkpoint's increment can land in the Server time bucket that receives it. The ledger keeps both pairs for audit. Its billing values estimate traffic from the configured weights.

## Continuity

- Restarting with the same boot, namespace, report mode, and policy recovers increments since the last checkpoint for interfaces with stable identities. The identity check uses the interface name, ifindex, iflink, type, and address. It runs only for selected interfaces.
- Each counter direction handles decreases separately. The decreasing direction starts a new baseline while the other direction can continue accruing.
- Boot, namespace, or policy changes start new baselines. An interface replacement, rename, or disappearance followed by reappearance starts a new baseline for the affected interface. Policy changes preserve existing totals and fractional remainders. Traffic between the last checkpoint and the new baseline can be lost.
- A failed kernel read or strict sample invalidates the baselines, checkpoints already accrued totals if possible, and withholds reports until a valid sample. A stale sample, overflow, or checkpoint failure also withholds reports. The Agent retries failed checkpoints and keeps observed increments in memory until they reach disk.
- A reset can be invisible if the counter passes its old value between observations with the same interface identity. A reboot or an interface replacement can also prevent recovery of increments accrued after the last checkpoint.

## Offline diagnostics

```bash
serverbee-agent traffic-version
serverbee-agent traffic-check --config /path/to/agent.toml
serverbee-agent traffic-status --config /path/to/agent.toml
```

The commands return schema-versioned JSON on stdout and report errors on stderr with a nonzero exit status. They run before enrollment, networking, and upgrade recovery. With `--config`, only the specified file and environment overrides are loaded. The commands can read a policy without a Server URL or token.

| Command | What it reports |
| --- | --- |
| `traffic-version` | Agent version, architecture, and OS. The built-in upgrade probe keeps its existing format. |
| `traffic-check` | Policy and checkpoint settings, missing required names, and observed selected interfaces with their weights, identities, and current counters. It prints the JSON even when a strict required interface is missing. It leaves the state directory untouched. |
| `traffic-status` | The durable checkpoint and whether its policy and report mode match the current config. It can read while the Agent is running. In-memory increments appear after checkpointing. |
| `traffic-seed` | The initial durable checkpoint, including explicit offsets and the selected interfaces' starting baselines. |

## Initialize or migrate a ledger

1. For an existing Agent, stop its process and obtain the last absolute cumulative inbound and outbound byte values accepted by the Server. Preserve its identity and TOML. An Agent's kernel totals or dashboard cycle totals are different from these accepted values.
2. Run `traffic-check` as the Agent user in its network namespace. Verify the selected interfaces and all weights.
3. Seed once using explicit byte offsets. For a new Agent that starts at zero, use the following command. For migration, set the pair designated by `report` to the last accepted pair. Choose and record the other pair's audit starting values.

   ```bash
   serverbee-agent traffic-seed --config /path/to/agent.toml \
     --raw-in 0 --raw-out 0 --billing-in 0 --billing-out 0
   serverbee-agent traffic-status --config /path/to/agent.toml
   ```

4. Start the Agent. Seed records the current kernel baselines and charges only subsequent increments. Keep the same ledger when changing filters or weights or replacing the binary with a build that supports interface accounting.

The Agent requires an existing valid ledger at startup. Seeding refuses to replace an existing one. To switch report modes, stop the Agent, record the last accepted pair, retain the previous ledger, and seed a new directory using that pair for the new mode. Back up the dedicated state directory with the Agent stopped. Restore a verified checkpoint after corruption. If the restored totals precede values already accepted by the Server, an explicit rebase is needed to keep the reported totals continuous.

The directory must be owned by the Agent user with mode `0700` and ledger files with mode `0600`. Use a local filesystem supporting `flock`, atomic rename, and directory sync. The writer locks `traffic.lock` and syncs `traffic-state.next` before replacing `traffic-state.json` and syncing the directory. The versioned checkpoint includes a checksum for corruption detection, current baselines, global totals and fractional remainders, and audit totals by interface name. Seed offsets are global. When fractions carry across names, the rounded per-interface billing totals may sum to less than the rounded global total. History is bounded to 4096 names and the checkpoint to 8 MiB.
