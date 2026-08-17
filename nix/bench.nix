# The benchmark: its generator, its inputs, and the runners that time the engine.
#
# The important property here is that a benchmark input is a *derivation*. It lives in
# the store, so Nix knows it exists, rebuilds it when it is missing, and `nix store gc`
# can reclaim the gigabyte when nothing references it any more. Writing it into $TMPDIR
# from a shell script — which is what this used to do — put a 1 GiB file somewhere Nix
# could neither guarantee nor collect.
{ pkgs, rust }:

let
  inherit (pkgs) lib;

  # The seeded generator, installed so an input can be built without a cargo checkout.
  generator = rust.rustPlatform.buildRustPackage {
    pname = "transaction-generator";
    version = "0.1.0";
    inherit (rust) src cargoLock;

    cargoBuildFlags = [
      "--example"
      "generate_transactions"
    ];
    doCheck = false;

    installPhase = ''
      runHook preInstall
      install -Dm755 \
        "$(find target -type f -name generate_transactions -perm -u+x | head -n1)" \
        "$out/bin/generate-transactions"
      runHook postInstall
    '';

    meta = {
      description = "Deterministic transaction-log generator for the benchmark";
      mainProgram = "generate-transactions";
    };
  };

  # One benchmark input.
  #
  # Pass `hash` to make it a fixed-output derivation: Nix then verifies the bytes after
  # generating them, which turns "the generator is deterministic" from a claim in the
  # README into something the build enforces. It is pinned for the small input, where
  # regenerating after a deliberate change costs a second, and left off the 1 GiB one so
  # that a change to the generator does not fail the build with a hash mismatch.
  input =
    {
      bytes,
      mix ? "balanced",
      clients ? 65535,
      seed ? 20260816,
      hash ? null,
    }:
    pkgs.runCommand "transactions-${mix}-${bytes}.csv"
      (
        {
          nativeBuildInputs = [ generator ];
        }
        // lib.optionalAttrs (hash != null) {
          outputHashAlgo = "sha256";
          outputHashMode = "flat";
          outputHash = hash;
        }
      )
      ''
        generate-transactions \
          --seed ${toString seed} \
          --bytes ${bytes} \
          --clients ${toString clients} \
          --mix ${mix} > $out
      '';

  large = input { bytes = "1GiB"; };
  settled = input {
    bytes = "1GiB";
    mix = "settled";
  };
  small = input {
    bytes = "16MiB";
    clients = 256;
    hash = "sha256-NKSsciXwnlZkmPtO32cIbjee8chIvoUr9/WRB2MvhEE=";
  };

  # Times the packaged engine — the same binary `nix build` produces, not whatever a
  # local cargo left in ./target — against a store-resident input.
  #
  # Every run appends a record to a history file (./bench/history.jsonl by default,
  # override with BENCH_HISTORY) and reports the delta against the previous run of the
  # same input, so a regression shows up as a number rather than a feeling.
  runner =
    { name, source }:
    pkgs.writeShellApplication {
      inherit name;
      runtimeInputs = with pkgs; [
        time
        coreutils
        gawk
        jq
        git
      ];
      text = ''
        input="${source}"
        history="''${BENCH_HISTORY:-bench/history.jsonl}"

        # Only the measurement artefacts are temporary, and they are cleaned up.
        work="$(mktemp -d)"
        trap 'rm -rf "$work"' EXIT

        input_bytes="$(stat -L -c %s "$input")"
        input_records="$(( $(wc -l < "$input") - 1 ))"

        printf '\n  input     %s, %s records\n' \
          "$(numfmt --to=iec "$input_bytes")" "$input_records"
        printf '  from      %s\n' "$input"

        # %e wall, %U user, %S sys, %P cpu%, %M peak RSS KiB, %F major faults,
        # %R minor faults, %w voluntary switches, %c involuntary switches.
        command time -o "$work/timing" -f '%e %U %S %P %M %F %R %w %c' \
          ${lib.getExe rust.transactionSolver} "$input" \
          > "$work/accounts.csv" 2> "$work/rejected.log"
        read -r wall user sys cpu rss majflt minflt volcsw involcsw < "$work/timing"
        cpu="''${cpu%\%}"

        accounts="$(( $(wc -l < "$work/accounts.csv") - 1 ))"
        rejected="$(wc -l < "$work/rejected.log")"
        output_sha="$(sha256sum "$work/accounts.csv" | cut -d' ' -f1)"

        # The trailing newline matters: `read` returns non-zero without a delimiter, and
        # this script runs under `set -e`.
        read -r rps mbps < <(awk -v b="$input_bytes" -v r="$input_records" -v e="$wall" \
          'BEGIN { if (e <= 0) e = 0.001; printf "%.0f %.1f\n", r / e, b / e / 1000000 }')

        printf '\n  wall      %s s\n' "$wall"
        printf '  cpu       %s s user, %s s sys (%s%% of one core)\n' "$user" "$sys" "$cpu"
        printf '  peak rss  %s\n' "$(numfmt --to=iec "$(( rss * 1024 ))")"
        printf '  faults    %s major, %s minor\n' "$majflt" "$minflt"
        printf '  switches  %s voluntary, %s involuntary\n' "$volcsw" "$involcsw"
        printf '  rate      %s records/s, %s MB/s\n' "$rps" "$mbps"
        printf '\n  accounts  %s\n' "$accounts"
        printf '  rejected  %s\n' "$rejected"
        printf '  output    sha256 %s\n' "''${output_sha:0:32}"

        # Compare against the last run over the same input before recording this one.
        previous=null
        if [[ -s "$history" ]]; then
          previous="$(jq -sc --arg i "$input" \
            '[.[] | select(.input == $i)] | last // null' "$history")"
        fi
        if [[ "$previous" != "null" ]]; then
          read -r prev_wall prev_rss <<< "$(jq -r '"\(.wall_s) \(.peak_rss_kb)"' <<< "$previous")"
          awk -v w="$wall" -v pw="$prev_wall" -v m="$rss" -v pm="$prev_rss" \
            'BEGIN { if (pw > 0 && pm > 0)
                       printf "\n  vs previous  wall %+.1f%%, peak rss %+.1f%%\n",
                              (w - pw) / pw * 100, (m - pm) / pm * 100 }'
        fi

        mkdir -p "$(dirname "$history")"
        jq -nc \
          --arg timestamp "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
          --arg commit "$(git rev-parse --short HEAD 2>/dev/null || echo unknown)" \
          --arg input "$input" \
          --arg output_sha256 "$output_sha" \
          --arg kernel "$(uname -r)" \
          --argjson cpus "$(nproc)" \
          --argjson input_bytes "$input_bytes" \
          --argjson records "$input_records" \
          --argjson wall_s "$wall" \
          --argjson user_s "$user" \
          --argjson sys_s "$sys" \
          --argjson cpu_pct "$cpu" \
          --argjson peak_rss_kb "$rss" \
          --argjson major_faults "$majflt" \
          --argjson minor_faults "$minflt" \
          --argjson voluntary_switches "$volcsw" \
          --argjson involuntary_switches "$involcsw" \
          --argjson records_per_s "$rps" \
          --argjson mb_per_s "$mbps" \
          --argjson accounts "$accounts" \
          --argjson rejected "$rejected" \
          '$ARGS.named' >> "$history"

        printf '  recorded  %s\n' "$history"
      '';
    };

  benchLarge = runner {
    name = "bench";
    source = large;
  };
  benchSmall = runner {
    name = "bench-small";
    source = small;
  };
  benchSettled = runner {
    name = "bench-settled";
    source = settled;
  };
in
{
  packages = {
    bench-generator = generator;
    bench-input = large;
    bench-input-small = small;
    bench-input-settled = settled;
  };

  apps = {
    bench = {
      type = "app";
      program = "${benchLarge}/bin/bench";
    };
    bench-small = {
      type = "app";
      program = "${benchSmall}/bin/bench-small";
    };
    bench-settled = {
      type = "app";
      program = "${benchSettled}/bin/bench-settled";
    };
  };

  # Building the small input verifies its pinned hash, so CI proves the generator is
  # still deterministic for the price of 16 MiB.
  checks.bench-input-reproducible = small;
}
