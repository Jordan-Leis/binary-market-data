# Every number in README.md comes from a file under results/ written here.
# Market data inputs are SYNTHETIC (crates/synth). No step uses the network.

export PATH := $(HOME)/.cargo/bin:$(HOME)/.local/bin:$(PATH)
CARGO ?= cargo
REL := target/release

.PHONY: reproduce test build sizes timing broker fuzz sample analyze cost bench-criterion demo check-numbers clean-results

reproduce: test sizes sample analyze fuzz broker timing cost check-numbers
	@echo "reproduce: done. Size and cost results are deterministic; timing and broker results depend on this machine."

build:
	$(CARGO) build --release --locked -p bench -p broker -p gateway

test:
	$(CARGO) test --locked --workspace --release

sizes: build
	$(REL)/sizes results

sample: build
	mkdir -p data/synthetic
	$(REL)/sample data/synthetic/sample.jsonl

analyze: sample
	$(REL)/analyze data/synthetic/sample.jsonl results/analyze_synthetic_sample.json

fuzz: build
	$(REL)/fuzz results 2000000

# Needs redis-server and dragonfly on PATH. A missing server is recorded as untested.
broker: build
	$(REL)/broker results

# Timing: run on a quiet machine. Results are specific to the machine.
timing: build
	$(REL)/timing results

cost: sizes timing
	cd costmodel && uv run --frozen python cost_model.py

check-numbers:
	python3 scripts/check_numbers.py

bench-criterion:
	$(CARGO) bench --locked -p bench

# Live demo: mock core + gateway + a JSON client and a binary client side by side.
demo: build
	bash scripts/demo.sh
