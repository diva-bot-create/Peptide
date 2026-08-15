# Peptide task runner. run `just` (no args) to list recipes.
# install just: https://github.com/casey/just  (brew install just)

# corpus location: $SSF2_SSFS_DIR if set, else the default sibling ../ssf2-ssfs
ssfs := env_var_or_default("SSF2_SSFS_DIR", "../ssf2-ssfs")

# list recipes
default:
    @just --list

# build the release binary -> build/release/peptide
build:
    cargo build --release

# run the whole test suite (corpus tests skip cleanly if the corpus is absent)
test:
    cargo test --workspace

# the same clippy gate CI enforces
lint:
    cargo clippy --workspace -- -D warnings

# format the whole codebase (heads up: large one-time diff -- never been run)
fmt:
    cargo fmt

# check formatting without writing (advisory; not yet CI-enforced)
fmt-check:
    cargo fmt -- --check

# convert one character by id, e.g. `just convert mario`
convert CHAR:
    ./build/release/peptide convert "{{ssfs}}/{{CHAR}}.ssf"

# fast inner loop: rebuild + reconvert sandbag
smoke:
    ./tools/rebuild-sandbag.sh

# regression sweep after stage-converter changes: a fixed SAMPLE, chosen for coverage.
#
# Not the whole corpus. Converting all 110 takes hours and writes tens of gigabytes -- one stage
# alone emits 687MB -- so nobody runs it, and a gate nobody runs is not a gate. These ten cover
# the paths a stage change can break: plain geometry, hazards declared in initialize and in
# update, weather, moving platforms, backdrop elements, and boundary/parallax handling. Every one
# is small, so the whole sweep is quick enough to run before every PR.
#
# Use `sweep-stages-all` for the full corpus when a change touches shared art or geometry
# rendering and you want the exhaustive answer.
sample_stages := "battlefield battlefield2 bombfactory butterbuilding bowserscastle crateria crystalsmash flatzoneplus fourside huecomundo"

sweep-stages:
    #!/usr/bin/env bash
    set -u
    fails=0; total=0
    for name in {{sample_stages}}; do
        total=$((total+1))
        if ! ./build/release/peptide ssf2 stage "{{ssfs}}/stages/$name.ssf" --out build/sweep >/dev/null 2>&1; then
            echo "FAIL: $name"; fails=$((fails+1))
        fi
    done
    echo "$((total-fails))/$total sample stages converted clean"
    exit $fails

# the exhaustive version: every corpus stage. slow, and writes a LOT — see sweep-stages.
sweep-stages-all:
    #!/usr/bin/env bash
    set -u
    fails=0; total=0
    for f in {{ssfs}}/stages/*.ssf; do
        total=$((total+1))
        if ! ./build/release/peptide ssf2 stage "$f" --out build/sweep >/dev/null 2>&1; then
            echo "FAIL: $(basename "$f")"; fails=$((fails+1))
        fi
    done
    echo "$((total-fails))/$total stages converted clean"
    exit $fails

# run a dev-tools diagnostic bin, e.g.
#   just dump dump_collision_box {{ssfs}}/mario.ssf a_air_forward
dump BIN *ARGS:
    cargo run -p ssf2_converter --features dev-tools --bin {{BIN}} -- {{ARGS}}

# build the double-clickable macOS app -> build/Peptide.app
app:
    ./tools/make-app.sh
