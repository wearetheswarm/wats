# Fuzzing

cargo-fuzz targets for the parts that read untrusted bytes (ADR 0010):

- `event_bytes`: strict parsing and checking of a wire event. Accepted events
  must re-serialize to an event with the same id that is accepted again.
- `strict_json`: the strict JSON profile and JCS. Canonical output is a fixed
  point.

Run them on nightly. `nightly-2026-09-25` is known to work; the 2026-10-14
nightly crashes while compiling `iroh-blobs` with sanitizers.

```sh
cd fuzz
# Seed corpus: the published test vectors.
mkdir -p corpus/event_bytes && python3 -c '
import json
for f in ["events.json", "events-invalid.json"]:
    for i, c in enumerate(json.load(open("../docs/test-vectors/" + f))["cases"]):
        text = c["json"] if "json" in c else json.dumps(c["event"], separators=(",", ":"))
        open(f"corpus/event_bytes/{f}-{i}", "w").write(text)'
cp -r corpus/event_bytes corpus/strict_json
cargo +nightly-2026-09-25 fuzz run event_bytes corpus/event_bytes -- -max_total_time=300
cargo +nightly-2026-09-25 fuzz run strict_json corpus/strict_json -- -max_total_time=300
```

The fuzz crate is outside the workspace and is not run in CI (Actions
budget). First run (2026-10-15): about 434,000 and 439,000 executions in
2 minutes each, with no crash.
