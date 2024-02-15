FROM rust:1.69 AS BUILDER

# 1. Create a new empty shell project
RUN USER=root cargo new --bin sshuttle_rust
WORKDIR /sshuttle_rust

# 2. Copy our manifests
COPY ./Cargo.lock ./Cargo.lock
COPY ./Cargo.toml ./Cargo.toml

# 3. Build only the dependencies to cache them
ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
RUN cargo build --release
RUN rm ./src/*.rs

# 4. Now that the dependency is built, copy your source code
COPY ./src ./src


# 5. Build for release.
RUN rm ./target/release/deps/sshuttle_rust*
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get clean \
    && apt-get --allow-unauthenticated update  --allow-insecure-repositories \
    && apt-get --allow-unauthenticated install -y iptables ssh && rm -rf /var/lib/apt/lists/*
COPY --from=builder /sshuttle_rust/target/release/sshuttle_rust /usr/local/bin/sshuttle_rust
COPY ./ssh_config /etc/ssh/ssh_config
CMD ["sshuttle_rust"]
