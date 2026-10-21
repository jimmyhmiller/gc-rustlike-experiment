# Runtime-only s390x checks; source is mounted read-only at test time.
FROM rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663
RUN apt-get update \
    && apt-get install -y --no-install-recommends gcc-s390x-linux-gnu libc6-dev-s390x-cross qemu-user \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add s390x-unknown-linux-gnu
