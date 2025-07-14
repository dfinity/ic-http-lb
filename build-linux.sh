#!/bin/bash

cargo build --target=x86_64-unknown-linux-musl --release && x86_64-linux-musl-strip target/x86_64-unknown-linux-musl/release/ic-http-lb && scp target/x86_64-unknown-linux-musl/release/ic-http-lb root@145.40.82.202:
