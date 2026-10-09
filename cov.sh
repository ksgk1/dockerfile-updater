#!/usr/bin/env bash
RUSTFLAGS="-Zcoverage-options=branch" cargo +nightly llvm-cov --html --ignore-filename-regex=main.rs -- --include-ignored